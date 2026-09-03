package wire

import (
	"encoding/json"
	"errors"
	"testing"
)

func TestValidateFilePath(t *testing.T) {
	tests := []struct {
		name string
		in   string
		want error
	}{
		{"plain file", "MEMORY.md", nil},
		{"dynamically named topic file", "debugging.md", nil},
		{"nested path", "topics/auth/tokens.md", nil},
		{"dot-prefixed name is fine", ".hidden.md", nil},
		{
			// The Node server this replaces used a substring check for
			// "..", which rejected this legitimate filename. Segment-wise
			// checking is deliberately more permissive here.
			name: "leading dots in a filename are not traversal",
			in:   "..config.md",
			want: nil,
		},
		{"empty", "", ErrMissingFilePath},
		{"absolute posix", "/etc/passwd", ErrFilePathAbsolute},
		{"absolute windows drive", "C:/Windows/system32", ErrFilePathAbsolute},
		{"backslash root", `\etc\passwd`, ErrFilePathAbsolute},
		{"traversal at the front", "../outside.md", ErrFilePathTraversal},
		{"traversal in the middle", "topics/../../outside.md", ErrFilePathTraversal},
		{"traversal with backslashes", `topics\..\..\outside.md`, ErrFilePathTraversal},
		{"bare traversal", "..", ErrFilePathTraversal},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := ValidateFilePath(tt.in)
			if !errors.Is(got, tt.want) {
				t.Errorf("ValidateFilePath(%q) = %v, want %v", tt.in, got, tt.want)
			}
		})
	}
}

func TestPushRequestValidate(t *testing.T) {
	valid := PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Content: "hi"}
	if err := valid.Validate(); err != nil {
		t.Errorf("valid push rejected: %v", err)
	}

	if err := (PushRequest{FilePath: "MEMORY.md"}).Validate(); !errors.Is(err, ErrMissingProjectKey) {
		t.Errorf("missing project_key: got %v", err)
	}

	// A delete carries no content, and that must not be treated as invalid.
	del := PushRequest{ProjectKey: "acme/app", FilePath: "MEMORY.md", Deleted: true}
	if err := del.Validate(); err != nil {
		t.Errorf("delete push rejected: %v", err)
	}
}

// The Go server has to be a drop-in for the Node one it replaces: a client
// on the old shell hooks and one on the new binary talk to the same
// deployment during migration. These assert the exact JSON key names.
func TestWireFormatMatchesTheNodeServer(t *testing.T) {
	b, err := json.Marshal(PushRequest{
		ProjectKey: "acme/app",
		FilePath:   "MEMORY.md",
		Content:    "hello",
		SourceEnv:  "laptop",
	})
	if err != nil {
		t.Fatal(err)
	}
	const want = `{"project_key":"acme/app","file_path":"MEMORY.md","content":"hello","source_env":"laptop"}`
	if string(b) != want {
		t.Errorf("push body =\n  %s\nwant\n  %s", b, want)
	}

	b, err = json.Marshal(PushRequest{ProjectKey: "acme/app", FilePath: "gone.md", Deleted: true, SourceEnv: "laptop"})
	if err != nil {
		t.Fatal(err)
	}
	const wantDelete = `{"project_key":"acme/app","file_path":"gone.md","source_env":"laptop","deleted":true}`
	if string(b) != wantDelete {
		t.Errorf("delete body =\n  %s\nwant\n  %s", b, wantDelete)
	}
}

// A tombstoned file must serialize content as null, not "" — an empty
// string is a legitimate empty memory file and the two cannot be conflated.
func TestTombstonedFileSerializesContentAsNull(t *testing.T) {
	b, err := json.Marshal(SyncResponse{
		ProjectKey: "acme/app",
		Files: []File{{
			FilePath: "gone.md", Content: nil, SourceEnv: "laptop",
			UpdatedAt: "2026-01-01T00:00:00.000Z", Deleted: true,
		}},
	})
	if err != nil {
		t.Fatal(err)
	}
	const want = `{"project_key":"acme/app","files":[{"file_path":"gone.md","content":null,"source_env":"laptop","updated_at":"2026-01-01T00:00:00.000Z","deleted":true}]}`
	if string(b) != want {
		t.Errorf("sync body =\n  %s\nwant\n  %s", b, want)
	}

	empty := ""
	b, _ = json.Marshal(File{FilePath: "empty.md", Content: &empty})
	if string(b) == `{"file_path":"empty.md","content":null,"source_env":"","updated_at":"","deleted":false}` {
		t.Error("an empty file serialized as null; it must be distinguishable from a tombstone")
	}
}
