package claudecode

import (
	"path/filepath"
	"testing"
)

// The expected values here are what the real Claude Code CLI produces —
// its JS is `path.replace(/[^a-zA-Z0-9]/g, "-")`, which replaces per UTF-16
// code unit. The non-ASCII rows are the ones the previous shell
// implementation got wrong (byte-wise, and differently depending on
// LC_ALL), which meant a wrong memory directory and silently dead sync.
func TestSlug(t *testing.T) {
	tests := []struct {
		name string
		in   string
		want string
	}{
		{
			name: "real project path, verified against ~/.claude/projects on disk",
			in:   "/home/user/recall",
			want: "-home-user-recall",
		},
		{
			name: "macOS-style home path",
			in:   "/Users/eko/code/recall",
			want: "-Users-eko-code-recall",
		},
		{
			name: "case is preserved, only non-alphanumerics are replaced",
			in:   "/Users/Eko/MyRepo",
			want: "-Users-Eko-MyRepo",
		},
		{
			name: "spaces and dots each become one dash",
			in:   "/tmp/my project.v2",
			want: "-tmp-my-project-v2",
		},
		{
			name: "pipe character — broke the shell version's sed outright",
			in:   "/home/weird|dir",
			want: "-home-weird-dir",
		},
		{
			name: "latin-1 accent is one UTF-16 unit, so one dash",
			in:   "/home/café",
			want: "-home-caf-",
		},
		{
			name: "astral char is a surrogate pair, so two dashes",
			in:   "/home/🚀app",
			want: "-home---app",
		},
		{
			name: "empty stays empty",
			in:   "",
			want: "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := Slug(tt.in); got != tt.want {
				t.Errorf("Slug(%q) = %q, want %q", tt.in, got, tt.want)
			}
		})
	}
}

func TestMemoryRootPrecedence(t *testing.T) {
	tests := []struct {
		name string
		env  Env
		want string
	}{
		{
			name: "remote memory dir wins over everything",
			env:  Env{RemoteMemoryDir: "/data/claude", ConfigDir: "/cfg", Home: "/home/u"},
			want: "/data/claude",
		},
		{
			name: "config dir wins over home",
			env:  Env{ConfigDir: "/cfg", Home: "/home/u"},
			want: "/cfg",
		},
		{
			name: "falls back to ~/.claude",
			env:  Env{Home: "/home/u"},
			want: "/home/u/.claude",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := tt.env.MemoryRoot(); got != tt.want {
				t.Errorf("MemoryRoot() = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestMemoryDirAndStateFile(t *testing.T) {
	env := Env{Home: "/home/user"}
	root := "/home/user/recall"

	wantMem := filepath.FromSlash("/home/user/.claude/projects/-home-user-recall/memory")
	if got := env.MemoryDir(root); got != wantMem {
		t.Errorf("MemoryDir() = %q, want %q", got, wantMem)
	}

	// Beside the memory dir, never inside it — otherwise the push hook would
	// try to sync its own bookkeeping file as if it were a memory note.
	wantState := filepath.FromSlash("/home/user/.claude/projects/-home-user-recall/.recall-state.json")
	if got := env.StateFile(root); got != wantState {
		t.Errorf("StateFile() = %q, want %q", got, wantState)
	}
	if dir := filepath.Dir(env.StateFile(root)); dir == env.MemoryDir(root) {
		t.Error("state file is inside the memory dir; it must be a sibling")
	}
}
