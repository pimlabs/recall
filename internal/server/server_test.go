package server

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"
	"time"

	"github.com/pimlabs/recall/internal/config"
	"github.com/pimlabs/recall/internal/store"
	"github.com/pimlabs/recall/internal/wire"
)

const testToken = "test-token"

func newTestServer(t *testing.T, tweak func(*config.Server)) (*httptest.Server, *store.Store) {
	t.Helper()
	st, err := store.Open(filepath.Join(t.TempDir(), "recall.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })

	cfg := config.Server{
		Token:           testToken,
		GitCommit:       "testcommit",
		RateLimitWindow: time.Minute,
		RateLimitMax:    1000,
		MergeEnabled:    false,
		ClaudeBin:       "definitely-not-a-real-binary",
	}
	if tweak != nil {
		tweak(&cfg)
	}
	ts := httptest.NewServer(New(cfg, st).Handler())
	t.Cleanup(ts.Close)
	return ts, st
}

func push(t *testing.T, ts *httptest.Server, token string, body wire.PushRequest) *http.Response {
	t.Helper()
	b, err := json.Marshal(body)
	if err != nil {
		t.Fatal(err)
	}
	req, err := http.NewRequest(http.MethodPost, ts.URL+"/sync", bytes.NewReader(b))
	if err != nil {
		t.Fatal(err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	req.Header.Set("Content-Type", "application/json")
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	return resp
}

func get(t *testing.T, ts *httptest.Server, token, path string) (*http.Response, []byte) {
	t.Helper()
	req, err := http.NewRequest(http.MethodGet, ts.URL+path, nil)
	if err != nil {
		t.Fatal(err)
	}
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	resp, err := ts.Client().Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	var buf bytes.Buffer
	buf.ReadFrom(resp.Body)
	return resp, buf.Bytes()
}

func TestAuth(t *testing.T) {
	ts, _ := newTestServer(t, nil)

	resp, _ := get(t, ts, "", "/sync?project_key=acme/app")
	if resp.StatusCode != http.StatusUnauthorized {
		t.Errorf("no token: got %d, want 401", resp.StatusCode)
	}

	resp, _ = get(t, ts, "wrong-token", "/sync?project_key=acme/app")
	if resp.StatusCode != http.StatusUnauthorized {
		t.Errorf("wrong token: got %d, want 401", resp.StatusCode)
	}

	resp, _ = get(t, ts, testToken, "/sync?project_key=acme/app")
	if resp.StatusCode != http.StatusOK {
		t.Errorf("valid token: got %d, want 200", resp.StatusCode)
	}
}

// /health has to stay pollable by uptime tooling that holds no secret.
func TestHealthNeedsNoToken(t *testing.T) {
	ts, _ := newTestServer(t, nil)
	resp, body := get(t, ts, "", "/health")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("got %d, want 200", resp.StatusCode)
	}
	var h wire.Health
	if err := json.Unmarshal(body, &h); err != nil {
		t.Fatal(err)
	}
	if h.Status != "ok" || h.GitCommit != "testcommit" {
		t.Errorf("unexpected health: %+v", h)
	}
}

func TestPushThenPull(t *testing.T) {
	ts, _ := newTestServer(t, nil)

	content := "# Memory\n- a fact\n"
	resp := push(t, ts, testToken, wire.PushRequest{
		ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: content, SourceEnv: "laptop",
	})
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("push: got %d, want 200", resp.StatusCode)
	}

	_, body := get(t, ts, testToken, "/sync?project_key=acme/app")
	var out wire.SyncResponse
	if err := json.Unmarshal(body, &out); err != nil {
		t.Fatal(err)
	}
	if len(out.Files) != 1 {
		t.Fatalf("got %d files, want 1", len(out.Files))
	}
	if out.Files[0].Content == nil || *out.Files[0].Content != content {
		t.Errorf("content round-trip altered: %v", out.Files[0].Content)
	}
}

func TestRejectsTraversalAndAbsolutePaths(t *testing.T) {
	ts, _ := newTestServer(t, nil)
	for _, bad := range []string{"../escape.md", "/etc/passwd", "a/../../b.md"} {
		resp := push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: bad, Content: "x"})
		resp.Body.Close()
		if resp.StatusCode != http.StatusBadRequest {
			t.Errorf("file_path %q: got %d, want 400", bad, resp.StatusCode)
		}
	}
}

// A tombstone keeps its content in the database for recovery, but must not
// hand it back — otherwise a pull would resurrect a deleted file.
func TestTombstoneWithholdsContent(t *testing.T) {
	ts, st := newTestServer(t, nil)

	secret := "# Secret\n- do not resurrect\n"
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "gone.md", Content: secret}).Body.Close()
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "gone.md", Deleted: true}).Body.Close()

	_, body := get(t, ts, testToken, "/sync?project_key=acme/app")
	var out wire.SyncResponse
	json.Unmarshal(body, &out)
	if len(out.Files) != 1 {
		t.Fatalf("got %d files, want 1", len(out.Files))
	}
	if !out.Files[0].Deleted {
		t.Error("file not reported as deleted")
	}
	if out.Files[0].Content != nil {
		t.Errorf("tombstoned content was served: %q", *out.Files[0].Content)
	}
	if bytes.Contains(body, []byte("do not resurrect")) {
		t.Error("tombstoned content leaked into the response body")
	}

	// Still recoverable at the database level, which is the point of a
	// tombstone rather than a delete.
	existing, err := st.Get("acme/app", "gone.md")
	if err != nil {
		t.Fatal(err)
	}
	if existing.Content != secret {
		t.Error("content was not preserved in the database")
	}
}

func TestProjectsAreIsolated(t *testing.T) {
	ts, _ := newTestServer(t, nil)

	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/website", FilePath: "MEMORY.md", Content: "website"}).Body.Close()
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/backend", FilePath: "MEMORY.md", Content: "backend"}).Body.Close()
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/website", FilePath: "MEMORY.md", Deleted: true}).Body.Close()

	_, body := get(t, ts, testToken, "/sync?project_key=acme/backend")
	var out wire.SyncResponse
	json.Unmarshal(body, &out)
	if len(out.Files) != 1 || out.Files[0].Deleted {
		t.Fatalf("deleting in one project affected another: %+v", out.Files)
	}
	if out.Files[0].Content == nil || *out.Files[0].Content != "backend" {
		t.Errorf("content = %v, want backend", out.Files[0].Content)
	}
}

// With no working claude CLI the server must still accept the push and fall
// back to last-write-wins, never reject it.
func TestMergeFailureFallsBackToLastWriteWins(t *testing.T) {
	ts, _ := newTestServer(t, func(c *config.Server) { c.MergeEnabled = true })

	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: "version A"}).Body.Close()
	resp := push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: "version B"})
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("conflicting push rejected: got %d", resp.StatusCode)
	}
	var pr wire.PushResponse
	json.NewDecoder(resp.Body).Decode(&pr)
	if pr.Merged {
		t.Error("reported a merge without a working claude CLI")
	}

	_, body := get(t, ts, testToken, "/sync?project_key=acme/app")
	var out wire.SyncResponse
	json.Unmarshal(body, &out)
	if out.Files[0].Content == nil || *out.Files[0].Content != "version B" {
		t.Errorf("content = %v, want last-write-wins 'version B'", out.Files[0].Content)
	}
}

func TestRateLimit(t *testing.T) {
	ts, _ := newTestServer(t, func(c *config.Server) { c.RateLimitMax = 3 })

	var lastStatus int
	for i := 0; i < 5; i++ {
		resp, _ := get(t, ts, testToken, "/sync?project_key=acme/app")
		lastStatus = resp.StatusCode
	}
	if lastStatus != http.StatusTooManyRequests {
		t.Errorf("got %d after exceeding the limit, want 429", lastStatus)
	}

	// /health is deliberately exempt so uptime polling can't be locked out.
	resp, _ := get(t, ts, "", "/health")
	if resp.StatusCode != http.StatusOK {
		t.Errorf("/health got %d while rate limited, want 200", resp.StatusCode)
	}
}

// Invalid tokens must be counted too, or a flood of them escapes the
// limiter by never reaching the auth check.
func TestRateLimitCountsUnauthorizedRequests(t *testing.T) {
	ts, _ := newTestServer(t, func(c *config.Server) { c.RateLimitMax = 3 })

	var lastStatus int
	for i := 0; i < 5; i++ {
		resp, _ := get(t, ts, "bad-token", "/sync?project_key=acme/app")
		lastStatus = resp.StatusCode
	}
	if lastStatus != http.StatusTooManyRequests {
		t.Errorf("got %d, want 429 — bad tokens must be rate limited too", lastStatus)
	}
}

func TestAdminPageServesWithoutLeakingData(t *testing.T) {
	ts, _ := newTestServer(t, nil)
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: "sensitive note"}).Body.Close()

	resp, body := get(t, ts, "", "/admin")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("got %d, want 200", resp.StatusCode)
	}
	if bytes.Contains(body, []byte("sensitive note")) {
		t.Error("admin page embedded memory content server-side")
	}
	if resp.Header.Get("Content-Security-Policy") == "" {
		t.Error("admin page served without a CSP")
	}

	resp, _ = get(t, ts, "", "/admin/stats")
	if resp.StatusCode != http.StatusUnauthorized {
		t.Errorf("/admin/stats without a token: got %d, want 401", resp.StatusCode)
	}
}

func TestAdminStats(t *testing.T) {
	ts, _ := newTestServer(t, nil)
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: "a", SourceEnv: "laptop"}).Body.Close()
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "gone.md", Content: "b", SourceEnv: "cloud"}).Body.Close()
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "gone.md", Deleted: true, SourceEnv: "cloud"}).Body.Close()

	_, body := get(t, ts, testToken, "/admin/stats")
	var stats wire.AdminStats
	if err := json.Unmarshal(body, &stats); err != nil {
		t.Fatal(err)
	}
	if stats.Totals.ProjectCount != 1 || stats.Totals.FileCount != 1 || stats.Totals.DeletedCount != 1 {
		t.Errorf("totals = %+v", stats.Totals)
	}
	if len(stats.Projects) != 1 || len(stats.Projects[0].Sources) != 2 {
		t.Errorf("projects = %+v", stats.Projects)
	}
}

func TestBackupProducesRestorableSnapshot(t *testing.T) {
	dir := t.TempDir()
	ts, st := newTestServer(t, nil)
	push(t, ts, testToken, wire.PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: "backed up"}).Body.Close()

	dest, err := st.Backup(dir, 7)
	if err != nil {
		t.Fatal(err)
	}

	// The snapshot must be a usable database, not just bytes on disk.
	restored, err := store.Open(dest)
	if err != nil {
		t.Fatal(err)
	}
	defer restored.Close()

	files, err := restored.List("acme/app")
	if err != nil {
		t.Fatal(err)
	}
	if len(files) != 1 || files[0].Content == nil || *files[0].Content != "backed up" {
		t.Errorf("restored snapshot doesn't contain the data: %+v", files)
	}
}

func TestBackupPrunesOldSnapshots(t *testing.T) {
	dir := t.TempDir()
	_, st := newTestServer(t, nil)

	for i := 0; i < 5; i++ {
		if _, err := st.Backup(dir, 2); err != nil {
			t.Fatal(err)
		}
		time.Sleep(2 * time.Millisecond)
	}
	entries, err := filepath.Glob(filepath.Join(dir, "recall-*.db"))
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) > 2 {
		t.Errorf("kept %d snapshots, want at most 2", len(entries))
	}
}

func TestServeShutsDownCleanly(t *testing.T) {
	st, err := store.Open(filepath.Join(t.TempDir(), "recall.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()

	cfg := config.Server{Token: testToken, Addr: "127.0.0.1:0", RateLimitWindow: time.Minute, RateLimitMax: 10}
	ctx, cancel := context.WithCancel(context.Background())
	srv := New(cfg, st)

	done := make(chan error, 1)
	go func() { done <- srv.ListenAndServe(ctx) }()
	time.Sleep(50 * time.Millisecond)
	cancel()

	select {
	case err := <-done:
		if err != nil {
			t.Errorf("shutdown returned %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Error("server did not shut down within 5s")
	}
}
