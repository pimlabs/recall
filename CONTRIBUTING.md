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
cargo build --release -p recall-cli    # binary at target/release/recall
```

The first build is slow — `rusqlite` compiles SQLite from C. After that it's cached.

`cargo test -p recall-wire` is the fastest useful check: that crate holds the request/response contract both halves depend on, and its tests assert byte-for-byte equality with what the deployed Node server produces.

## Ground rules

See `CLAUDE.md`'s "Ground rules" section — same reason, one source of truth. Touching any of them? Stop and confirm with the user first.

Two more that aren't in `CLAUDE.md` because they're about this code rather than the project's shape, and both would look like harmless cleanups:

- **The SQLite schema, the HTTP JSON, the timestamp format, and the env var names are frozen.** The deployed Node server wrote the rows currently in production and speaks that JSON. See `docs/rust-rewrite.md`.
- **`recall-paths`'s `slug()` must stay UTF-16-based.** It reproduces a JavaScript regex replace inside Claude Code, which operates on UTF-16 code units. Iterating bytes or `chars()` instead is wrong for any non-ASCII path, and wrong here means silently reading and writing a directory Claude Code never touches.

## Commit messages

State the why, not just the what.
