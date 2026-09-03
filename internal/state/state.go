// Package state keeps the baseline that lets a delete be noticed at all.
//
// Claude Code has no delete event, and a delete via the Bash tool wouldn't
// match the Edit|Write hook even if it did. So the push hook reconciles
// instead: it compares what's on disk now against what was there last time.
// This file is that "last time".
package state

import (
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
)

// State is the on-disk format: the set of memory file paths, relative to
// the memory directory, that were present at the last push or pull.
type State struct {
	Files []string `json:"files"`
}

// Load reads the baseline. The exists flag distinguishes "no baseline yet"
// from "a baseline that happens to be empty" — a distinction that decides
// whether an empty memory directory means "nothing has ever synced" or
// "everything was just deleted". Getting it wrong on a first run would
// tombstone the project's entire history on the server.
func Load(path string) (s State, exists bool, err error) {
	b, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return State{}, false, nil
	}
	if err != nil {
		return State{}, false, err
	}
	if err := json.Unmarshal(b, &s); err != nil {
		// A corrupt baseline is treated as absent rather than fatal: the
		// cost is one missed delete propagation, versus a hook that fails
		// on every edit until someone deletes the file by hand.
		return State{}, false, nil
	}
	return s, true, nil
}

// Save writes the baseline atomically, so two hooks racing can't leave a
// truncated file behind — the shell version wrote it with a plain redirect
// and could.
func Save(path string, files []string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	b, err := json.Marshal(State{Files: files})
	if err != nil {
		return err
	}

	tmp, err := os.CreateTemp(filepath.Dir(path), ".recall-state-*.json")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())

	if _, err := tmp.Write(b); err != nil {
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

// ListMemoryFiles returns the memory files currently on disk, as sorted
// slash-separated paths relative to dir. A missing directory is not an
// error — it just hasn't been created yet.
func ListMemoryFiles(dir string) ([]string, error) {
	var out []string
	err := filepath.WalkDir(dir, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			if os.IsNotExist(err) {
				return nil
			}
			return err
		}
		if d.IsDir() {
			return nil
		}
		rel, err := filepath.Rel(dir, p)
		if err != nil {
			return err
		}
		out = append(out, filepath.ToSlash(rel))
		return nil
	})
	if err != nil && !os.IsNotExist(err) {
		return nil, err
	}
	sort.Strings(out)
	return out, nil
}
