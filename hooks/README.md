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

### claude.ai cloud environments also need a network allowlist entry

Confirmed live 2026-08-12: a claude.ai cloud environment's outbound network
access defaults to something short of unrestricted, so a self-hosted
`RECALL_URL` domain gets rejected with `403`/`CONNECT tunnel failed` until
explicitly allowed. Fix in that environment's settings (the same
"Add/Edit cloud environment" dialog where the variables above get set):
set **Network access** to **Custom** and add the server's domain (e.g.
`recall.pimlabs.id`) under **Allowed domains**. This is per-environment,
not account-wide — a new environment needs it set again.

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
  injection needed. Also removes any local file the server has tombstoned
  (see below) — deleted content doesn't come back on a fresh pull.

### Deletes: no dedicated hook, so `recall-push` reconciles instead

There's no `FileDeleted`-style event, and a delete via the `Bash` tool
(`rm ...`) wouldn't match `recall-push`'s `Edit|Write` matcher even if
there were one. So `recall-push` doesn't rely on being told about a
delete directly — every time it runs (triggered by *any* Edit/Write to a
memory file), it compares the current directory listing against the last
known one (`hooks/lib.sh:recall_state_file`, a small JSON file kept
*next to* the memory directory, not inside it) and reports anything
that's gone missing as a delete to the server. The server keeps a
tombstone row (content preserved, flagged `deleted`) rather than removing
the row outright, so a mistaken delete is recoverable at the database
level even though nothing in the app surfaces an "undo" yet.

Practical consequence: a delete propagates the next time *any* memory
file in that project is edited, not the instant it happens — there's no
hook to catch the instant, so this is the closest available
approximation. `recall-pull` also refreshes the state file after every
pull, so the baseline stays accurate even on a machine that only ever
pulls.

Both scripts derive the local memory directory the same way Claude Code's
own CLI does (`hooks/lib.sh:recall_memory_dir`), and derive the
cross-machine `project_key` from the git remote's `owner/repo` — a
deliberately different, more stable derivation than Claude Code's own
(local-filesystem-path-based) memory scoping. See `../ARCHITECTURE.md`.

## Dependencies

`bash`, `curl`, `jq`. No language runtime beyond what's already on the
box running Claude Code.
