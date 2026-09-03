// Package hookio parses what Claude Code feeds a hook on stdin, and
// defines how a hook is allowed to fail.
//
// The failure policy matters as much as the parsing: these run inside
// someone's session. A push hook that errors noisily on every unrelated
// edit, or a pull hook that stops a session from starting because the
// server is down, is worse than one that does nothing.
package hookio

import (
	"encoding/json"
	"io"
)

// PostToolUse is the payload Claude Code sends after a tool runs. Only the
// fields Recall acts on are declared; the real payload carries more
// (session_id, transcript_path, tool_response, …) and is free to grow.
type PostToolUse struct {
	HookEventName string `json:"hook_event_name"`
	ToolName      string `json:"tool_name"`
	ToolInput     struct {
		FilePath string `json:"file_path"`
	} `json:"tool_input"`
}

// ParsePostToolUse reads a hook payload.
//
// A malformed or empty payload yields a zero value and no error: the hook
// runs on every Edit and Write in the session, so anything unrecognized
// should be quietly ignored rather than turned into noise the user can't
// act on. Callers treat an empty FilePath as "nothing to do".
func ParsePostToolUse(r io.Reader) PostToolUse {
	var p PostToolUse
	b, err := io.ReadAll(io.LimitReader(r, 8<<20))
	if err != nil || len(b) == 0 {
		return PostToolUse{}
	}
	if err := json.Unmarshal(b, &p); err != nil {
		return PostToolUse{}
	}
	return p
}

// Exit codes, which are part of the contract with Claude Code.
const (
	// ExitOK covers both success and a deliberate no-op — an edit to a file
	// that isn't a memory file, or a pull that couldn't reach the server.
	ExitOK = 0
	// ExitConfig is a misconfiguration only the user can fix: no token, not
	// a git repository. Worth surfacing.
	ExitConfig = 1
	// ExitServer is the server rejecting a request.
	ExitServer = 2
)
