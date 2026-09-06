# Contributing

Solo/personal project; light process to keep the history useful.

## Layout

See `README.md`'s "Project docs" table for what each file/directory is for — kept in one place so it doesn't drift out of sync with a second copy here.

## Working on the code

A Cargo workspace; every crate builds and tests on its own.

```sh
cargo test --workspace                 # what CI runs
cargo test -p recall-hooks             # just one crate, much faster
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --release -p recall-sync    # binary at target/release/recall
```

The first build is slow — `rusqlite` compiles SQLite from C. After that it's cached.

`cargo test -p recall-wire` is the fastest useful check: that crate holds the request/response contract both halves depend on, and its tests assert byte-for-byte equality with what the deployed Node server produces.

### Documentation is checked, not just written

```sh
cargo doc --workspace --no-deps --open        # read it
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps   # what CI runs
```

`missing_docs` is denied in every library crate, so an undocumented public item fails the build; the rustdoc run above catches the other half, a doc link pointing at something that no longer exists.

`docs/api.md` is checked the same way — not by review, but by assertion:

```sh
cargo build --release -p recall-sync
./scripts/api-doc-check.sh target/release/recall
```

27 checks against a real server on a real socket: every status code, error string, field order and `null`-versus-`""` claim the document makes. Change a handler without changing the doc and this fails, which is the point.

### The rate-limit bucket is a security boundary

```sh
./scripts/trusted-ip-check.sh target/release/recall
```

Nine checks on a real socket. Rate limiting runs *before* auth so a flood of
invalid tokens is limited too — which means a client that can choose its own
bucket has unlimited attempts at guessing the token. `RECALL_TRUSTED_IP_HEADER`
names the one header the ingress sets; everything else a client might send is
ignored, and the compose files keep the origin unreachable except through that
ingress. Change any of those three and this fails.

### Before touching anything frozen

```sh
cargo build --release
./scripts/compat-check.sh target/release/recall
```

Eleven checks across the mixed fleet: this server opening a database the Node server wrote, the old shell hooks against this server, this client against the Node server, and byte-exact round trips. It has caught two bugs that every test suite in the repo missed, both times because it used the real thing where the tests used a stand-in. Run it before a production cutover, and again after.

## Ground rules

See `CLAUDE.md`'s "Ground rules" section — same reason, one source of truth. Touching any of them? Stop and confirm with the user first.

Two more that aren't in `CLAUDE.md` because they're about this code rather than the project's shape, and both would look like harmless cleanups:

- **The SQLite schema, the HTTP JSON, the timestamp format, and the env var names are frozen.** The deployed Node server wrote the rows currently in production and speaks that JSON. See `docs/rust-rewrite.md`.
- **`recall-paths`'s `slug()` must stay UTF-16-based.** It reproduces a JavaScript regex replace inside Claude Code, which operates on UTF-16 code units. Iterating bytes or `chars()` instead is wrong for any non-ASCII path, and wrong here means silently reading and writing a directory Claude Code never touches.

## Releasing

```sh
./scripts/release.sh v0.1.0 --dry-run
```

Runs everything above plus both real-server checkers, verifies the three
version fields agree, and then asks before each irreversible step. See
`docs/releasing.md` for what only the owner can push, and why.

## Commit messages

State the why, not just the what.
