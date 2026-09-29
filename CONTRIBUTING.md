# Contributing

Solo/personal project; light process to keep the history useful.

## Layout

See `README.md`'s "Project docs" table for what each file/directory is for — kept in one place so it doesn't drift out of sync with a second copy here.

## Working on the code

A Cargo workspace; every crate builds and tests on its own.

```sh
cargo test --workspace                 # every test CI runs
cargo test -p recall-hooks             # just one crate, much faster
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --release -p recall    # the client, at target/release/recall
cargo build --release -p recall-server   # the server, beside it
```

The first build is slow — `rusqlite` compiles SQLite from C. After that it's cached.

CI's Linux job runs the same tests with [cargo-nextest](https://nexte.st) (`cargo nextest run --workspace --profile ci`, then `cargo test --doc --workspace` for the doctests nextest does not run), which puts every test in a process of its own and spreads them over every core; the Windows jobs keep `cargo test`, since Windows starts processes too slowly for that to pay. Either way runs every test once; nextest is only faster. Its settings are in `.config/nextest.toml`.

`cargo test -p recall-wire` is the fastest useful check: that crate holds the request/response contract both halves depend on, and its tests pin the wire format byte for byte — field order and the `null`-versus-`""` distinction included — because those were once compatibility guarantees against a second implementation and are now guarantees against the rows already in production.

### The CLI surface

One module per command in `crates/recall/src/`, `pub async fn run(...) ->
anyhow::Result<i32>` returning an `exit::*` constant, a `///` line on the
`Cmd` variant in the imperative mood, and a `//!` header on the module stating
**how loudly that command is allowed to fail**. The failure policy lives beside
the command it governs, not in a table somewhere.

That `///` line is the command's summary in the lists and in `-h`: one line,
at most 60 characters, no full stop. Anything longer goes in a paragraph
after it, which `--help` adds. Tests in `main.rs` hold the summary to that
length and every line of every help to 100 columns.

Three conventions are settled and worth not relitigating:

- **`--help`, `-h` and `help` all work, and so does `help <command>` and
  `<command> --help`.** clap gives all five; the tests assert them because
  "clap gives it to you" stops being true the moment someone reaches for
  `disable_help_subcommand` to tidy the command list.
- **`--version`, `-V` and `version` all work and print the same bytes.** The
  subcommand is the older surface and has been the CLI since v0.1.0, so it
  cannot be dropped; the flag is what fingers type and scripts reach for, so
  it cannot be missing. Both go through `version_line()` in `main.rs` and a
  test pins the three together rather than each to a literal — two ways of
  asking one question that give different answers is a bug this project has
  had more than once. Every help prints the same line first, from the same
  function.
- **A bare `recall` prints help and exits non-zero.** It is a question, not an
  instruction, and answering with silence and success would be wrong twice.

The version line's `recall <version>` prefix is a contract, not a format
choice: `scripts/release.sh` matches on it to confirm the binary it built is
the one being tagged.

Adding a command means adding it to `COMMANDS` in
`crates/recall/tests/cli.rs`, which is what makes the help tests notice a
command missing from the help, and naming it in one of `GROUPS` in
`main.rs`, which is where the top-level help lists it. A command in no group
is listed under "Other" rather than left out.

### Where a test goes

Three places, and the choice is not taste:

- **Beside the code, in `src/`.** Use this when the test needs something
  private: an internal invariant, an error path reachable only by building
  internal state, a helper nobody outside the crate should see. Most of the
  suite lives here.
- **In the crate's `tests/`.** Use this when *the point* of the test is that
  the public API is enough. These compile against the crate as a dependency,
  so anything they can reach, a user can reach.
- **In a doc comment.** An example that would be worth reading anyway;
  `cargo test` runs it, so it cannot drift from the code it documents.

The tell, and the only rule that really matters: **if putting a test in
`tests/` would make you mark something `pub` that nobody outside the crate
needs, it belongs beside the code instead.** A public surface widened for a
test is still a public surface, and `missing_docs` will then ask you to
document, and therefore commit to, something you never meant to expose.

That is why the split across crates looks uneven — `recall-wire` and
`recall-hooks` are entirely in-crate, while `recall-server` and `recall`
also have a `tests/`. It follows what each crate is: the first two are
libraries whose interesting behaviour is internal, and the last two are
asked whether their outside edge — an HTTP surface, a command-line one —
behaves for someone who only has the outside.

`recall-wire`'s one `tests/` file is that same question asked of the wire:
`tests/golden.rs` reads what every released version actually sent, kept in
`crates/recall-wire/fixtures/wire/`, with nothing but the public types. A
field renamed or dropped fails there before it reaches a client in the
field. Add a directory there when a release changes a shape; the README in
it says how.

Note that `tests/` means **`crates/<name>/tests/`**. Cargo does not compile a
`tests/` at the root of a workspace; a directory there would silently never
run. `fixtures/` at the root holds data, not tests, and says so in its own
README.

### Documentation is checked, not just written

```sh
cargo doc --workspace --no-deps --open        # read it
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps   # what CI runs
```

`missing_docs` is denied in every library crate, so an undocumented public item fails the build; the rustdoc run above catches the other half, a doc link pointing at something that no longer exists.

`docs/reference/api.md` is checked the same way — not by review, but by assertion:

```sh
cargo build --release -p recall-server
./scripts/api-doc-check.sh target/release/recall-server
```

142 checks against a real server on a real socket: every status code, error string, field order and `null`-versus-`""` claim the document makes. Change a handler without changing the doc and this fails, which is the point.

### Links between documents

```sh
python3 scripts/link-check.py
```

Every relative Markdown link in the tree, resolved against the filesystem. No
fixed number: it counts whatever is there, which is why it is the one checker
with no count quoted here to fall out of date.

It deliberately does not verify anchors (that would mean reimplementing
GitHub's heading slugs, and getting that subtly wrong is worse than not
claiming it), does not fetch external URLs (a link rotting on someone else's
schedule should not turn this build red), and ignores fenced blocks — several
documents quote what Claude writes into a memory file, and those samples link
to files in someone's memory directory rather than in this repository.

### The rate-limit bucket is a security boundary

```sh
./scripts/trusted-ip-check.sh target/release/recall-server
```

Nine checks on a real socket. Rate limiting runs *before* auth so a flood of
invalid tokens is limited too — which means a client that can choose its own
bucket has unlimited attempts at guessing the token. `RECALL_TRUSTED_IP_HEADER`
names the one header the ingress sets; everything else a client might send is
ignored, and the compose files keep the origin unreachable except through that
ingress. Change any of those three and this fails.

### The database procedures are run, not read

```sh
./scripts/restore-check.sh target/release/recall-server
```

Fifty-one checks. The restore, the backups taken by hand and the switch
back to the rollback journal in `deploy/README.md` are extracted from the
README and run as written against a real server, through a stand-in
`docker` on PATH (no daemon needed): a restore with the server running and
after a crash, one naming a snapshot that is not there, one whose snapshot
is empty or not SQLite, one on a stack whose volume has another name, one
whose stop stopped nothing. Under WAL the mistakes in these are silent, and
the database is the only copy, so change a block and this runs it. It needs
python3 with PyYAML, or the `docker compose` CLI in its place, to read the
compose files, and says so if it has neither; `release.sh` asks before its
build.

### Before touching anything frozen

```sh
cargo build --release
./scripts/compat-check.sh target/release/recall-server
```

Nineteen checks against `fixtures/node-written.db` — a database the retired Node server actually wrote, kept because production's rows were written by it. This server opens that file, serves every row correctly (byte-exact content, no/one/two trailing newlines, an empty file, unicode, a nested path, a tombstone with its content still withheld, project isolation), and then keeps writing to it.

It has caught two bugs that every test suite in the repo missed, both times because it used the real thing where the tests used a stand-in. Run it before a production cutover, and again after.

### Probes cost real money

`scripts/probes/` establishes what Claude Code actually does with memory
files by running it, not by reading it. **Each probe is a real API call**,
which is why none of them is part of `cargo test`. Retrieval is
probabilistic, so a single run proves nothing — see
`scripts/probes/README.md` and `docs/history/memory-loading-findings.md` before
drawing a conclusion from one.

### Actions are pinned to commits

Every `uses:` under `.github/` names a full commit SHA, with the version it
is as a comment:

```yaml
- uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09 # v5.1.0
```

A tag such as `@v5` can be moved by whoever owns the action, and the moved
tag then runs with this repository's secrets, the deploy key among them. A
SHA cannot move. To add an action, look up the commit its release tag
points to (`git ls-remote --tags https://github.com/<owner>/<repo>`, the
`^{}` line for an annotated tag) and write both. `check-scripts` refuses a
tag (`scripts/actions-pin-check.py`), and `.github/dependabot.yml` opens a
weekly `deps:` pull request that moves the SHAs and their comments
together. `dtolnay/rust-toolchain` is the exception Dependabot leaves
alone: it is pinned to its `stable` branch, which has no releases, and is
moved by hand.

## Ground rules

See `CLAUDE.md`'s "Ground rules" section — same reason, one source of truth. Touching any of them? Stop and confirm with the user first.

Two more that aren't in `CLAUDE.md` because they're about this code rather than the project's shape, and both would look like harmless cleanups:

- **The SQLite schema, the HTTP JSON, the timestamp format, and the env var names are frozen.** The rows in production were written by the Node server this one replaced, and every environment already has the variables provisioned. Nothing here is style. See `docs/history/rust-rewrite.md`.
- **`recall-hooks`'s `claude::slug()` must stay UTF-16-based.** It reproduces a JavaScript regex replace inside Claude Code, which operates on UTF-16 code units. Iterating bytes or `chars()` instead is wrong for any non-ASCII path, and wrong here means silently reading and writing a directory Claude Code never touches.

## Releasing

```sh
./scripts/release.sh v0.1.0 --dry-run
```

Runs everything above plus both real-server checkers, verifies the three
version fields agree, and then asks before each irreversible step. See
`docs/reference/releasing.md` for what only the owner can push, and why.

## Commit messages

State the why, not just the what.

The title is `area: what it does`, the way tokio and Go write theirs:

```
ci: run every job on public pull requests, add ci-passed (#141)
deps: rustls 0.23.45 for RUSTSEC-2026-0285 (#136)
release: 0.4.6 (#139)
```

- **One area, lowercase, from this list:** `server` (`recall-server`),
  `worker`, `cli` (the `recall` binary), `hooks`, `wire`, `admin` (the
  `/admin` page), `ci`, `release`, `deploy`, `install` (`install.sh`,
  `install.ps1`, npm, the formula), `deps`, `docs`. A change that spans
  several takes the one a reader would look under first. Add an area here
  before using a new one.
- **After the colon:** a short sentence in the imperative, lowercase first
  word (`add`, `fix`, `keep`, `rename`), no full stop.
- **At most about 72 characters**, the squash merge's ` (#N)` included:
  that is where GitHub's lists and `git log --oneline` cut it off. What
  does not fit goes in the body.
- **Releases always read the same:** `release: 0.4.6` for the bump, and
  `release: formula for 0.4.6` for the follow-up after it.
- **No prefix carries a version meaning.** This is not Conventional
  Commits: whether a change is a patch or breaking is decided by
  `docs/reference/releasing.md` and stated in the pull request, never by
  the prefix.

The squash merge takes the pull request's title, so the title is where
this is applied. Commits before #141 used no single scheme; they are left
as they are.
