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

## Client side: pure Claude Code hooks, no daemon

Both directions are implemented as hooks in the **project's own `.claude/settings.json`** (see `PROMPT.md` for why it can't be user-level config).

### Push — `FileChanged` (or `PostToolUse` matching `Edit|Write`), `type: "http"`

Claude Code's hook runner makes the HTTP call itself — no client script, no daemon:

```json
{
  "hooks": {
    "FileChanged": [
      {
        "matcher": "MEMORY.md",
        "hooks": [
          {
            "type": "http",
            "url": "https://<your-recall-host>/sync",
            "headers": { "Authorization": "Bearer $RECALL_TOKEN" }
          }
        ]
      }
    ]
  }
}
```

Open question to resolve in Phase 0 (flagged, not assumed): whether `FileChanged`'s matcher can pick up **dynamically-created topic files** under `memory/` (files like `debugging.md` that Claude names on the fly), or only literal pre-known filenames. If it can't glob-match new files, `PostToolUse` on `Edit|Write` with an `if` path-glob against `**/memory/**` is the fallback — confirm against the installed Claude Code version's actual hook-matching behavior before committing to one.

### Pull — `SessionStart`, `type: "command"`

```json
{
  "hooks": {
    "SessionStart": [
      { "type": "command", "command": "recall-pull" }
    ]
  }
}
```

`recall-pull` is a small script (bundled with this project, installed however's simplest — a single self-contained binary/script is preferable to a language runtime dependency, decide in Phase 0) that:
1. Derives the project key (see below).
2. `GET`s the latest merged snapshot from the server.
3. Writes it into `~/.claude/projects/<project>/memory/` before Claude Code loads context.

## Project identity

Derive the same way Claude Code derives its own per-project memory scoping: from the project's git remote URL. Don't invent a separate ID scheme — if Recall's key derivation drifts from Claude Code's own, a laptop and a cloud session could disagree about which memory belongs to which project. Confirm the exact derivation (likely a normalized form of the `origin` remote URL) empirically in Phase 0 rather than guessing the hash/format.

## Server

Deliberately boring. Two endpoints:

- `POST /sync` — body: `{ project_key, file_path, content, source_env, timestamp }`. Runs merge (see below) against the stored version, persists the result.
- `GET /sync?project_key=...` — returns the current merged set of memory files for that project.

Storage: whatever's simplest to self-host and keep running — a single SQLite file behind a small server process is enough for one user's data; don't reach for a distributed database for this. Auth: one bearer token, generated once, stored as an env var on every environment (never committed to the repo).

## Merge strategy

**Not append-only, not naive last-write-wins.** For memory content specifically, follow `claude-brain`'s approach: shell out to the local `claude` CLI (`claude -p`) to semantically merge two versions of a memory file — dedupe restated facts, reconcile contradictions, keep both if they're genuinely different information. This keeps the "no API key" constraint intact (`claude -p` rides whatever auth is already on the machine running the merge — almost certainly the server, so the server itself needs a logged-in `claude` CLI available to it, which is a real operational requirement to design around, not an afterthought).

Structured/settings-like data, if Recall ever expands beyond auto memory (it currently shouldn't — see Non-goals in `PROMPT.md`) would use plain deterministic merge; this doesn't apply to the current scope.

## What's deliberately not here

- No client daemon or background watcher — hooks are the entire client.
- No multi-user auth, no OAuth, no billing.
- No Anthropic API key anywhere in the request path.
- No attempt to sync `CLAUDE.md`, skills, or settings — git already does `CLAUDE.md`, and the rest is out of scope (see `PROMPT.md`).
