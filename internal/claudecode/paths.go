// Package claudecode holds facts about Claude Code itself — where it keeps
// auto-memory, and how it names the per-project directory.
//
// These are reverse-engineered from the installed CLI (v2.1.42), not a
// published contract; see docs/phase-0-findings.md. They live in their own
// package precisely because they track someone else's implementation: when
// Claude Code changes, there is exactly one place to fix, with its own
// tests, instead of the assumption being smeared across the codebase.
package claudecode

import (
	"path/filepath"
	"strings"
	"unicode/utf16"
)

// Env is the subset of the environment these paths depend on. Passed in
// rather than read from os.Getenv inside, so the derivation is testable
// without mutating process state.
type Env struct {
	// RemoteMemoryDir is CLAUDE_CODE_REMOTE_MEMORY_DIR. When set it wins
	// outright — and note that merely having it set is also what enables
	// auto-memory at all in a remote session (findings §5).
	RemoteMemoryDir string
	// ConfigDir is CLAUDE_CONFIG_DIR.
	ConfigDir string
	// Home is $HOME.
	Home string
}

// MemoryRoot resolves the root Claude Code keeps per-project data under,
// in the same precedence order the CLI uses:
//
//	CLAUDE_CODE_REMOTE_MEMORY_DIR > CLAUDE_CONFIG_DIR > $HOME/.claude
func (e Env) MemoryRoot() string {
	if e.RemoteMemoryDir != "" {
		return e.RemoteMemoryDir
	}
	if e.ConfigDir != "" {
		return e.ConfigDir
	}
	return filepath.Join(e.Home, ".claude")
}

// Slug reproduces Claude Code's own project-directory naming, which in the
// CLI is the JavaScript expression:
//
//	path.replace(/[^a-zA-Z0-9]/g, "-")
//
// The subtlety worth stating: a JavaScript regex replace operates on UTF-16
// code units, so "é" (one code unit) becomes one dash while "🚀" (a
// surrogate pair, two code units) becomes two. Iterating bytes or runes
// instead would diverge for any non-ASCII path — and the shell
// implementation this replaces did exactly that, byte-wise and
// locale-dependent, so the same project could resolve to different
// directories on two machines and silently sync nothing. Encoding to UTF-16
// first is what makes this match the real thing.
func Slug(path string) string {
	units := utf16.Encode([]rune(path))
	var b strings.Builder
	b.Grow(len(units))
	for _, u := range units {
		switch {
		case u >= 'a' && u <= 'z', u >= 'A' && u <= 'Z', u >= '0' && u <= '9':
			// Every retained unit is ASCII by construction.
			b.WriteByte(byte(u))
		default:
			b.WriteByte('-')
		}
	}
	return b.String()
}

// MemoryDir is where Claude Code reads and writes auto-memory for the
// project rooted at projectRoot: <root>/projects/<slug>/memory
func (e Env) MemoryDir(projectRoot string) string {
	return filepath.Join(e.MemoryRoot(), "projects", Slug(projectRoot), "memory")
}

// StateFile is Recall's own bookkeeping for delete reconciliation. It sits
// *beside* the memory directory rather than inside it, so it can never be
// mistaken for a memory file and pushed to the server.
func (e Env) StateFile(projectRoot string) string {
	return filepath.Join(e.MemoryRoot(), "projects", Slug(projectRoot), ".recall-state.json")
}
