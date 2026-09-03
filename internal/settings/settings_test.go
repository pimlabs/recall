package settings

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tidwall/gjson"
)

func countCommand(t *testing.T, doc []byte, command string) int {
	t.Helper()
	n := 0
	var walk func(gjson.Result)
	walk = func(r gjson.Result) {
		if r.IsObject() {
			if r.Get("command").String() == command {
				n++
			}
			r.ForEach(func(_, v gjson.Result) bool { walk(v); return true })
		} else if r.IsArray() {
			r.ForEach(func(_, v gjson.Result) bool { walk(v); return true })
		}
	}
	walk(gjson.ParseBytes(doc))
	return n
}

func TestWireFromNothing(t *testing.T) {
	out, changed, err := Wire(nil)
	if err != nil {
		t.Fatal(err)
	}
	if !changed {
		t.Fatal("expected a change when starting from an empty document")
	}
	if got := countCommand(t, out, PushCommand); got != 1 {
		t.Errorf("push hook count = %d, want 1", got)
	}
	if got := countCommand(t, out, PullCommand); got != 1 {
		t.Errorf("pull hook count = %d, want 1", got)
	}
	if m := gjson.GetBytes(out, "hooks.PostToolUse.0.matcher").String(); m != PushMatcher {
		t.Errorf("matcher = %q, want %q", m, PushMatcher)
	}
	if !json.Valid(out) {
		t.Error("output is not valid JSON")
	}
}

func TestWireIsIdempotent(t *testing.T) {
	first, _, err := Wire(nil)
	if err != nil {
		t.Fatal(err)
	}
	second, changed, err := Wire(first)
	if err != nil {
		t.Fatal(err)
	}
	if changed {
		t.Error("second run reported a change; wiring must be idempotent")
	}
	if got := countCommand(t, second, PushCommand); got != 1 {
		t.Errorf("push hook duplicated: count = %d", got)
	}
	if got := countCommand(t, second, PullCommand); got != 1 {
		t.Errorf("pull hook duplicated: count = %d", got)
	}
}

// The file belongs to the user's project and may already carry another
// tool's hooks. Recall must add itself alongside, never replace.
func TestWirePreservesOtherToolsAndUnrelatedKeys(t *testing.T) {
	src := []byte(`{
  "permissions": { "allow": ["Bash(npm test)"] },
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "prettier --write" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "audit-log" }] }
    ],
    "SessionStart": [ { "hooks": [{ "type": "command", "command": "some-other-tool" }] } ]
  }
}`)

	out, changed, err := Wire(src)
	if err != nil {
		t.Fatal(err)
	}
	if !changed {
		t.Fatal("expected a change")
	}

	if got := gjson.GetBytes(out, "permissions.allow.0").String(); got != "Bash(npm test)" {
		t.Errorf("unrelated key lost: permissions.allow.0 = %q", got)
	}
	for _, cmd := range []string{"prettier --write", "audit-log", "some-other-tool"} {
		if got := countCommand(t, out, cmd); got != 1 {
			t.Errorf("pre-existing hook %q count = %d, want 1", cmd, got)
		}
	}
	// Appended to the existing Edit|Write entry rather than creating a
	// second entry with the same matcher.
	matchers := gjson.GetBytes(out, `hooks.PostToolUse.#(matcher=="Edit|Write")#`).Array()
	if len(matchers) != 1 {
		t.Errorf("Edit|Write matcher entries = %d, want 1", len(matchers))
	}
	if got := countCommand(t, out, PushCommand); got != 1 {
		t.Errorf("push hook count = %d, want 1", got)
	}
}

// Go maps lose ordering, which would silently re-sort a file the user
// commits and reviews. sjson edits in place, so this must hold.
func TestWirePreservesKeyOrder(t *testing.T) {
	src := []byte(`{"zebra": 1, "alpha": 2, "middle": 3}`)
	out, _, err := Wire(src)
	if err != nil {
		t.Fatal(err)
	}
	s := string(out)
	iZebra, iAlpha, iMiddle := strings.Index(s, "zebra"), strings.Index(s, "alpha"), strings.Index(s, "middle")
	if !(iZebra < iAlpha && iAlpha < iMiddle) {
		t.Errorf("key order not preserved:\n%s", s)
	}
}

func TestWireRejectsInvalidJSON(t *testing.T) {
	if _, _, err := Wire([]byte("{not json")); err == nil {
		t.Error("expected an error for a malformed settings file")
	}
}

func TestWireFileCreatesAndIsAtomic(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, ".claude", "settings.json")

	changed, err := WireFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !changed {
		t.Error("expected the first WireFile to report a change")
	}

	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !IsWired(b) {
		t.Error("written file is not detected as wired")
	}

	changed, err = WireFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if changed {
		t.Error("second WireFile reported a change")
	}

	// No temp files left behind next to the real one.
	entries, err := os.ReadDir(filepath.Dir(path))
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range entries {
		if strings.HasPrefix(e.Name(), ".settings-") {
			t.Errorf("temp file left behind: %s", e.Name())
		}
	}
}

func TestIsWired(t *testing.T) {
	if IsWired([]byte(`{}`)) {
		t.Error("empty settings reported as wired")
	}
	if IsWired([]byte(`{not json`)) {
		t.Error("invalid settings reported as wired")
	}
	out, _, _ := Wire(nil)
	if !IsWired(out) {
		t.Error("freshly wired settings not reported as wired")
	}
}
