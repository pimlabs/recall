# Recall hooks

Two small `command`-type hook scripts, no daemon. Wire them into a project's
own `.claude/settings.json` (see `settings.snippet.json` — merge its
`hooks` block into the target project's existing settings) so any clone of
that project, laptop or fresh cloud session, gets sync automatically once
the environment variables below are set.

## Client-side environment variables

Set these on every environment that should push/pull (laptop shell
profile, cloud session secrets) — never commit them:

| Variable | Required by | Purpose |
|---|---|---|
| `RECALL_URL` | both hooks | Base URL of the Recall server, e.g. `https://recall.example.com` |
| `RECALL_TOKEN` | both hooks | The personal bearer token (see server setup) |
| `RECALL_SOURCE_ENV` | `recall-push` (optional) | Label stored with each push, e.g. `laptop` / `cloud-<session-id>`. Defaults to `hostname` |
| `CLAUDE_CODE_REMOTE_MEMORY_DIR` | remote/cloud environments only | See below — **not optional there** |

### Why `CLAUDE_CODE_REMOTE_MEMORY_DIR` matters

Confirmed by reading the installed Claude Code CLI source (v2.1.42): auto
memory is **disabled by default whenever `CLAUDE_CODE_REMOTE` is set**,
unless `CLAUDE_CODE_REMOTE_MEMORY_DIR` is also set. Every ephemeral cloud
session sets `CLAUDE_CODE_REMOTE=true`. Without this variable, Claude Code
never reads or writes `MEMORY.md`/topic files at all in that session — pull
would have nothing to feed into, and there'd be nothing for push to catch.

This can't be baked into the committed `settings.json` snippet: settings
`env` values are applied as **literal strings**, with no `$HOME`/`${HOME}`
expansion (confirmed empirically — see `../docs/phase-0-findings.md`). A
hardcoded absolute path would break on every machine with a different home
directory, and guessing wrong risks silently redirecting (or fragmenting)
a laptop's existing `~/.claude` memory. So set it the same way you'd set
`RECALL_TOKEN`: as a real environment secret on each remote/cloud
environment, pointing at that environment's own `~/.claude`
(e.g. `CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/claude/.claude`). Laptops
normally don't need it at all — `CLAUDE_CODE_REMOTE` isn't set there, so
auto memory already works without it.

## Server-side environment variables

| Variable | Purpose |
|---|---|
| `RECALL_TOKEN` | The same bearer token clients send |
| `RECALL_PORT` | Defaults to `8787` |
| `RECALL_DB_PATH` | Defaults to `server/data/recall.db` |

## How the two hooks work

- **`recall-push`** — a `PostToolUse` hook matching `Edit\|Write`. Claude
  Code has no `FileChanged` event and no `"http"` hook type in the
  installed version, so this is a `"command"` hook: every Edit/Write call
  reaches the script (the declarative `matcher` only matches on tool name,
  not path), the script itself checks whether `tool_input.file_path` falls
  under the project's auto-memory directory and exits silently if not.
  Because the check happens per-call in the script rather than via a
  filename-based matcher, it catches topic files Claude names on the fly
  (e.g. `debugging.md`) exactly the same as `MEMORY.md` — confirmed live
  against the real CLI, see `../docs/phase-0-findings.md`.
- **`recall-pull`** — a `SessionStart` hook that fetches the latest synced
  snapshot and writes it straight into the same directory Claude Code
  reads auto-memory from, before context loads. No `additionalContext`
  injection needed.

Both scripts derive the local memory directory the same way Claude Code's
own CLI does (`hooks/lib.sh:recall_memory_dir`), and derive the
cross-machine `project_key` from the git remote's `owner/repo` — a
deliberately different, more stable derivation than Claude Code's own
(local-filesystem-path-based) memory scoping. See `../ARCHITECTURE.md`.

## Dependencies

`bash`, `curl`, `jq`. No language runtime beyond what's already on the
box running Claude Code.
