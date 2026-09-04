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

### One correction to the above, found during the port

The port surfaced a **sixth** bug, and this one Rust's types did force into
the open. Go's `json.Marshal` silently replaces invalid UTF-8 with U+FFFD
and returns no error, so a memory file that wasn't valid UTF-8 was pushed
*corrupted* — verified: `caf\xe9\n` marshals to `"caf�\n"`, round-trips
back as `63 61 66 ef bf bd 0a`, and the next pull writes that corruption to
disk. The byte-exactness tests missed it because every case they used was
already valid UTF-8.

Rust cannot do this by accident, because `content` is a `String` and the
bytes have to be converted deliberately. The port makes it an explicit
refusal (`hooks::Error::NotUtf8`) rather than sending anything.

So the honest scorecard is five-to-one, not six-to-nothing — and the one is
a silent data-corruption bug in exactly the property the whole round-trip
test suite exists to protect. That is a better argument for the move than
anything in the original reasoning.

### And one the type system did not catch

A seventh, for symmetry, because it cuts the other way. `PushRequest`
omitted `content` when it was empty — the natural-looking
`skip_serializing_if = "String::is_empty"` in Rust, `json:",omitempty"` in
Go. So pushing an **empty memory file** sent no `content` field at all, and
the Node server answers 400 for that (`typeof undefined !== "string"`). A
project containing one empty note could never sync.

Both implementations had it. Both test suites missed it, and missed it the
same way: every test that covered an empty file used a stand-in server that
accepted anything, so the hole sat precisely where the tests were looking
away. What found it was `scripts/compat-check.sh` pushing a real empty file
at a real Node server.

The fix is to make the distinction explicit in the type —
`content: Option<String>`, where `Some("")` is an empty file and `None` is
a delete. Rust can express that better than Go can, but it did not *force*
it; the first Rust cut made the same mistake. The lesson is the older one:
tests against a stand-in agree with whatever the stand-in was built to
believe.

What Rust does genuinely buy here: a smaller binary, `serde` (notably
nicer than hand-rolling JSON key-order preservation, which in Go needed a
third-party surgical-edit library), and `Result`/`Option` being stronger
than Go's error returns.

### Two more, from an adversarial test pass

Written after the port, when the suite was extended from 113 tests to 159
specifically to hunt for edge cases. Both were live in the Rust
implementation; neither was reachable by any test that existed.

**Eighth: an empty merge result silently wiped memory.** When two versions
of a file conflict, the server asks the local `claude` CLI to merge them
and stores what comes back. If the CLI answered `{"is_error": false,
"result": ""}` — a well-formed success envelope with nothing in it — the
server believed it: it stored `""`, reported `merged: true`, and the
original content was gone from every machine on the next pull. Verified by
putting a stub `claude` on `PATH`. The failure needs a model to return
empty on a valid request, which is rare and entirely possible, and the blast
radius is the one thing the whole service exists to protect.

The fix is `merge::Error::EmptyResult`: an empty result is a merge
*failure*, so the existing conflict path runs and both versions survive.
The one legitimate empty merge — both inputs were already empty — is
allowed through explicitly.

**Ninth: `push` demanded configuration before deciding it had nothing to
do.** `recall push` runs as a `PostToolUse` hook on *every* `Edit` and
`Write`, and the vast majority of those are ordinary source files it should
ignore. It read `RECALL_URL`/`RECALL_TOKEN` first and only then checked
whether the edited path was a memory file — so on a machine where a project
had hooks wired but the environment was not yet set (a fresh clone, a new
laptop, a cloud session), every single edit printed a configuration error.
Reordering the two checks makes the not-my-business case a silent exit
before configuration is ever consulted.

Both were found the same way the seventh was: by testing the real thing
rather than a stand-in — one with a fake `claude` binary, one by running
the actual compiled `recall` under `env_clear()` and asserting on the exit
code and stderr, which the library tests can't see because they call
functions directly.

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
