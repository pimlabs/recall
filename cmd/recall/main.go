// Command recall syncs Claude Code's auto memory across machines.
//
// One binary, both halves: `recall serve` runs the server, everything else
// runs on a developer machine or inside a Claude Code session as a hook.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"

	"github.com/pimlabs/recall/internal/config"
	"github.com/pimlabs/recall/internal/hookio"
	"github.com/pimlabs/recall/internal/hooks"
	"github.com/pimlabs/recall/internal/project"
	"github.com/pimlabs/recall/internal/server"
	"github.com/pimlabs/recall/internal/settings"
	"github.com/pimlabs/recall/internal/state"
	"github.com/pimlabs/recall/internal/store"
	"github.com/pimlabs/recall/internal/syncclient"
)

// Set at build time: -ldflags "-X main.version=v0.1.0 -X main.commit=abc1234"
var (
	version = "dev"
	commit  = "unknown"
)

const usage = `recall — sync Claude Code's auto memory across machines and cloud sessions

Usage:
  recall init      Wire the current project's .claude/settings.json for sync
  recall status    Show whether sync is configured and reachable, here
  recall serve     Run the sync server
  recall push      (hook) Push a changed memory file — called by PostToolUse
  recall pull      (hook) Pull the latest memory — called by SessionStart
  recall version   Print the version
  recall help      Show this help

Environment (client — see docs/token-setup.md):
  RECALL_URL         Base URL of your Recall server
  RECALL_TOKEN       Your bearer token
  RECALL_SOURCE_ENV  Label recorded with each push (default: hostname)

Environment (server):
  RECALL_TOKEN, RECALL_PORT, RECALL_DB_PATH, RECALL_BACKUP_DIR,
  RECALL_MERGE_ENABLED, RECALL_MERGE_TIMEOUT_MS, RECALL_CLAUDE_BIN
`

func main() {
	args := os.Args[1:]
	if len(args) == 0 {
		fmt.Print(usage)
		os.Exit(hookio.ExitOK)
	}

	var err error
	code := hookio.ExitOK

	switch args[0] {
	case "init":
		code, err = cmdInit()
	case "status":
		code, err = cmdStatus(args[1:])
	case "serve":
		code, err = cmdServe()
	case "push":
		code, err = cmdPush()
	case "pull":
		code, err = cmdPull()
	case "version", "--version", "-v":
		fmt.Printf("recall %s (%s)\n", version, commit)
	case "help", "--help", "-h":
		fmt.Print(usage)
	default:
		fmt.Fprint(os.Stderr, usage)
		os.Exit(hookio.ExitConfig)
	}

	if err != nil {
		fmt.Fprintf(os.Stderr, "recall: %v\n", err)
		if code == hookio.ExitOK {
			code = hookio.ExitConfig
		}
	}
	os.Exit(code)
}

// gitRoot resolves the project root the same way Claude Code does: the git
// root, falling back to the working directory.
func gitRoot() string {
	out, err := exec.Command("git", "rev-parse", "--show-toplevel").Output()
	if err == nil {
		if root := strings.TrimSpace(string(out)); root != "" {
			return root
		}
	}
	cwd, err := os.Getwd()
	if err != nil {
		return "."
	}
	return cwd
}

func gitRemote() string {
	out, err := exec.Command("git", "remote", "get-url", "origin").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}

func clientEnv() (hooks.Env, config.Client, error) {
	cfg := config.LoadClient()
	root := gitRoot()
	env := hooks.Env{
		MemoryDir:  cfg.Claude.MemoryDir(root),
		StateFile:  cfg.Claude.StateFile(root),
		ProjectKey: project.Key(gitRemote(), root),
		SourceEnv:  cfg.SourceEnv,
	}
	if err := cfg.Require(); err != nil {
		return env, cfg, err
	}
	env.Client = syncclient.New(cfg.URL, cfg.Token)
	return env, cfg, nil
}

func cmdInit() (int, error) {
	out, err := exec.Command("git", "rev-parse", "--show-toplevel").Output()
	if err != nil {
		return hookio.ExitConfig, fmt.Errorf("not inside a git repository — run this from the project you want to sync")
	}
	root := strings.TrimSpace(string(out))
	path := filepath.Join(root, ".claude", "settings.json")

	changed, err := settings.WireFile(path)
	if err != nil {
		return hookio.ExitConfig, err
	}
	if changed {
		fmt.Printf("recall: wired hooks into %s\n", path)
	} else {
		fmt.Printf("recall: already wired, nothing to change (%s)\n", path)
	}

	cfg := config.LoadClient()
	if cfg.URL == "" || cfg.Token == "" {
		fmt.Println()
		if cfg.URL == "" {
			fmt.Println("  ! RECALL_URL is not set in this shell")
		}
		if cfg.Token == "" {
			fmt.Println("  ! RECALL_TOKEN is not set in this shell")
		}
		fmt.Print(`
  Add these to your shell profile (~/.zshrc, ~/.bashrc) before sync works:

    export RECALL_URL="https://your-recall-host"
    export RECALL_TOKEN="<your token>"

  See docs/token-setup.md for generating the token, and for the extra
  variables a claude.ai cloud environment needs.
`)
	}

	if changed {
		fmt.Printf(`
  Next: review and commit the change, so fresh clones and cloud sessions
  pick it up too — that's what makes this work without per-machine setup.

    git -C %q diff .claude/settings.json
    git -C %q add .claude/settings.json && git -C %q commit -m "Enable Recall memory sync"
`, root, root, root)
	}
	return hookio.ExitOK, nil
}

// statusReport is what `recall status --json` prints, for scripts and CI.
type statusReport struct {
	Project      string `json:"project"`
	ProjectKey   string `json:"project_key"`
	MemoryDir    string `json:"memory_dir"`
	MemoryFiles  int    `json:"memory_files"`
	HooksWired   bool   `json:"hooks_wired"`
	URLSet       bool   `json:"url_set"`
	TokenSet     bool   `json:"token_set"`
	ServerOK     bool   `json:"server_ok"`
	ServerError  string `json:"server_error,omitempty"`
	GitCommit    string `json:"git_commit,omitempty"`
	MergeReady   bool   `json:"merge_ready"`
	SyncedFiles  int    `json:"synced_files"`
	LastSyncedAt string `json:"last_synced_at,omitempty"`
}

func cmdStatus(args []string) (int, error) {
	asJSON := len(args) > 0 && args[0] == "--json"

	cfg := config.LoadClient()
	root := gitRoot()
	rep := statusReport{
		Project:    root,
		ProjectKey: project.Key(gitRemote(), root),
		MemoryDir:  cfg.Claude.MemoryDir(root),
		URLSet:     cfg.URL != "",
		TokenSet:   cfg.Token != "",
	}

	if files, err := state.ListMemoryFiles(rep.MemoryDir); err == nil {
		rep.MemoryFiles = len(files)
	}
	if b, err := os.ReadFile(filepath.Join(root, ".claude", "settings.json")); err == nil {
		rep.HooksWired = settings.IsWired(b)
	}

	ctx := context.Background()
	if rep.URLSet {
		c := syncclient.New(cfg.URL, cfg.Token)
		if health, err := c.Health(ctx); err != nil {
			rep.ServerError = err.Error()
		} else {
			rep.ServerOK = true
			rep.GitCommit = health.GitCommit
			rep.LastSyncedAt = health.LastSyncAt
			rep.MergeReady = health.Merge.ClaudeCLI.LoggedIn != nil && *health.Merge.ClaudeCLI.LoggedIn
		}
		if rep.TokenSet {
			if resp, err := c.Pull(ctx, rep.ProjectKey); err == nil {
				for _, f := range resp.Files {
					if !f.Deleted {
						rep.SyncedFiles++
					}
				}
			}
		}
	}

	if asJSON {
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		return hookio.ExitOK, enc.Encode(rep)
	}

	fmt.Printf("project      : %s\n", rep.Project)
	fmt.Printf("project_key  : %s\n", rep.ProjectKey)
	fmt.Printf("memory dir   : %s\n", rep.MemoryDir)
	fmt.Printf("memory files : %d on disk\n", rep.MemoryFiles)
	if rep.HooksWired {
		fmt.Println("hooks wired  : yes")
	} else {
		fmt.Println("hooks wired  : NO — run 'recall init' in this project")
	}
	fmt.Printf("RECALL_URL   : %s\n", orUnset(cfg.URL))
	fmt.Printf("RECALL_TOKEN : %s\n", setOrUnset(cfg.Token))
	switch {
	case !rep.URLSet:
	case rep.ServerOK:
		fmt.Printf("server       : reachable (git_commit %s)\n", rep.GitCommit)
		fmt.Printf("merge        : %s\n", readyOrNot(rep.MergeReady))
		fmt.Printf("synced files : %d on server\n", rep.SyncedFiles)
	default:
		fmt.Printf("server       : UNREACHABLE (%s)\n", rep.ServerError)
	}
	return hookio.ExitOK, nil
}

func cmdServe() (int, error) {
	cfg, err := config.LoadServer()
	if err != nil {
		return hookio.ExitConfig, err
	}
	st, err := store.Open(cfg.DBPath)
	if err != nil {
		return hookio.ExitConfig, err
	}
	defer st.Close()

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	srv := server.New(cfg, st)
	srv.Start(ctx)
	if err := srv.ListenAndServe(ctx); err != nil {
		return hookio.ExitServer, err
	}
	return hookio.ExitOK, nil
}

func cmdPush() (int, error) {
	payload := hookio.ParsePostToolUse(os.Stdin)
	if payload.ToolInput.FilePath == "" {
		return hookio.ExitOK, nil
	}
	env, _, err := clientEnv()
	if err != nil {
		return hookio.ExitConfig, err
	}
	res, err := hooks.Push(context.Background(), env, payload.ToolInput.FilePath)
	if err != nil {
		return hookio.ExitServer, err
	}
	if res.Pushed != "" || len(res.Deleted) > 0 {
		fmt.Fprintf(os.Stderr, "recall-push: pushed %d, deleted %d for %s\n",
			boolToInt(res.Pushed != ""), len(res.Deleted), env.ProjectKey)
	}
	return hookio.ExitOK, nil
}

func cmdPull() (int, error) {
	env, _, err := clientEnv()
	if err != nil {
		// A session must still start when Recall isn't configured here.
		fmt.Fprintf(os.Stderr, "recall-pull: %v, leaving local memory untouched\n", err)
		return hookio.ExitOK, nil
	}
	res, err := hooks.Pull(context.Background(), env)
	if err != nil {
		// Likewise when the server is unreachable: a pull failure must
		// never be the reason a session can't start.
		fmt.Fprintf(os.Stderr, "recall-pull: fetch failed (%v), leaving local memory untouched\n", err)
		return hookio.ExitOK, nil
	}
	res.Describe(os.Stderr, env.ProjectKey)
	return hookio.ExitOK, nil
}

func orUnset(s string) string {
	if s == "" {
		return "(unset)"
	}
	return s
}

func setOrUnset(s string) string {
	if s == "" {
		return "(unset)"
	}
	return "set"
}

func readyOrNot(ready bool) string {
	if ready {
		return "ready (claude CLI logged in)"
	}
	return "not configured — server falls back to last-write-wins"
}

func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}
