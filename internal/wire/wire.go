// Package wire is the contract between the client and the server: request
// and response shapes, and the validation rules that apply to both.
//
// This package is the concrete payoff of putting both halves in one module.
// Before the rewrite these rules existed twice — the server rejected
// traversal in `file_path` in JavaScript while the client built those same
// paths in bash — with nothing keeping the two in agreement, so a drift
// between them would only surface in production. Here there is one
// definition and one set of tests covering both sides.
//
// The JSON shapes below must stay byte-compatible with the Node server they
// replace: during migration a laptop still on the shell hooks and a session
// already on Go talk to the same deployment.
package wire

import (
	"errors"
	"path"
	"strings"
)

// PushRequest is the body of POST /sync.
//
// Content is omitted for a delete, which is why it can't be `omitempty` on
// its own terms — an empty file is a legitimate push, and must not be
// mistaken for a tombstone. Deleted carries that distinction explicitly.
type PushRequest struct {
	ProjectKey string `json:"project_key"`
	FilePath   string `json:"file_path"`
	Content    string `json:"content,omitempty"`
	SourceEnv  string `json:"source_env,omitempty"`
	Deleted    bool   `json:"deleted,omitempty"`
}

// PushResponse is the body returned by POST /sync.
type PushResponse struct {
	OK         bool   `json:"ok"`
	ProjectKey string `json:"project_key"`
	FilePath   string `json:"file_path"`
	Deleted    bool   `json:"deleted"`
	Merged     bool   `json:"merged"`
	UpdatedAt  string `json:"updated_at"`
}

// File is one memory file as returned by GET /sync.
//
// Content is a pointer so a tombstoned row can report JSON null rather than
// an empty string: the server withholds deleted content so a pull can't
// resurrect it, and "" would be indistinguishable from a genuinely empty
// file.
type File struct {
	FilePath  string  `json:"file_path"`
	Content   *string `json:"content"`
	SourceEnv string  `json:"source_env"`
	UpdatedAt string  `json:"updated_at"`
	Deleted   bool    `json:"deleted"`
}

// SyncResponse is the body returned by GET /sync.
type SyncResponse struct {
	ProjectKey string `json:"project_key"`
	Files      []File `json:"files"`
}

// ClaudeCLIStatus reports whether the server can actually perform a
// semantic merge, so a degraded deployment is visible from outside rather
// than silently falling back forever.
type ClaudeCLIStatus struct {
	CheckedAt string `json:"checked_at"`
	Available *bool  `json:"available"`
	LoggedIn  *bool  `json:"logged_in"`
	Error     string `json:"error,omitempty"`
}

// MergeStatus is the `merge` object inside GET /health.
type MergeStatus struct {
	Enabled        bool            `json:"enabled"`
	ClaudeCLI      ClaudeCLIStatus `json:"claude_cli"`
	LastMergeAt    string          `json:"last_merge_at,omitempty"`
	LastMergeError *MergeError     `json:"last_merge_error"`
}

// MergeError records the most recent failed merge attempt.
type MergeError struct {
	Message string `json:"message"`
	At      string `json:"at"`
}

// Health is the body returned by GET /health, which is intentionally
// unauthenticated so it can be polled by uptime checks.
type Health struct {
	Status       string      `json:"status"`
	GitCommit    string      `json:"git_commit"`
	StartedAt    string      `json:"started_at"`
	LastSyncAt   string      `json:"last_sync_at,omitempty"`
	LastBackupAt string      `json:"last_backup_at,omitempty"`
	Merge        MergeStatus `json:"merge"`
}

// ProjectStats is one row of GET /admin/stats.
type ProjectStats struct {
	ProjectKey    string   `json:"project_key"`
	FileCount     int      `json:"file_count"`
	DeletedCount  int      `json:"deleted_count"`
	Sources       []string `json:"sources"`
	LastUpdatedAt string   `json:"last_updated_at"`
}

// AdminTotals is the aggregate line of GET /admin/stats.
type AdminTotals struct {
	ProjectCount int `json:"project_count"`
	FileCount    int `json:"file_count"`
	DeletedCount int `json:"deleted_count"`
}

// AdminStats is the body returned by GET /admin/stats.
type AdminStats struct {
	Projects     []ProjectStats `json:"projects"`
	Totals       AdminTotals    `json:"totals"`
	GitCommit    string         `json:"git_commit"`
	LastBackupAt string         `json:"last_backup_at,omitempty"`
}

// ErrorResponse is the body returned for any non-2xx.
type ErrorResponse struct {
	Error string `json:"error"`
}

// Validation errors, exported so both the server (rejecting a request) and
// the client (refusing to send one) can act on the same reasons.
var (
	ErrMissingProjectKey = errors.New("project_key is required")
	ErrMissingFilePath   = errors.New("file_path is required")
	ErrMissingContent    = errors.New("content (string) is required unless deleted is true")
	ErrFilePathAbsolute  = errors.New("file_path must be relative")
	ErrFilePathTraversal = errors.New("file_path must not contain a .. segment")
)

// ValidateFilePath enforces that a file_path is safe to join onto a memory
// directory on any machine that later pulls it.
//
// A pulled file gets written to disk by whoever fetches it, so a bad path
// here is not merely invalid data — it is a write outside the memory
// directory on someone else's machine. That is why this is checked on the
// way in (server) as well as on the way out (client), and why it rejects
// per-segment rather than by substring: a filename like "..config" is
// perfectly legitimate and must not be caught, while a "a/../../b" segment
// must be.
func ValidateFilePath(p string) error {
	if p == "" {
		return ErrMissingFilePath
	}
	if path.IsAbs(p) || strings.HasPrefix(p, "/") || strings.HasPrefix(p, `\`) {
		return ErrFilePathAbsolute
	}
	// Windows-style drive prefixes ("C:...") are absolute too, and would be
	// treated as such by anything that later joins this path.
	if len(p) >= 2 && p[1] == ':' {
		return ErrFilePathAbsolute
	}
	for _, seg := range strings.FieldsFunc(p, func(r rune) bool { return r == '/' || r == '\\' }) {
		if seg == ".." {
			return ErrFilePathTraversal
		}
	}
	return nil
}

// Validate checks a push is well-formed, applying the same rules on both
// sides of the wire.
func (r PushRequest) Validate() error {
	if r.ProjectKey == "" {
		return ErrMissingProjectKey
	}
	if err := ValidateFilePath(r.FilePath); err != nil {
		return err
	}
	return nil
}
