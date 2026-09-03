// Package config loads settings from the environment for both halves of
// the binary.
//
// Variable names are deliberately unchanged from the shell/Node
// implementation this replaces, so no machine and no cloud environment
// needs re-provisioning to switch over.
package config

import (
	"errors"
	"os"
	"strconv"
	"time"

	"github.com/pimlabs/recall/internal/claudecode"
)

// Client is what `recall push`, `pull`, `status` and `init` need.
type Client struct {
	URL       string
	Token     string
	SourceEnv string
	Claude    claudecode.Env
}

var (
	ErrMissingURL   = errors.New("RECALL_URL must be set (e.g. https://recall.example.com)")
	ErrMissingToken = errors.New("RECALL_TOKEN must be set")
)

// LoadClient reads client configuration. It does not error on missing
// values — callers that need them say so via Require, because `recall
// status` and `recall init` are specifically useful when configuration is
// incomplete and should report that rather than refuse to run.
func LoadClient() Client {
	sourceEnv := os.Getenv("RECALL_SOURCE_ENV")
	if sourceEnv == "" {
		if h, err := os.Hostname(); err == nil {
			sourceEnv = h
		} else {
			sourceEnv = "unknown"
		}
	}
	return Client{
		URL:       os.Getenv("RECALL_URL"),
		Token:     os.Getenv("RECALL_TOKEN"),
		SourceEnv: sourceEnv,
		Claude: claudecode.Env{
			RemoteMemoryDir: os.Getenv("CLAUDE_CODE_REMOTE_MEMORY_DIR"),
			ConfigDir:       os.Getenv("CLAUDE_CONFIG_DIR"),
			Home:            os.Getenv("HOME"),
		},
	}
}

// Require reports what's missing for an operation that actually talks to
// the server.
func (c Client) Require() error {
	if c.URL == "" {
		return ErrMissingURL
	}
	if c.Token == "" {
		return ErrMissingToken
	}
	return nil
}

// Server is what `recall serve` needs.
type Server struct {
	Addr      string
	Token     string
	DBPath    string
	GitCommit string

	BackupDir      string
	BackupInterval time.Duration
	BackupKeep     int

	RateLimitWindow time.Duration
	RateLimitMax    int

	MergeEnabled         bool
	MergeTimeout         time.Duration
	ClaudeBin            string
	ClaudeStatusInterval time.Duration
}

// ErrMissingServerToken mirrors the Node server's refusal to start without
// auth — a server reachable from the internet with no token is not a
// degraded mode worth supporting.
var ErrMissingServerToken = errors.New("RECALL_TOKEN is not set; refusing to start with no auth")

// LoadServer reads server configuration, applying the same defaults the
// Node implementation used.
func LoadServer() (Server, error) {
	s := Server{
		Addr:                 ":" + envOr("RECALL_PORT", "8787"),
		Token:                os.Getenv("RECALL_TOKEN"),
		DBPath:               envOr("RECALL_DB_PATH", "data/recall.db"),
		GitCommit:            envOr("RECALL_GIT_COMMIT", "unknown"),
		BackupDir:            os.Getenv("RECALL_BACKUP_DIR"),
		BackupInterval:       time.Duration(envInt("RECALL_BACKUP_INTERVAL_HOURS", 24)) * time.Hour,
		BackupKeep:           envInt("RECALL_BACKUP_KEEP", 7),
		RateLimitWindow:      time.Duration(envInt("RECALL_RATE_LIMIT_WINDOW_MS", 60_000)) * time.Millisecond,
		RateLimitMax:         envInt("RECALL_RATE_LIMIT_MAX", 60),
		MergeEnabled:         os.Getenv("RECALL_MERGE_ENABLED") != "false",
		MergeTimeout:         time.Duration(envInt("RECALL_MERGE_TIMEOUT_MS", 45_000)) * time.Millisecond,
		ClaudeBin:            envOr("RECALL_CLAUDE_BIN", "claude"),
		ClaudeStatusInterval: time.Duration(envInt("RECALL_CLAUDE_STATUS_INTERVAL_MS", 30*60_000)) * time.Millisecond,
	}
	if s.Token == "" {
		return s, ErrMissingServerToken
	}

	// setInterval in the Node version silently misbehaved past ~24.8 days;
	// Go has no such limit, but an absurd interval is still worth clamping
	// rather than honoring, and a non-positive one would spin.
	if s.BackupInterval <= 0 {
		s.BackupInterval = 24 * time.Hour
	}
	if s.RateLimitWindow <= 0 {
		s.RateLimitWindow = time.Minute
	}
	return s, nil
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func envInt(key string, fallback int) int {
	v := os.Getenv(key)
	if v == "" {
		return fallback
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return fallback
	}
	return n
}
