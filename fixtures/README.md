# Fixtures

One file, and it is not replaceable: a SQLite database the **Node** server
actually wrote, captured before Phase 7 deleted that implementation from the
tree.

It matters because production's rows were written by that server. The schema
is frozen, and the only honest way to prove this implementation can still read
what the old one wrote is to hand it a real database rather than one this code
produced itself. `scripts/compat-check.sh` does exactly that, nineteen times,
and it has caught two bugs no unit test in the repo caught — both times
because it used the real thing where the tests used a stand-in.

`.gitignore` excludes `*.db` everywhere and then re-includes this one by name.
That is deliberate; don't "fix" it.

## Why this directory is not called `tests/`

It was, until v0.1.0. Cargo looks for `tests/` **inside each crate**, so a
`tests/` at the root of a workspace is compiled by nothing at all. The name
promised a thing that could never happen, and the first person to put a `.rs`
file here would have watched it silently never run.

Rust tests live in the crates. This holds data.
