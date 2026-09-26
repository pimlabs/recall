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

## One workspace, two binaries

Client and server are built from one Rust workspace, at one version
(`docs/history/rust-rewrite.md`): `recall-server` runs the server; `recall
init` / `status` / `backfill` / `promote` / `push` / `pull` run on a
developer machine. Until 0.4.0 they were one binary, `recall serve` being the
server, and every laptop carried a server it never ran; Part 4 of
`docs/design/handshake.md` has the split. The shared workspace is not
packaging convenience. The validation rules and the tombstone/empty-file distinction
previously existed twice — in JavaScript on the server and in bash on the
client — with nothing keeping them in agreement. The `recall-wire` crate is
now the single definition both halves use, which is also why the workspace
is split by boundary rather than being one crate.

### Code map

Each crate is a boundary, not a folder. They compile and test independently,
and the dependency arrows only ever point downward.

```
recall            the binary: one module per command
   │              init · status · backfill · promote · hook (push/pull)
   │              serve · project
   ├──────────────┐
   ▼              ▼
recall-hooks   recall-server      the two halves
   │              │
   │              ▼
   │          recall-worker       the merge, and (its own binary) the
   │              │               worker that runs it beside the server
   └──────┬───────┘
          ▼
     recall-wire
     the frozen HTTP contract
```

A crate here is a boundary that the compiler enforces, not a folder. The
arrow that matters is the one that is *absent*: `recall-server` lists
`recall-wire`, and `recall-worker` built without its `client` feature for
the merge code alone, so the half that faces the internet cannot reach the
half that reads `~/.claude` — writing `use recall_hooks::…` inside
`recall-server` is `error[E0433]`, not a review comment.

That is also why there is no fifth crate. `recall-paths` used to sit beside
`recall-wire` holding the path derivations, but its only consumers were
`recall-hooks` and the binary — both of which already depend on
`recall-hooks` — and `recall-server` never touched it. It stopped nothing, so
it was a published name for no reason; it is now four modules inside
`recall-hooks` (`claude`, `project`, `scope`, `config`). Folded in v0.2.0,
before the name could acquire dependents.

| Crate | Holds | Why it's separate |
|---|---|---|
| `recall-wire` | Request/response shapes and the validation both sides apply | These rules were once written twice — JavaScript and bash — and drifted. One definition is the whole point. |
| `recall-hooks` | `push`, `pull`, `backfill`, the baseline, the HTTP client, the settings merge — and the derivations they run on: Claude Code's memory paths, `project_key`, scopes, client config | The whole client half. The derivations track *someone else's* implementation, so when the CLI changes there is one place to fix; they live here rather than in their own crate because nothing outside this crate and the binary ever reads them. |
| `recall-worker` | The `claude -p` merge, and the `recall-worker` binary: an enrolled device with no port that takes merge jobs from the server's queue, and evaluate jobs, whose reports on what memory holds it makes from the files the job carries | Moves the `claude` login out of the process the internet reaches. The server uses its merge code, without the HTTP client, while no worker is enrolled. |
| `recall-server` | SQLite store, the merge queue, the axum API, and the `admin` subcommands the API cannot reach | Everything that runs on the host. Never depends on `recall-hooks`. |
| `recall` | Argument parsing and one module per command — `serve` among them, so this is where the server process starts too | Thin. Each command's *failure policy* is documented beside the command it governs. Shipped as `recall-sync` through v0.1.0, because `recall-cli` and `recall` were both taken on crates.io when that preflight ran. |

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

- **`project_key`** (server-side, must agree across machines): the git remote's `owner/repo`, taking just the last two path segments so it normalizes identically across SSH (`git@host:owner/repo.git`), HTTPS (`https://host/owner/repo.git`), and locally-proxied remotes that cloud sandboxes rewrite `origin` to. Implemented in `recall_hooks::project::key`.
- **local memory directory** (client-side, per-machine): replicates Claude Code's own local-path-slug algorithm exactly, so the hooks read and write the same directory Claude Code itself uses on that machine. Implemented in `recall_hooks::claude`. The subtlety: Claude Code's slug is a JavaScript regex replace, which operates on **UTF-16 code units**, so `é` becomes one dash and `🚀` becomes two. Iterating bytes or `chars()` both diverge for any non-ASCII path — and the shell version did exactly that, computing a directory Claude Code never writes to.

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
memory directory. There are three:

| Scope | Key | Local subtree | Travels to |
|---|---|---|---|
| project | `owner/repo` from the git remote, or `RECALL_PROJECT_KEY` | the memory directory itself | anyone syncing that repository |
| global | `global:<RECALL_GLOBAL_KEY>` | `<memory dir>/global/` | every project you sync |
| machine | `machine:<name>` — `[machine] name` in `~/.recall/config.toml`, or `RECALL_MACHINE_KEY` | `<memory dir>/machine/` | only a machine with the same name |

The global scope exists because Claude Code stores facts about *the user*
inside whichever project it happened to learn them in — it even labels them
`type: user` in the file's own front matter — and those should follow the
person, not the repository.

The machine scope exists because some of what it records describes neither:
how much RAM this box has, which of two `dotnet` installs wins here. The
global scope is not a loose fit for that, it is a way of making memory
confidently wrong — "this machine has 8 GB" is false on the next machine, and
that is worse than having no memory of it at all. Both extra scopes are
off unless their variable is set, for different reasons: global because
sharing more than someone expected is rude, machine because a box that has
not said which box it is must not be handed another's facts. That also makes
an ephemeral cloud session right by default — new machine every time, no key,
no machine scope.

The server learns nothing new from this. A scope key is just another opaque
`project_key`, so the frozen HTTP surface and the SQLite schema are
untouched; `global:your-name` is a project as far as storage is concerned. All the
routing is client-side, in `recall_hooks::scope`.

Three rules earn their place:

- **A path under a reserved directory never falls through to the project
  scope.** With that scope off it is ignored, not absorbed. Pushing someone's
  personal notes, or one machine's facts, into a repository's history is a
  one-way door. The reserved names live in one list that both the router and
  the miscased-name guard read, so a fourth scope cannot be added to one and
  forgotten in the other.
- **Files are only useful if Claude reads them**, and it reads what
  `MEMORY.md` links. `recall pull` maintains a link per file in *every*
  reserved directory — the machine scope shipped without this and was inert
  for one release, syncing correctly into a directory nothing pointed at —
  carrying
  each file's own front-matter description as the gloss, because that gloss
  is what the model sees when deciding what to open. See
  [`docs/history/memory-loading-findings.md`](docs/history/memory-loading-findings.md).
- **Getting a note *into* the scope is an explicit act**, not a heuristic.
  `recall promote <file>` moves one note out of the project and into
  `global/` or `machine/` — `--to` picks, defaulting to global: stored under
  that scope's key, tombstoned under the project's,
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
- `GET /admin/stats` and `GET /admin` — read-only. There is deliberately no admin *write* route for memory, on this listener or any other: nothing over HTTP renames, removes or restores a project, so a leaked token cannot quietly destroy history through the server. Renaming, removing and restoring a project are `recall-server admin` subcommands, which open no listener at all; see "Admin commands" below.
- `/v1/devices` and `/v1/authkeys` (0.4.1) — enrolling a machine as a device with its own key, and approving, listing and revoking devices and the authkeys cloud sessions enrol with. They change state about devices only; none of them reads or writes memory.

Storage: whatever's simplest to self-host and keep running — a single SQLite file behind a small server process is enough for one user's data; don't reach for a distributed database for this. Auth: one bearer token, generated once, stored as an env var on every environment (never committed to the repo); and, from 0.4.1, enrolled devices that sign each request with a key of their own, individually revocable (Part 2 of [`docs/design/handshake.md`](docs/design/handshake.md)). The token stays as the legacy path while machines move over.

### Configuration

Every setting is an environment variable, and the authoritative list — name,
default, and what it does — is the table on `recall_server::Config` in the
generated docs:

```sh
cargo doc --workspace --no-deps --open
```

Client-side settings are split by where they are set. The per-machine ones
live in `~/.recall` (`config.toml`: server and machine name;
`credentials.toml`: tokens), written by `recall connect`, with environment
variables (`RECALL_URL`, `RECALL_TOKEN`, `RECALL_SOURCE_ENV`,
`RECALL_MACHINE_KEY`) winning over them where set — which is how a cloud
environment configures them, alongside Claude Code's own
`CLAUDE_CODE_REMOTE_MEMORY_DIR`. Both are in
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

`recall pull` and `recall backfill` are the two commands that leave a baseline
behind. That matters most on a machine that has never had one, which is
exactly the machine `backfill` is for: until the first one is written, a
delete is indistinguishable from a file that was never there.

### Admin commands: a subcommand, not a listener

`recall-server admin list | rename | remove | restore` is how the owner moves,
deletes or puts back what is stored under a project key. It is run inside the
server's container, `docker exec -it -u node recall-server recall-server
admin …`, and the procedure is in [`deploy/README.md`](deploy/README.md).

The absence of an admin write surface on the public listener is a security
property, and the shape was chosen to keep it without qualification. The
obvious design was a second HTTP listener bound to `127.0.0.1`, the way
sqlite-web is bound, reached over an SSH tunnel. A subcommand is stricter and
simpler, so it won:

- **What it takes to reach it.** A loopback listener can be reached by any
  process that can open a socket on that host: another local user, a
  container on host networking, a request forgery in anything else running
  there. So it needs authentication of its own, which is one more credential
  to leak. A subcommand opens no socket. Reaching it takes an exec into the
  container, which is control of the Docker daemon, root-equivalent on the
  host and already enough to open the database file directly. It gives an
  attacker nothing they did not have, and holds no credential to leak;
  `RECALL_TOKEN` plays no part in it.
- **What it adds to the half that faces the internet.** A listener is a
  second router, a second auth path and a second set of request shapes, each
  of which must be kept off the public listener forever, by review. The
  subcommand is code the router cannot call: `recall_server::admin` and the
  store's `admin` module are reached only from `main`, and
  `crates/recall-server/tests/admin.rs` asserts that no route renames, removes
  or restores anything, whatever the credential.
- **What it costs.** A shell on the host, which the hand-written SQL it
  replaced needed too, and nothing that works from a phone. For something run
  rarely, at a moment when something is already wrong, that is the right
  trade.

What each change does, in order, is what the SQL procedure used to ask a
person to remember: name the keys exactly (never a prefix or a pattern) and
confirm them by typing each back, a rename's target included, or by `--yes`,
which prints them instead; take a backup with `Store::backup` into
`backups/admin/`, which the server's rotation never prunes, and read it back
to check it holds the rows shown; then one transaction, which reads the rows
again, refuses if they differ from what was shown, closes the merge jobs
still open for those rows (a worker's late result would otherwise land on a
row the change moved, removed or replaced), appends one audit leaf as the
`host`, through the same catch-up `reset-passkeys` uses so a running server
and the command write one tree, and commits only if `changes()` equals the
number of rows and jobs it named. `--dry-run` opens the
database read-only and stops before the backup. A rename refuses a target key
that holds any rows at all, because the primary key is `(project_key,
file_path)` and folding two projects together has no single right answer for
the files both have; the refusal points to the safe fold in
`deploy/README.md` (copy the machine's memory directory aside, rename the old
key to an archive key, reconcile by hand), not to "point the machine at the
new key and let it push", whose first pull overwrites that machine's files. A
restore never overwrites a differing live row without `--overwrite`, never
removes a row, and never turns a live file into a tombstone (a deletion on
every machine at its next pull) without `--restore-deletions` as well.

It runs beside a live server, not instead of one. The store keeps the file
in SQLite's WAL mode (see "Storage: WAL, synced at every commit" below),
and both sides wait on a lock for the same 5 seconds, which each states
explicitly rather than inheriting rusqlite's default. Every write, the
server's and a change's, is one transaction that takes the write lock
before it reads anything: a single statement does by itself, and anything
longer (every audited write, and a change) begins with `BEGIN IMMEDIATE`.
So a change waits behind a write in flight, a write waits behind it, and
neither can deadlock the other. Under WAL only another writer can hold a
change up; a reader, such as sqlite-web mid-page or a backup being read, no
longer can. A lock that never comes rolls the change back and says so.

What the lock does not do is order a change against a whole push. A push is
several statements: the handler reads the stored row, may merge for up to
`RECALL_MERGE_TIMEOUT_MS` holding no lock, then writes. A push that read
before a change committed writes after it, and partly undoes it: a row comes
back under the key a rename or remove emptied, or a merge of the replaced
version lands over a restored row. The admin side cannot prevent that
without changing the push path, which moving merge behind a queue (Part 5 of
`docs/design/handshake.md`) is set to change anyway. So after
committing, the command waits out the merge window (the server's timeout,
read from the same environment the same way, plus a second) and checks; if
anything came back it names the paths and the command to run, and exits 3.

The commands leave the journal mode alone, and do not check it. The modes
that cannot roll back, `off` and `memory`, are settings of the connection
that asks for them and are never stored in the file, so the admin
connection always gets the file's own: WAL, or the rollback journal of a
file no current server has opened yet, and both roll back. The commands
refuse to write as any user but the database file's owner, since `docker
exec` defaults to root and a root-owned journal left by a crash is one the
server cannot open; a process that cannot tell who it runs as refuses too.

### Storage: WAL, synced at every commit

`Store::open` puts the file in SQLite's WAL mode, once, with
`synchronous=FULL` on its connection. WAL for two reasons: a commit appends
to `recall.db-wal` and syncs that one file, where the rollback journal
synced a journal, then the database, then deleted the journal, and since
the audit log every push and every pull is a write transaction; and readers
(sqlite-web, the admin commands' reads, a backup being taken) stop holding
up writers. FULL rather than WAL's customary NORMAL, because NORMAL does
not sync a commit until the next checkpoint, so a power cut or a host crash
can take back pushes whose client already got `200`, and that client may
be an ephemeral cloud session whose memory this server was the only copy
of. The syncing is most of what a push costs, and the price is paid
deliberately: the numbers are beside `use_durable_wal` in `store.rs`.

What WAL asks in return is that `recall.db` is no longer the whole database
while the server runs, or after a crash: the newest commits can be only in
`recall.db-wal`. So nothing copies the file. The server's snapshots and
every admin change's backup are `VACUUM INTO`, which reads through SQLite
and writes one self-contained file in the rollback journal's mode, and
`crates/recall-server/tests/edge_cases.rs` holds a test that a backup
taken while pushes land restores every push acknowledged before it, which
a file copy fails. A restore moves `recall.db`, `recall.db-wal` and
`recall.db-shm` aside together, because SQLite replays a WAL it finds into
the file beside it, whichever file that is; `deploy/README.md` has the
procedures, and why the volume has to be a local filesystem, and
`scripts/restore-check.sh` runs them, as the README has them, against a
real server. The server
checkpoints every sweep, so the file alone is never far behind, and empties
the WAL into the file when it stops; neither is relied on for correctness.

Two things WAL took away are put back by hand. Under the rollback journal a
connection noticed a `recall.db` replaced beneath it by the file's change
counter; under WAL it goes by its WAL index and its cache, and writes on
into the file it opened, moved aside or not, answering 200. So the store
records the (device, inode) it opened, and every audited write first checks
the path still names it, refusing (a 500) if not; a file overwritten in
place is still not seen, and the restore never does that. And the bundled
SQLite is at least 3.51.3, the first with the fix for a race between a
checkpoint on one connection and a write that starts the WAL over on
another, which the server and an admin command running beside it are.

## Merge strategy

**Implemented in Phase 2 (see `ROADMAP.md`).** Not append-only, not naive last-write-wins. `POST /sync` only attempts a merge when there's actually something to reconcile — an existing, non-tombstoned row whose stored content differs byte-for-byte from the incoming push, **and** is not the version the push names as its base (`base_sha256`: the content the client last pulled or pushed). A brand-new file, a revived tombstone, a client re-pushing unchanged content, and the next edit of the stored version all skip straight to a plain write. The base is load-bearing: the merge keeps every distinct fact from both versions, so it cannot express a deletion, and before 0.3.1 — when every differing push was merged — a line deleted on purpose, or a resolved `CONFLICT` marker, came back on the next push. When it does attempt one, it shells out to the *local* `claude` CLI (`claude -p`), never the Anthropic API directly, keeping the no-API-key rule in `CLAUDE.md` intact — merge rides whatever account is logged into that CLI on the server host (`claude setup-token`, a one-time interactive step documented in `deploy/README.md`; a real operational requirement, not an afterthought).

The merge prompt instructs the model to preserve every distinct fact from both versions, collapse restated facts to one clear wording, and keep both sides of a genuine contradiction with an inline marker for a human to resolve later — confirmed live to do exactly that, including on a real contradiction (`docs/history/phase-0-findings.md`-style empirical check, not just a read of the prompt). The call runs with a minimal custom system prompt, `--exclude-dynamic-system-prompt-sections`, and `--strict-mcp-config`, in a neutral working directory: confirmed live that skipping all three (i.e. plain `claude -p` from inside a real project directory) balloons a trivial merge call from roughly $0.01 to $0.19 in wasted cache-creation tokens, since the task needs no tools and no project context.

Every failure mode — CLI missing, not logged in, non-zero exit, malformed output, a `RECALL_MERGE_TIMEOUT_MS`-exceeding hang (default 45s) — falls back to last-write-wins rather than rejecting the sync, because a broken or not-yet-configured merge step must never be able to take basic sync down with it. `GET /health`'s `merge` object (`claude_cli.logged_in`, `last_merge_at`, `last_merge_error`) exists specifically so this degraded state is visible from outside instead of silent.

Structured/settings-like data, if Recall ever expands beyond auto memory (it currently shouldn't — see "Explicitly deferred" in `ROADMAP.md`) would use plain deterministic merge; this doesn't apply to the current scope.

## What's deliberately not here

- No client daemon or background watcher — the hooks are the only part of
  the client that runs unprompted.
- No multi-user auth, no OAuth, no billing.
- No Anthropic API key anywhere in the request path.
- No attempt to sync `CLAUDE.md`, skills, or settings — git already does `CLAUDE.md`, and the rest is out of scope (see "Explicitly deferred" in `ROADMAP.md`).
