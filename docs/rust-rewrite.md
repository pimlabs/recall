# Rust rewrite

Recall's third implementation. The owner chose Rust explicitly, as a first
real project in the language — a legitimate reason for a personal tool, and
recorded here as the actual reason rather than dressed up as an engineering
argument, because the engineering case was thin.

## What the case for Rust actually was, honestly

Weak, and worth writing down so nobody re-derives it later as though it
were strong. Of the five bugs the Go port surfaced, **none** would have
been prevented by Rust:

| Bug | Would Rust have caught it? |
|---|---|
| `sed` breaking on paths containing `\|` | No — shell quoting |
| Locale-dependent, byte-wise slug | **No, and Rust is more exposed**: `str` is UTF-8, so matching Claude Code's UTF-16 code-unit semantics takes a deliberate `encode_utf16()` |
| `memory-notes` treated as inside `memory` | No — logic |
| Backup timestamp rendering `000` | No — format string |
| npm shim resolving `$0` through a symlink | No — shell |

They were semantics and integration bugs. Rust's guarantees target memory
safety and data races, which this program does not have and cannot get
much value from: it reads small text files, makes HTTP calls, talks to
SQLite, and spawns one subprocess.

What Rust does genuinely buy here: a smaller binary, `serde` (notably
nicer than hand-rolling JSON key-order preservation, which in Go needed a
third-party surgical-edit library), and `Result`/`Option` being stronger
than Go's error returns.

## What it costs, concretely

**Cross-compilation.** The Go build was `CGO_ENABLED=0` plus a
`GOOS`/`GOARCH` matrix — four platforms, seconds, no C toolchain anywhere.
`rusqlite`'s `bundled` feature compiles the SQLite amalgamation from C, so
that trick is gone. The release workflow now builds every target on a
**native runner** (including arm64 Linux), which is the clean way to pay
that cost rather than fighting cross-toolchains. Docker likewise needs
`musl-dev` in the build stage.

A pure-Rust SQLite exists but is young, and this database is the only copy
of memory that originated in an ephemeral cloud session. Not a place to be
early.

## Shape

A Cargo workspace, one crate per boundary. This isn't ceremony — it's what
let the port run in parallel, since each crate compiles and tests on its
own:

```
crates/recall-wire     the request/response contract + validation, shared
crates/recall-paths    Claude Code path derivation, project_key, config
crates/recall-hooks    hook payloads, state file, settings merge, client, push/pull
crates/recall-server   SQLite store, claude -p merge, axum HTTP
crates/recall-cli      the binary that wires them together
```

`recall-wire` is the load-bearing one. It exists because the rules it holds
— `file_path` validation, the tombstone-versus-empty-file distinction —
were once written twice, in JavaScript on the server and bash on the
client, with nothing keeping them in agreement.

## Frozen surfaces

Unchanged, and not open to tidying:

- **The SQLite schema.** The server opens the existing production database
  file. No migration; rollback is starting the old container against the
  same untouched file.
- **The HTTP API and its JSON**, down to field order and the `null`-versus-
  `""` distinction for tombstoned content.
- **Timestamp format** (`2026-09-03T21:49:55.191Z`) — rows already in the
  database carry it.
- **Environment variable names** — no machine or cloud environment needs
  re-provisioning.
- **The CLI surface** (`serve`, `init`, `status`, `push`, `pull`,
  `version`) — projects have `recall push` committed in their
  `.claude/settings.json`.

## What was retired

`cmd/` and `internal/` — the Go implementation — are gone. It never ran in
production, so keeping it would have meant carrying two dead
implementations rather than one.

`server/index.js` and `hooks/*.sh` stay. Node is what actually serves the
owner's memory today, so it is the real rollback path until the Rust
binary has run in production for a while, and projects already wired to
`$CLAUDE_PROJECT_DIR/hooks/recall-push` keep working unchanged.
