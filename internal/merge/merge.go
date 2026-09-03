// Package merge reconciles two versions of a memory file by shelling out
// to the local `claude` CLI.
//
// It shells out on purpose: no Anthropic API key appears anywhere in this
// codebase (see CLAUDE.md), so the merge rides whatever account is already
// logged into the CLI on the host. That is a real operational dependency,
// which is why every failure here degrades to last-write-wins rather than
// failing the sync — a not-yet-configured merge must never be able to take
// basic syncing down with it.
package merge

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"time"
)

// SystemPrompt replaces Claude Code's default agentic system prompt.
//
// Together with --exclude-dynamic-system-prompt-sections and
// --strict-mcp-config, and running from a neutral working directory, this
// is what keeps a merge call cheap: measured live, the default prompt plus
// project context turned a trivial merge into roughly $0.19 of
// cache-creation tokens, versus about $0.01 with these. The task needs no
// tools and no project context — only text in, text out.
const SystemPrompt = "You are a precise text-merging assistant for a personal notes-sync tool. " +
	"You merge two versions of a Claude Code auto-memory file that were edited independently on different machines and then synced through a central server. " +
	"Rules: preserve every distinct fact from both versions; if both state the same fact in different words, keep it once, worded clearly (prefer the more complete wording); " +
	"if they directly contradict each other, keep both and mark the conflict inline so a human can resolve it later; never invent information that isn't present in either version. " +
	"Output ONLY the merged file content — no preamble, no explanation, no code fences, nothing else."

// Merger runs merges through a `claude` binary.
type Merger struct {
	Bin     string
	Timeout time.Duration
}

// Prompt builds the user-side prompt for one merge.
func Prompt(oldContent, newContent string) string {
	return fmt.Sprintf("--- VERSION A (currently stored) ---\n%s\n\n--- VERSION B (incoming) ---\n%s", oldContent, newContent)
}

// cliResult is the subset of `claude -p --output-format json` we read.
type cliResult struct {
	IsError bool   `json:"is_error"`
	Result  string `json:"result"`
	Subtype string `json:"subtype"`
}

var ErrNotAvailable = errors.New("claude CLI unavailable")

// Merge returns a single reconciled version of the two inputs.
func (m Merger) Merge(ctx context.Context, oldContent, newContent string) (string, error) {
	timeout := m.Timeout
	if timeout <= 0 {
		timeout = 45 * time.Second
	}
	ctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	cmd := exec.CommandContext(ctx, m.Bin,
		"-p",
		"--output-format", "json",
		"--input-format", "text",
		"--system-prompt", SystemPrompt,
		"--exclude-dynamic-system-prompt-sections",
		"--strict-mcp-config",
	)
	// A neutral working directory: nothing here should read, or be
	// influenced by, whatever project happens to be on disk.
	cmd.Dir = os.TempDir()
	cmd.Stdin = strings.NewReader(Prompt(oldContent, newContent))

	var stdout, stderr strings.Builder
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		if ctx.Err() == context.DeadlineExceeded {
			return "", fmt.Errorf("claude merge timed out after %s", timeout)
		}
		detail := strings.TrimSpace(stderr.String())
		if detail == "" {
			detail = "(no stderr)"
		}
		return "", fmt.Errorf("claude failed: %v: %s", err, truncate(detail, 500))
	}

	var res cliResult
	if err := json.Unmarshal([]byte(stdout.String()), &res); err != nil {
		return "", fmt.Errorf("claude returned non-JSON output: %s", truncate(stdout.String(), 500))
	}
	if res.IsError {
		return "", fmt.Errorf("claude merge failed: %s", truncate(res.Result, 500))
	}
	return res.Result, nil
}

// Status is the local, zero-cost check of whether a merge could work at
// all: is the binary there, and is it logged in. Exposed via /health so a
// degraded deployment is visible without waiting for a real conflict.
type Status struct {
	CheckedAt string
	Available bool
	LoggedIn  bool
	Err       string
}

type authStatus struct {
	LoggedIn bool `json:"loggedIn"`
}

// CheckStatus runs `claude auth status`, which is a local check and costs
// no tokens.
func (m Merger) CheckStatus(ctx context.Context) Status {
	ctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()

	out, err := exec.CommandContext(ctx, m.Bin, "auth", "status").Output()
	now := time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
	if err != nil {
		msg := "claude CLI not found on PATH"
		if !errors.Is(err, exec.ErrNotFound) {
			msg = err.Error()
		}
		return Status{CheckedAt: now, Err: msg}
	}
	var a authStatus
	if err := json.Unmarshal(out, &a); err != nil {
		return Status{CheckedAt: now, Available: true, Err: "could not parse `claude auth status` output"}
	}
	return Status{CheckedAt: now, Available: true, LoggedIn: a.LoggedIn}
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n]
}
