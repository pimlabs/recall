package hooks

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"testing"

	"github.com/pimlabs/recall/internal/state"
	"github.com/pimlabs/recall/internal/syncclient"
	"github.com/pimlabs/recall/internal/wire"
)

// fakeServer stands in for the Recall server, recording pushes and serving
// whatever files it's told to.
type fakeServer struct {
	*httptest.Server
	pushes []wire.PushRequest
	files  []wire.File
}

func newFakeServer(t *testing.T) *fakeServer {
	t.Helper()
	fs := &fakeServer{}
	mux := http.NewServeMux()
	mux.HandleFunc("/sync", func(w http.ResponseWriter, r *http.Request) {
		if r.Method == http.MethodPost {
			var req wire.PushRequest
			if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
				http.Error(w, err.Error(), http.StatusBadRequest)
				return
			}
			fs.pushes = append(fs.pushes, req)
			json.NewEncoder(w).Encode(wire.PushResponse{OK: true})
			return
		}
		json.NewEncoder(w).Encode(wire.SyncResponse{
			ProjectKey: r.URL.Query().Get("project_key"),
			Files:      fs.files,
		})
	})
	fs.Server = httptest.NewServer(mux)
	t.Cleanup(fs.Close)
	return fs
}

func testEnv(t *testing.T, fs *fakeServer) Env {
	t.Helper()
	dir := t.TempDir()
	return Env{
		MemoryDir:  filepath.Join(dir, "memory"),
		StateFile:  filepath.Join(dir, ".recall-state.json"),
		ProjectKey: "acme/app",
		SourceEnv:  "test",
		Client:     syncclient.New(fs.URL, "token"),
	}
}

func write(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestPushIgnoresFilesOutsideTheMemoryDir(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)

	other := filepath.Join(t.TempDir(), "src", "main.go")
	write(t, other, "package main")

	res, err := Push(context.Background(), env, other)
	if err != nil {
		t.Fatal(err)
	}
	if !res.Skipped {
		t.Error("expected a non-memory file to be skipped")
	}
	if len(fs.pushes) != 0 {
		t.Errorf("pushed %d requests for a non-memory file", len(fs.pushes))
	}
	// A skipped run must not write a baseline either — doing so on an
	// unrelated edit would record an empty directory as the truth.
	if _, err := os.Stat(env.StateFile); !os.IsNotExist(err) {
		t.Error("state file written for a skipped push")
	}
}

// "/a/memory-notes" must not count as inside "/a/memory".
func TestPushIgnoresSiblingDirectoryWithSharedPrefix(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)

	sneaky := env.MemoryDir + "-notes/NOTES.md"
	write(t, sneaky, "not memory")

	res, err := Push(context.Background(), env, sneaky)
	if err != nil {
		t.Fatal(err)
	}
	if !res.Skipped {
		t.Error("a sibling directory sharing a name prefix was treated as inside the memory dir")
	}
}

func TestPushSendsExactBytes(t *testing.T) {
	cases := map[string]string{
		"no trailing newline":       "# Memory\n- fact",
		"one trailing newline":      "# Memory\n- fact\n",
		"two trailing newlines":     "# Memory\n- fact\n\n",
		"empty file":                "",
		"crlf line endings":         "# Memory\r\n- fact\r\n",
		"trailing spaces preserved": "# Memory\n- fact   \n",
	}

	for name, content := range cases {
		t.Run(name, func(t *testing.T) {
			fs := newFakeServer(t)
			env := testEnv(t, fs)
			path := filepath.Join(env.MemoryDir, "MEMORY.md")
			write(t, path, content)

			if _, err := Push(context.Background(), env, path); err != nil {
				t.Fatal(err)
			}
			if len(fs.pushes) != 1 {
				t.Fatalf("got %d pushes, want 1", len(fs.pushes))
			}
			if got := fs.pushes[0].Content; got != content {
				t.Errorf("content round-trip altered:\n got %q\nwant %q", got, content)
			}
		})
	}
}

// A file Claude names on the fly must be pushed exactly like MEMORY.md —
// there is no list of known filenames anywhere in this path.
func TestPushCatchesDynamicallyNamedTopicFile(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)
	path := filepath.Join(env.MemoryDir, "debugging.md")
	write(t, path, "# Debugging\n")

	res, err := Push(context.Background(), env, path)
	if err != nil {
		t.Fatal(err)
	}
	if res.Pushed != "debugging.md" {
		t.Errorf("pushed %q, want debugging.md", res.Pushed)
	}
}

func TestPushSendsNestedPathWithForwardSlashes(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)
	path := filepath.Join(env.MemoryDir, "topics", "auth.md")
	write(t, path, "# Auth\n")

	if _, err := Push(context.Background(), env, path); err != nil {
		t.Fatal(err)
	}
	if got := fs.pushes[0].FilePath; got != "topics/auth.md" {
		t.Errorf("file_path = %q, want topics/auth.md", got)
	}
}

// The single most dangerous failure mode: on a first run there is no
// baseline, and an empty or partial memory dir must not be read as
// "everything was deleted".
func TestPushDoesNotTombstoneEverythingOnFirstRun(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)
	path := filepath.Join(env.MemoryDir, "MEMORY.md")
	write(t, path, "# Memory\n")

	res, err := Push(context.Background(), env, path)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Deleted) != 0 {
		t.Errorf("first run reported deletes: %v", res.Deleted)
	}
	for _, p := range fs.pushes {
		if p.Deleted {
			t.Errorf("first run sent a tombstone for %q", p.FilePath)
		}
	}
}

func TestPushReconcilesDeletes(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)

	memory := filepath.Join(env.MemoryDir, "MEMORY.md")
	gone := filepath.Join(env.MemoryDir, "gone.md")
	write(t, memory, "# Memory\n")
	write(t, gone, "# Gone\n")

	// Establish a baseline containing both.
	if _, err := Push(context.Background(), env, memory); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(gone); err != nil {
		t.Fatal(err)
	}

	// A later edit to an unrelated memory file is what carries the delete.
	res, err := Push(context.Background(), env, memory)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Deleted) != 1 || res.Deleted[0] != "gone.md" {
		t.Fatalf("deletes = %v, want [gone.md]", res.Deleted)
	}

	var tombstoned bool
	for _, p := range fs.pushes {
		if p.Deleted && p.FilePath == "gone.md" {
			tombstoned = true
		}
	}
	if !tombstoned {
		t.Error("no tombstone push was sent for the deleted file")
	}

	// And the baseline no longer lists it, so it isn't re-reported forever.
	s, exists, err := state.Load(env.StateFile)
	if err != nil || !exists {
		t.Fatalf("state missing after push: exists=%v err=%v", exists, err)
	}
	for _, f := range s.Files {
		if f == "gone.md" {
			t.Error("deleted file still listed in the refreshed baseline")
		}
	}
}

func TestPullWritesFilesAndRemovesTombstoned(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)

	content := "# Memory\n- from the server\n"
	stale := filepath.Join(env.MemoryDir, "stale.md")
	write(t, stale, "should be removed")

	fs.files = []wire.File{
		{FilePath: "MEMORY.md", Content: &content},
		{FilePath: "stale.md", Content: nil, Deleted: true},
	}

	res, err := Pull(context.Background(), env)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Written) != 1 || res.Written[0] != "MEMORY.md" {
		t.Errorf("written = %v, want [MEMORY.md]", res.Written)
	}
	if len(res.Removed) != 1 || res.Removed[0] != "stale.md" {
		t.Errorf("removed = %v, want [stale.md]", res.Removed)
	}

	got, err := os.ReadFile(filepath.Join(env.MemoryDir, "MEMORY.md"))
	if err != nil {
		t.Fatal(err)
	}
	if string(got) != content {
		t.Errorf("pulled content = %q, want %q", got, content)
	}
	if _, err := os.Stat(stale); !os.IsNotExist(err) {
		t.Error("tombstoned file still present after pull")
	}
}

// A malicious or buggy server must not be able to make a pull write outside
// the memory directory.
func TestPullRefusesTraversalPaths(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)

	evil := "pwned"
	fs.files = []wire.File{
		{FilePath: "../../escaped.md", Content: &evil},
		{FilePath: "/etc/passwd", Content: &evil},
	}

	res, err := Pull(context.Background(), env)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Written) != 0 {
		t.Errorf("wrote %v; traversal paths must be refused", res.Written)
	}
	outside := filepath.Join(filepath.Dir(filepath.Dir(env.MemoryDir)), "escaped.md")
	if _, err := os.Stat(outside); !os.IsNotExist(err) {
		t.Error("a file was written outside the memory directory")
	}
}

func TestPullRoundTripsBytesExactly(t *testing.T) {
	for _, content := range []string{"", "no newline", "one\n", "two\n\n", "\n\n\n"} {
		fs := newFakeServer(t)
		env := testEnv(t, fs)
		c := content
		fs.files = []wire.File{{FilePath: "MEMORY.md", Content: &c}}

		if _, err := Pull(context.Background(), env); err != nil {
			t.Fatal(err)
		}
		got, err := os.ReadFile(filepath.Join(env.MemoryDir, "MEMORY.md"))
		if err != nil {
			t.Fatal(err)
		}
		if string(got) != content {
			t.Errorf("pull altered content: got %q, want %q", got, content)
		}
	}
}

// A machine that only ever pulls still needs an accurate baseline, or its
// first local delete would go unnoticed.
func TestPullRefreshesTheBaseline(t *testing.T) {
	fs := newFakeServer(t)
	env := testEnv(t, fs)
	c := "# Memory\n"
	fs.files = []wire.File{{FilePath: "MEMORY.md", Content: &c}}

	if _, err := Pull(context.Background(), env); err != nil {
		t.Fatal(err)
	}
	s, exists, err := state.Load(env.StateFile)
	if err != nil || !exists {
		t.Fatalf("no baseline after pull: exists=%v err=%v", exists, err)
	}
	sort.Strings(s.Files)
	if len(s.Files) != 1 || s.Files[0] != "MEMORY.md" {
		t.Errorf("baseline = %v, want [MEMORY.md]", s.Files)
	}
}
