# Architecture

## Shape

```
┌────────────────────┐        push (HTTP, direct from Claude Code hook)
│ Laptop A            │ ─────────────────────────────────────┐
│ ~/.claude/projects/  │                                       ▼
│  <project>/memory/   │                             ┌──────────────────┐
└────────────────────┘        pull (SessionStart)     │  Recall server    │
┌────────────────────┐ ◄─────────────────────────────│  (self-hosted)    │
│ Ephemeral cloud sesh │        push                   │                    │
│ (fresh clone, no     │ ─────────────────────────────►│  storage:          │
│  prior pairing)      │        pull                   │  per-project       │
└────────────────────┘ ◄─────────────────────────────│  memory snapshots  │
                                                        └──────────────────┘
```

No peer-to-peer link between environments — every environment only ever talks to the server. This is what makes the "ephemeral session with zero prior setup" requirement work: there's nothing to pair, just one URL + one token.

## One binary

Client and server are the same Rust binary (`docs/rust-rewrite.md`):
`recall serve` runs the server, `recall init` / `status` / `push` / `pull`
run on a developer machine. That is not packaging convenience — the
validation rules and the tombstone/empty-file distinction previously
existed twice, in JavaScript and in bash, with nothing keeping them in
agreement. The `recall-wire` crate is now the single definition both
halves use, which is also why the workspace is split by boundary rather
than being one crate.

## Client side: pure Claude Code hooks, no daemon

Both directions are implemented as hooks in the **project's own `.claude/settings.json`**, not user-level config — a fresh cloud session only has whatever's in the repo it cloned, so user-level hooks would silently never fire there (see `CLAUDE.md`'s Ground rules).

### Push — `PostToolUse` matching `Edit|Write`, `type: "command"`

**Resolved in Phase 0 (see `docs/phase-0-findings.md`):** the installed Claude Code CLI (v2.1.42) has no `FileChanged` event and no `"http"` hook type at all. Memory files are written through the plain `Write`/`Edit` tools, so the push hook is a `PostToolUse` hook matching `Edit|Write`, `type: "command"`, running a script (`hooks/recall-push`) that does the HTTP call itself with `curl`:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          { "type": "command", "command": "recall push" }
        ]
      }
    ]
  }
}
```

The `matcher` field only matches on tool name, not path — there's no built-in path glob. `recall-push` itself checks `tool_input.file_path` from the JSON payload on stdin against the project's memory directory and exits silently if it's not a memory file. **Confirmed live** (not just by reading the source): this catches **dynamically-created topic files** — a real run with this exact hook fired identically for a pre-known `MEMORY.md` write and a `debugging.md` file Claude named on the fly in the same turn, because the check happens per-call against the actual path rather than via a filename registered in advance.

### Pull — `SessionStart`, `type: "command"`

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command", "command": "recall pull" }
        ]
      }
    ]
  }
}
```

`recall-pull` (`hooks/recall-pull`, plain bash + curl + jq — no language runtime dependency, per Phase 0's decision) that:
1. Derives the project key (see below).
2. `GET`s the latest merged snapshot from the server.
3. Writes it into `$CLAUDE_CODE_REMOTE_MEMORY_DIR/projects/<slug>/memory/` (or `~/.claude/projects/<slug>/memory/` when that env var isn't set) before Claude Code loads context.

**Load-bearing prerequisite found in Phase 0, not in the original design:** in a remote/cloud session, Claude Code's auto-memory feature is *disabled by default* unless `CLAUDE_CODE_REMOTE_MEMORY_DIR` is set (see `docs/phase-0-findings.md` §5). `recall-pull` writing files is necessary but not sufficient — that env var has to be set on the remote environment (as a secret, alongside `RECALL_TOKEN`; it can't be baked into committed `settings.json`, whose `env` values don't support `$HOME` expansion) or Claude Code never looks at the memory directory at all.

## Project identity

**Resolved in Phase 0 (see `docs/phase-0-findings.md` §6):** Claude Code does *not* scope its own memory storage by git remote — it uses the local filesystem path (git root, or cwd if none) with non-alphanumeric characters replaced by `-`. That's machine-local by construction (a laptop clone and a cloud clone of the same repo get different slugs), which is exactly the gap Recall exists to bridge — so Recall deliberately uses a *different* derivation than Claude Code's own:

- **`project_key`** (server-side, must agree across machines): the git remote's `owner/repo`, taking just the last two path segments so it normalizes identically across SSH (`git@host:owner/repo.git`), HTTPS (`https://host/owner/repo.git`), and locally-proxied remotes that cloud sandboxes rewrite `origin` to. Implemented in `hooks/lib.sh:recall_project_key`.
- **local memory directory** (client-side, per-machine): replicates Claude Code's own local-path-slug algorithm exactly, so hooks read/write the same directory Claude Code itself uses on that machine. Implemented in `hooks/lib.sh:recall_memory_dir`.

Known limitation: git hosts with nested groups (e.g. GitLab subgroups) collapse to their last two path segments too, which can collide across different subgroups with the same repo name. Acceptable for Phase 0; revisit in Phase 2 if it matters in practice.

## Server

Deliberately boring. Two endpoints:

- `POST /sync` — body: `{ project_key, file_path, content, source_env, timestamp }`. Runs merge (see below) against the stored version, persists the result. `content` can be omitted if `deleted: true` is set instead — see "Deletes are tombstones, not row removal" below.
- `GET /sync?project_key=...` — returns the current merged set of memory files for that project, each with a `deleted` flag.

Storage: whatever's simplest to self-host and keep running — a single SQLite file behind a small server process is enough for one user's data; don't reach for a distributed database for this. Auth: one bearer token, generated once, stored as an env var on every environment (never committed to the repo).

### Deletes are tombstones, not row removal

A pushed delete (`{ project_key, file_path, deleted: true, source_env }`)
sets a `deleted` flag on the existing row rather than removing it —
`content` is left untouched by that update, so the last known content is
still sitting in the database even though nothing in the app exposes an
"undo" for it yet. `GET /sync` reports `deleted: true` for that row and
withholds `content` (`null`) so a pull can't accidentally resurrect it.

This existed as a real gap before it was built: `recall-push` used to
silently no-op when the file it was called about no longer existed,
which meant the server never learned about a delete at all, and a
deleted file would come back on the next pull. See `hooks/README.md` for
how the client side actually detects a local delete — there's no hook
event for it, so it's closer to "eventual" than "instant."

## Merge strategy

**Implemented in Phase 2 (see `ROADMAP.md`).** Not append-only, not naive last-write-wins. `POST /sync` only attempts a merge when there's actually something to reconcile — an existing, non-tombstoned row whose stored content differs byte-for-byte from the incoming push; a brand-new file, a revived tombstone, or a client re-pushing unchanged content all skip straight to a plain write. When it does attempt one, it shells out to the *local* `claude` CLI (`claude -p`), never the Anthropic API directly, keeping the no-API-key rule in `CLAUDE.md` intact — merge rides whatever account is logged into that CLI on the server host (`claude setup-token`, a one-time interactive step documented in `deploy/README.md`; a real operational requirement, not an afterthought).

The merge prompt instructs the model to preserve every distinct fact from both versions, collapse restated facts to one clear wording, and keep both sides of a genuine contradiction with an inline marker for a human to resolve later — confirmed live to do exactly that, including on a real contradiction (`docs/phase-0-findings.md`-style empirical check, not just a read of the prompt). The call runs with a minimal custom system prompt, `--exclude-dynamic-system-prompt-sections`, and `--strict-mcp-config`, in a neutral working directory: confirmed live that skipping all three (i.e. plain `claude -p` from inside a real project directory) balloons a trivial merge call from roughly $0.01 to $0.19 in wasted cache-creation tokens, since the task needs no tools and no project context.

Every failure mode — CLI missing, not logged in, non-zero exit, malformed output, a `RECALL_MERGE_TIMEOUT_MS`-exceeding hang (default 45s) — falls back to last-write-wins rather than rejecting the sync, because a broken or not-yet-configured merge step must never be able to take basic sync down with it. `GET /health`'s `merge` object (`claude_cli.logged_in`, `last_merge_at`, `last_merge_error`) exists specifically so this degraded state is visible from outside instead of silent.

Structured/settings-like data, if Recall ever expands beyond auto memory (it currently shouldn't — see "Explicitly deferred" in `ROADMAP.md`) would use plain deterministic merge; this doesn't apply to the current scope.

## What's deliberately not here

- No client daemon or background watcher — hooks are the entire client.
- No multi-user auth, no OAuth, no billing.
- No Anthropic API key anywhere in the request path.
- No attempt to sync `CLAUDE.md`, skills, or settings — git already does `CLAUDE.md`, and the rest is out of scope (see "Explicitly deferred" in `ROADMAP.md`).
