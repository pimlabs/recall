// Package hooks is what `recall push` and `recall pull` actually do.
package hooks

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"github.com/pimlabs/recall/internal/state"
	"github.com/pimlabs/recall/internal/syncclient"
	"github.com/pimlabs/recall/internal/wire"
)

// Env is everything the hooks need to know about where they're running,
// passed in rather than discovered inside, so the logic is testable without
// a real home directory or a real git repository.
type Env struct {
	MemoryDir  string
	StateFile  string
	ProjectKey string
	SourceEnv  string
	Client     *syncclient.Client
}

// PushResult reports what a push actually did, for logging and tests.
type PushResult struct {
	Pushed  string
	Deleted []string
	Skipped bool
}

// Push handles one PostToolUse invocation.
//
// Two things happen, and only one of them is about the file that triggered
// the hook. The triggering file is pushed if it's a memory file. Separately
// — and regardless of what triggered this run — the memory directory is
// reconciled against the last known baseline, and anything that vanished is
// reported as a delete. That reconciliation is the only mechanism that
// catches deletes at all, since Claude Code has no delete event.
func Push(ctx context.Context, env Env, triggeredPath string) (PushResult, error) {
	var res PushResult

	inMemoryDir := triggeredPath != "" && isUnder(env.MemoryDir, triggeredPath)
	if !inMemoryDir {
		// Not a memory file. Nothing to push, and — importantly — no
		// reconciliation either: this hook fires on every Edit and Write in
		// the session, and doing a directory walk plus a state write on each
		// one would be wasteful. Deletes still propagate on the next edit
		// that does touch a memory file.
		res.Skipped = true
		return res, nil
	}

	prev, hadState, err := state.Load(env.StateFile)
	if err != nil {
		return res, err
	}

	// Reconcile deletes, but never on the very first run for a project:
	// with no baseline, an empty directory would otherwise be read as
	// "everything was deleted" and tombstone the whole project.
	if hadState {
		for _, rel := range prev.Files {
			if _, err := os.Stat(filepath.Join(env.MemoryDir, filepath.FromSlash(rel))); err == nil {
				continue
			}
			req := wire.PushRequest{
				ProjectKey: env.ProjectKey,
				FilePath:   rel,
				Deleted:    true,
				SourceEnv:  env.SourceEnv,
			}
			if _, err := env.Client.Push(ctx, req); err != nil {
				return res, fmt.Errorf("pushing delete of %s: %w", rel, err)
			}
			res.Deleted = append(res.Deleted, rel)
		}
	}

	if info, err := os.Stat(triggeredPath); err == nil && !info.IsDir() {
		rel, err := filepath.Rel(env.MemoryDir, triggeredPath)
		if err != nil {
			return res, err
		}
		rel = filepath.ToSlash(rel)

		// Exact bytes. The shell version used command substitution, which
		// strips every trailing newline, so a file with none or with two
		// came back from a round-trip with exactly one — content silently
		// altered. Reading raw is what prevents that here.
		content, err := os.ReadFile(triggeredPath)
		if err != nil {
			return res, err
		}
		req := wire.PushRequest{
			ProjectKey: env.ProjectKey,
			FilePath:   rel,
			Content:    string(content),
			SourceEnv:  env.SourceEnv,
		}
		if _, err := env.Client.Push(ctx, req); err != nil {
			return res, fmt.Errorf("pushing %s: %w", rel, err)
		}
		res.Pushed = rel
	}

	return res, refreshState(env)
}

// PullResult reports what a pull changed.
type PullResult struct {
	Written []string
	Removed []string
}

// Pull fetches the server's state and makes the local memory directory
// match it, then refreshes the baseline so a machine that only ever pulls
// still has an accurate one.
func Pull(ctx context.Context, env Env) (PullResult, error) {
	var res PullResult

	resp, err := env.Client.Pull(ctx, env.ProjectKey)
	if err != nil {
		return res, err
	}
	if len(resp.Files) == 0 {
		return res, nil
	}

	if err := os.MkdirAll(env.MemoryDir, 0o755); err != nil {
		return res, err
	}

	for _, f := range resp.Files {
		// The server validates this on the way in, and it's validated again
		// here on the way out: this is the moment a bad path would become a
		// write outside the memory directory on this machine.
		if err := wire.ValidateFilePath(f.FilePath); err != nil {
			continue
		}
		dest := filepath.Join(env.MemoryDir, filepath.FromSlash(f.FilePath))

		if f.Deleted {
			if err := os.Remove(dest); err == nil {
				res.Removed = append(res.Removed, f.FilePath)
			} else if !os.IsNotExist(err) {
				return res, err
			}
			continue
		}
		if f.Content == nil {
			continue
		}
		if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
			return res, err
		}
		if err := writeFileAtomic(dest, []byte(*f.Content)); err != nil {
			return res, err
		}
		res.Written = append(res.Written, f.FilePath)
	}

	return res, refreshState(env)
}

func refreshState(env Env) error {
	files, err := state.ListMemoryFiles(env.MemoryDir)
	if err != nil {
		return err
	}
	return state.Save(env.StateFile, files)
}

// writeFileAtomic writes via a temp file and a rename, so a session
// starting while a pull is in flight never reads a half-written memory
// file.
func writeFileAtomic(path string, content []byte) error {
	tmp, err := os.CreateTemp(filepath.Dir(path), ".recall-*.tmp")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())

	if _, err := tmp.Write(content); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	if err := os.Chmod(tmp.Name(), 0o644); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), path)
}

// isUnder reports whether path sits inside dir. Compared segment-wise after
// cleaning, so "/a/memory-notes" is correctly not treated as inside
// "/a/memory" — a plain string prefix check would get that wrong.
func isUnder(dir, path string) bool {
	rel, err := filepath.Rel(filepath.Clean(dir), filepath.Clean(path))
	if err != nil {
		return false
	}
	if rel == "." || rel == ".." {
		return false
	}
	return !strings.HasPrefix(rel, ".."+string(filepath.Separator))
}

// Describe renders a short human-readable summary for the hook's stderr.
func (r PullResult) Describe(w io.Writer, projectKey string) {
	fmt.Fprintf(w, "recall-pull: synced %d memory file(s), removed %d deleted file(s) for %s\n",
		len(r.Written), len(r.Removed), projectKey)
}
