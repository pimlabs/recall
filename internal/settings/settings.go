// Package settings edits a project's own .claude/settings.json — the file
// `recall init` writes and the user then commits.
//
// Because that file is committed and reviewed, edits are made surgically
// with sjson rather than by decoding into a map and re-encoding: Go maps
// have no order, so a round-trip through one would silently re-sort every
// key in the user's file and turn a two-line change into a whole-file diff.
package settings

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/tidwall/gjson"
	"github.com/tidwall/pretty"
	"github.com/tidwall/sjson"
)

const (
	// PushMatcher is matched against the tool name, not the file path —
	// Claude Code offers no path matching, which is exactly why it catches
	// topic files named on the fly. See docs/phase-0-findings.md §4.
	PushMatcher = "Edit|Write"
	// PushCommand and PullCommand resolve through PATH, so a project
	// commits only this reference and never a copy of the implementation.
	PushCommand = "recall push"
	PullCommand = "recall pull"
)

var ErrInvalidJSON = errors.New("settings file exists but is not valid JSON")

type hookEntry struct {
	Type    string `json:"type"`
	Command string `json:"command"`
}

// Wire adds Recall's hooks to the settings JSON in src, returning the new
// document. It is idempotent, and additive: an existing Edit|Write matcher
// belonging to some other tool gets Recall's hook appended to it rather
// than being replaced, and unrelated keys are untouched.
//
// Reports whether anything actually changed, so callers can tell the user
// "already wired" instead of implying work was done.
func Wire(src []byte) (out []byte, changed bool, err error) {
	if len(src) == 0 {
		src = []byte("{}")
	}
	if !gjson.ValidBytes(src) {
		return nil, false, ErrInvalidJSON
	}

	out = src
	pushAdded, err := addMatcherHook(&out, "hooks.PostToolUse", PushMatcher, PushCommand)
	if err != nil {
		return nil, false, err
	}
	pullAdded, err := addSessionStartHook(&out, PullCommand)
	if err != nil {
		return nil, false, err
	}

	changed = pushAdded || pullAdded
	if changed {
		out = pretty.PrettyOptions(out, &pretty.Options{Indent: "  ", SortKeys: false})
	}
	return out, changed, nil
}

// addMatcherHook handles the PostToolUse shape, where entries are keyed by
// a matcher and each holds its own list of hooks.
func addMatcherHook(doc *[]byte, path, matcher, command string) (bool, error) {
	entries := gjson.GetBytes(*doc, path)

	if entries.Exists() && entries.IsArray() {
		for i, entry := range entries.Array() {
			if entry.Get("matcher").String() != matcher {
				continue
			}
			// Matcher already present — append unless our command is there.
			for _, h := range entry.Get("hooks").Array() {
				if h.Get("command").String() == command {
					return false, nil
				}
			}
			target := fmt.Sprintf("%s.%d.hooks.-1", path, i)
			updated, err := sjson.SetBytes(*doc, target, hookEntry{Type: "command", Command: command})
			if err != nil {
				return false, err
			}
			*doc = updated
			return true, nil
		}
	}

	// No such matcher yet: append a whole new entry.
	updated, err := sjson.SetBytes(*doc, path+".-1", map[string]any{
		"matcher": matcher,
		"hooks":   []hookEntry{{Type: "command", Command: command}},
	})
	if err != nil {
		return false, err
	}
	*doc = updated
	return true, nil
}

// addSessionStartHook handles the SessionStart shape, which has no matcher
// — entries are just groups of hooks.
func addSessionStartHook(doc *[]byte, command string) (bool, error) {
	const path = "hooks.SessionStart"
	for _, entry := range gjson.GetBytes(*doc, path).Array() {
		for _, h := range entry.Get("hooks").Array() {
			if h.Get("command").String() == command {
				return false, nil
			}
		}
	}
	updated, err := sjson.SetBytes(*doc, path+".-1", map[string]any{
		"hooks": []hookEntry{{Type: "command", Command: command}},
	})
	if err != nil {
		return false, err
	}
	*doc = updated
	return true, nil
}

// WireFile applies Wire to a settings.json on disk, creating it if absent.
// The write is atomic so an interrupted run can't leave the user with a
// truncated settings file in their repo.
func WireFile(path string) (changed bool, err error) {
	src, err := os.ReadFile(path)
	if err != nil && !os.IsNotExist(err) {
		return false, err
	}

	out, changed, err := Wire(src)
	if err != nil {
		return false, err
	}
	if !changed {
		return false, nil
	}

	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return false, err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".settings-*.json")
	if err != nil {
		return false, err
	}
	defer os.Remove(tmp.Name())

	if _, err := tmp.Write(out); err != nil {
		tmp.Close()
		return false, err
	}
	if err := tmp.Close(); err != nil {
		return false, err
	}
	if err := os.Chmod(tmp.Name(), 0o644); err != nil {
		return false, err
	}
	if err := os.Rename(tmp.Name(), path); err != nil {
		return false, err
	}
	return true, nil
}

// IsWired reports whether a settings document already references Recall's
// push hook — used by `recall status` to answer "is this project opted in".
func IsWired(src []byte) bool {
	if !gjson.ValidBytes(src) {
		return false
	}
	for _, entry := range gjson.GetBytes(src, "hooks.PostToolUse").Array() {
		for _, h := range entry.Get("hooks").Array() {
			if h.Get("command").String() == PushCommand {
				return true
			}
		}
	}
	return false
}
