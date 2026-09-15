# Architecture

## Shape

```
   Laptop                          Ephemeral cloud session
   ~/.claude/projects/<slug>/      fresh clone, never paired
        │  ▲                            │  ▲
   push │  │ pull                  push │  │ pull
        ▼  │                            ▼  │
   ┌───────────────────────────────────────────────┐
   │           Recall server (self-hosted)         │
   │   one SQLite file; every row keyed by         │
   │   (project_key, file_path)                    │
   └───────────────────────────────────────────────┘
```

No peer-to-peer link between environments — every environment only ever talks to the server. This is what makes the "ephemeral session with zero prior setup" requirement work: there's nothing to pair, just one URL + one token.

## One binary

Client and server are the same Rust binary (`docs/history/rust-rewrite.md`):
`recall serve` runs the server; `recall init` / `status` / `promote` /
`push` / `pull` run on a developer machine. That is not packaging
convenience. The validation rules and the tombstone/empty-file distinction
previously existed twice — in JavaScript on the server and in bash on the
client — with nothing keeping them in agreement. The `recall-wire` crate is
now the single definition both halves use, which is also why the workspace
is split by boundary rather than being one crate.

### Code map

Each crate is a boundary, not a folder. They compile and test independently,
and the dependency arrows only ever point downward.

```
recall-sync       the binary: one module per command
   │              init · status · promote · hook (push/pull) · serve · project
   ├──────────────┬──────────────┐
   ▼              ▼              │
recall-hooks   recall-server     │   the two halves
   │  │           │              │
   │  └───────────┼──────────────┼────┐
   └──────┬───────┘              │    │
          ▼                      ▼    ▼
     recall-wire            recall-paths
     the frozen             where things live, what a
     HTTP contract          project is called, what is
                            synced under which key
```

| Crate | Holds | Why it's separate |
|---|---|---|
| `recall-wire` | Request/response shapes and the validation both sides apply | These rules were once written twice — JavaScript and bash — and drifted. One definition is the whole point. |
| `recall-paths` | Claude Code's memory paths, `project_key` derivation, client config | Tracks *someone else's* implementation. When the CLI changes there is one place to fix, with its own tests. |
| `recall-hooks` | `push`, `pull`, the baseline, the HTTP client, the settings merge | Everything that runs inside a user's editing session, where being quiet matters more than being thorough. |
| `recall-server` | SQLite store, `claude -p` merge, the axum API | Everything that runs on the host. Never depends on `recall-hooks`. |
| `recall-sync` | Argument parsing and one module per command — `serve` among them, so this is where the server process starts too | Thin. Each command's *failure policy* is documented beside the command it governs. Named for the client half only because `recall-cli` and `recall` were both taken on crates.io; published since v0.1.0, so the name is now fixed. |

The generated API docs (`cargo doc --workspace --open`) are the reference;
`missing_docs` is denied in every library crate and CI runs rustdoc with
`-D warnings`, so an undocumented public item or a stale doc link fails the
build.

## Client side: pure Claude Code hooks, no daemon

Both directions are implemented as hooks in the **project's own `.claude/settings.json`**, not user-level config — a fresh cloud session only has whatever's in the repo it cloned, so user-level hooks would silently never fire there (see `CLAUDE.md`'s Ground rules).

### Push — `PostToolUse` matching `Edit|Write`, `type: "command"`

**Resolved in Phase 0 (see `docs/history/phase-0-findings.md`):** the installed Claude Code CLI (v2.1.42) has no `FileChanged` event and no `"http"` hook type at all. Memory files are written through the plain `Write`/`Edit` tools, so the push hook is a `PostToolUse` hook matching `Edit|Write`, `type: "command"`, running `recall push`, which makes the HTTP call itself:

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          { "type": "command",
            "command": "if command -v recall >/dev/null 2>&1; then recall push; fi" }
        ]
      }
    ]
  }
}
```

The `matcher` field only matches on tool name, not path — there's no built-in path glob. `recall push` itself checks `tool_input.file_path` from the JSON payload on stdin against the project's memory directory and exits silently if it's not a memory file — *before* it reads any configuration, so a machine that has cloned a wired project but isn't set up yet doesn't error on every unrelated edit. **Confirmed live** (not just by reading the source): this catches **dynamically-created topic files** — a real run with this exact hook fired identically for a pre-known `MEMORY.md` write and a `debugging.md` file Claude named on the fly in the same turn, because the check happens per-call against the actual path rather than via a filename registered in advance.

**Why the command is guarded rather than a bare `recall push`:** this file is
committed, so it travels to every machine that clones the project —
including the ephemeral cloud session that is the whole reason it is
committed rather than wired user-side. On a machine that has not installed
Recall, a bare command exits 127 on every Edit and Write in the session. The
bash implementation this replaced was self-contained in the repository and so
worked in a fresh clone with nothing installed; a binary cannot be, so it
earns the property back with the guard. Not installed is a silent no-op.

### Pull — `SessionStart`, `type: "command"`

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          { "type": "command",
            "command": "if command -v recall >/dev/null 2>&1; then recall pull; fi" }
        ]
      }
    ]
  }
}
```

`recall pull`:
1. Derives the project key (see below).
2. `GET`s the latest merged snapshot from the server.
3. Writes it into `$CLAUDE_CODE_REMOTE_MEMORY_DIR/projects/<slug>/memory/` (or `~/.claude/projects/<slug>/memory/` when that env var isn't set) before Claude Code loads context — atomically, so a session starting mid-write can never read half a memory file.

A pull that can't reach the server, or a machine with nothing configured, warns on stderr and exits **0**. A hook must not be the reason a session fails to start.

**Load-bearing prerequisite found in Phase 0, not in the original design:** in a remote/cloud session, Claude Code's auto-memory feature is *disabled by default* unless `CLAUDE_CODE_REMOTE_MEMORY_DIR` is set (see `docs/history/phase-0-findings.md` §5). `recall pull` writing files is necessary but not sufficient — that env var has to be set on the remote environment (as a secret, alongside `RECALL_TOKEN`; it can't be baked into committed `settings.json`, whose `env` values don't support `$HOME` expansion) or Claude Code never looks at the memory directory at all.

## Project identity

**Resolved in Phase 0 (see `docs/history/phase-0-findings.md` §6):** Claude Code does *not* scope its own memory storage by git remote — it uses the local filesystem path (git root, or cwd if none) with non-alphanumeric characters replaced by `-`. That's machine-local by construction (a laptop clone and a cloud clone of the same repo get different slugs), which is exactly the gap Recall exists to bridge — so Recall deliberately uses a *different* derivation than Claude Code's own:

- **`project_key`** (server-side, must agree across machines): the git remote's `owner/repo`, taking just the last two path segments so it normalizes identically across SSH (`git@host:owner/repo.git`), HTTPS (`https://host/owner/repo.git`), and locally-proxied remotes that cloud sandboxes rewrite `origin` to. Implemented in `recall_paths::project::key`.
- **local memory directory** (client-side, per-machine): replicates Claude Code's own local-path-slug algorithm exactly, so the hooks read and write the same directory Claude Code itself uses on that machine. Implemented in `recall_paths::claude`. The subtlety: Claude Code's slug is a JavaScript regex replace, which operates on **UTF-16 code units**, so `é` becomes one dash and `🚀` becomes two. Iterating bytes or `chars()` both diverge for any non-ASCII path — and the shell version did exactly that, computing a directory Claude Code never writes to.

`RECALL_PROJECT_KEY` overrides the derivation, trimmed and lowercased into
the same namespace so a declaration on one machine and a derived key on
another are the same string. It covers the cases no derivation reaches: a
repo with no remote (which otherwise falls back to a `local:<path>` key that
differs per machine — the exact split Recall exists to close), sub-projects
of a monorepo that should or should not share one history, a fork that wants
to keep reading the upstream's memory, and the nested-group collision below.

A value that holds whitespace or starts with `global:` is refused and the
derived key stands. Refusing rather than failing keeps a working project
working; the cost is a setting that silently does nothing, so
`ClientConfig::rejected_vars` records the refusal and `recall status` reports
it. The same applies to `RECALL_GLOBAL_KEY`.

An empty value never reaches that machinery: `config::var` filters it to
`None` first, matching what every previous implementation did by construction,
so it reads as unset rather than as refused. That is fine when it came from a
shell and misleading when it came from a settings file, where someone plainly
meant something — so `recall status` reports an empty *declaration* from the
`declared_env` side instead.

Declaring a key is as load-bearing as deriving one, in one direction only:
the server files memory under the key it was pushed with and moves nothing,
so *changing* a declaration strands the old history under the old key.

Known limitation: git hosts with nested groups (e.g. GitLab subgroups) collapse to their last two path segments too, which can collide across different subgroups with the same repo name. `RECALL_PROJECT_KEY` is the way out; the derivation itself is unchanged.

## Scopes: what is synced, under which key

A **scope** pairs a `project_key` on the wire with a subtree of the local
memory directory. There are two:

| Scope | Key | Local subtree |
|---|---|---|
| project | `owner/repo` from the git remote, or `RECALL_PROJECT_KEY` | the memory directory itself |
| global | `global:<RECALL_GLOBAL_KEY>` | `<memory dir>/global/` |

The global scope exists because Claude Code stores facts about *the user*
inside whichever project it happened to learn them in — it even labels them
`type: user` in the file's own front matter — and those should follow the
person, not the repository.

The server learns nothing new from this. A scope key is just another opaque
`project_key`, so the frozen HTTP surface and the SQLite schema are
untouched; `global:your-name` is a project as far as storage is concerned. All the
routing is client-side, in `recall_paths::scope`.

Three rules earn their place:

- **A path under `global/` never falls through to the project scope.** With
  global sync off it is ignored, not absorbed. Pushing someone's personal
  notes into one repository's history is a one-way door.
- **Files are only useful if Claude reads them**, and it reads what
  `MEMORY.md` links. `recall pull` maintains a link per global file, carrying
  each file's own front-matter description as the gloss, because that gloss
  is what the model sees when deciding what to open. See
  [`docs/history/memory-loading-findings.md`](docs/history/memory-loading-findings.md).
- **Getting a note *into* the scope is an explicit act**, not a heuristic.
  `recall promote <file>` moves one note out of the project and into
  `global/`: stored under the global key, tombstoned under the project's,
  moved on disk, and linked from `MEMORY.md`. A move rather than a copy —
  a note in both scopes is pulled twice into every future session of this
  project, and the two copies drift the first time either is edited.
  Recovery drives the ordering: nothing is sent or moved until the store
  succeeds, the new copy is written before the old is removed (so a crash
  leaves it in *both* places, never in neither), and the baseline is
  refreshed last so an unsent tombstone is re-sent by the next push. A
  re-run that finds an identical copy already in `global/` finishes the move
  rather than refusing it. It is also the one thing allowed to delete a line
  of the project's own `MEMORY.md` — the link to the path it just emptied,
  which it pushes, because a line removed only locally comes back with the
  next pull. Implemented in `recall_hooks::promote`.

## Server

Deliberately boring. Two endpoints do the work and three exist to look at it. The full reference — schemas, status codes, worked `curl` examples — is **[`docs/reference/api.md`](docs/reference/api.md)**.

- `POST /sync` — one memory file, or one delete. Runs merge (see below) against the stored version and persists the result. `content` is omitted only when `deleted: true` — see "Deletes are tombstones, not row removal" below.
- `GET /sync?project_key=...` — the current merged set for that project, tombstones included, so a puller can remove local copies.
- `GET /health` — unauthenticated, and the only way a silently-degraded merge becomes visible from outside.
- `GET /admin/stats` and `GET /admin` — read-only. There is deliberately no admin *write* surface, so a leaked token cannot quietly destroy history through it.

Storage: whatever's simplest to self-host and keep running — a single SQLite file behind a small server process is enough for one user's data; don't reach for a distributed database for this. Auth: one bearer token, generated once, stored as an env var on every environment (never committed to the repo).

### Configuration

Every setting is an environment variable, and the authoritative list — name,
default, and what it does — is the table on `recall_server::Config` in the
generated docs:

```sh
cargo doc --workspace --no-deps --open
```

Client-side variables are split by where they are set. The per-machine ones
(`RECALL_URL`, `RECALL_TOKEN`, `RECALL_SOURCE_ENV`, and Claude Code's own
`CLAUDE_CODE_REMOTE_MEMORY_DIR`) are in
[`docs/reference/token-setup.md`](docs/reference/token-setup.md), which also covers the
per-environment network allowlist a claude.ai cloud environment needs before
it can reach a self-hosted server at all. The two that describe a *project*
rather than a machine — `RECALL_PROJECT_KEY` and `RECALL_GLOBAL_KEY` — are in
[`docs/reference/install.md`](docs/reference/install.md).

Where they are *read from* is a second question, and the answer is not
`std::env`. Claude Code applies the `env` block of the settings files in
scope to the processes it spawns, replacing what the shell exported, so the
environment a hook runs under and the one an interactive shell holds are
different things. `recall_hooks::declared_env` layers them in Claude Code's
own order — the user-level `settings.json`, then the project's
`.claude/settings.json`, then its untracked `.claude/settings.local.json`,
all of them above the shell — and every command resolves through it, so a
diagnostic cannot describe a different environment than the one it is
diagnosing. Managed (enterprise) settings and command-line overrides, the two
layers above those, are deliberately not modelled: there is no fleet here to
be managed by, and inventing platform paths nothing can test would trade a
known gap for an unknown one. One more thing it cannot do: the *location* of
the user-level file is read from the process environment, since something has
to be — so `CLAUDE_CONFIG_DIR` declared in a project's settings moves the
memory directory but not the lookup that found that settings file.

### Deletes are tombstones, not row removal

A pushed delete (`{ project_key, file_path, deleted: true, source_env }`)
sets a `deleted` flag on the existing row rather than removing it —
`content` is left untouched by that update, so the last known content is
still sitting in the database even though nothing in the app exposes an
"undo" for it yet. `GET /sync` reports `deleted: true` for that row and
withholds `content` (`null`) so a pull can't accidentally resurrect it.

This existed as a real gap before it was built: the push hook used to
silently no-op when the file it was called about no longer existed,
which meant the server never learned about a delete at all, and a
deleted file would come back on the next pull.

How the client detects a local delete: there is no delete event, so
`recall push` reconciles the memory directory against a baseline
(`.recall-state.json`, kept *beside* the memory directory so it can never be
mistaken for a memory file) on every run that does touch a memory file.
Anything in the baseline that is no longer on disk is pushed as a tombstone.
That makes deletes "eventual" rather than instant — they propagate on the next
memory edit — and it is why a **missing** baseline is treated differently from
an **empty** one: with no baseline at all, an empty memory directory would
read as "everything was deleted" and tombstone the project's whole history.

## Merge strategy

**Implemented in Phase 2 (see `ROADMAP.md`).** Not append-only, not naive last-write-wins. `POST /sync` only attempts a merge when there's actually something to reconcile — an existing, non-tombstoned row whose stored content differs byte-for-byte from the incoming push; a brand-new file, a revived tombstone, or a client re-pushing unchanged content all skip straight to a plain write. When it does attempt one, it shells out to the *local* `claude` CLI (`claude -p`), never the Anthropic API directly, keeping the no-API-key rule in `CLAUDE.md` intact — merge rides whatever account is logged into that CLI on the server host (`claude setup-token`, a one-time interactive step documented in `deploy/README.md`; a real operational requirement, not an afterthought).

The merge prompt instructs the model to preserve every distinct fact from both versions, collapse restated facts to one clear wording, and keep both sides of a genuine contradiction with an inline marker for a human to resolve later — confirmed live to do exactly that, including on a real contradiction (`docs/history/phase-0-findings.md`-style empirical check, not just a read of the prompt). The call runs with a minimal custom system prompt, `--exclude-dynamic-system-prompt-sections`, and `--strict-mcp-config`, in a neutral working directory: confirmed live that skipping all three (i.e. plain `claude -p` from inside a real project directory) balloons a trivial merge call from roughly $0.01 to $0.19 in wasted cache-creation tokens, since the task needs no tools and no project context.

Every failure mode — CLI missing, not logged in, non-zero exit, malformed output, a `RECALL_MERGE_TIMEOUT_MS`-exceeding hang (default 45s) — falls back to last-write-wins rather than rejecting the sync, because a broken or not-yet-configured merge step must never be able to take basic sync down with it. `GET /health`'s `merge` object (`claude_cli.logged_in`, `last_merge_at`, `last_merge_error`) exists specifically so this degraded state is visible from outside instead of silent.

Structured/settings-like data, if Recall ever expands beyond auto memory (it currently shouldn't — see "Explicitly deferred" in `ROADMAP.md`) would use plain deterministic merge; this doesn't apply to the current scope.

## What's deliberately not here

- No client daemon or background watcher — hooks are the entire client.
- No multi-user auth, no OAuth, no billing.
- No Anthropic API key anywhere in the request path.
- No attempt to sync `CLAUDE.md`, skills, or settings — git already does `CLAUDE.md`, and the rest is out of scope (see "Explicitly deferred" in `ROADMAP.md`).
