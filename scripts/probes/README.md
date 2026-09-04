# Probes

Scripts that establish what Claude Code actually does with memory files, by
running it rather than reading its source — the CLI is a compiled binary now,
so this is the only way to know.

Each one plants files, starts a real session, and prints what came back.
**Each costs an API call**, and they are not part of `cargo test` for that
reason.

| Script | Question it answers |
|---|---|
| `linked-file-is-read.sh` | Is a memory directory Claude Code did not create itself read at all? |
| `subdirectory-is-read.sh` | Is a linked file in a subdirectory read? |
| `global-is-not-special.sh` | Is a directory named `global` treated differently from any other? Both in one session, so the comparison is fair. |
| `retrieval-is-probabilistic.sh` | Does the same input always give the same answer? |
| `cross-project-end-to-end.sh` | Does a global memory pushed from one project reach a *different* project on a *different* machine, and does Claude read it there? Needs a `recall` binary: pass its path. |

## Read the results carefully

Retrieval is a model deciding which files to open, not a loader walking a
tree. **A single `UNKNOWN` proves nothing** — one configuration here returned
`UNKNOWN` four times and the correct answer on the fifth run, unchanged. Two
findings during this work were drawn from single runs and were both wrong;
see `docs/memory-loading-findings.md` §4.

Run anything more than once before believing it.
