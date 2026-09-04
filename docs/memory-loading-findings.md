# How Claude Code actually loads memory

Empirical, against **CLI 2.1.260**, by planting files and asking a fresh
`claude -p` session what it knows. In the same spirit as
[`phase-0-findings.md`](phase-0-findings.md): what the code assumes, versus
what the installed CLI does.

The probe scripts are in `scripts/probes/`. They cost a real API call each.

## 1. The on-disk layout has not changed

Recall's oldest assumption still holds. A session run with
`CLAUDE_CODE_REMOTE_MEMORY_DIR` pointed at an empty directory created:

```
<dir>/projects/<slug>/memory/MEMORY.md
<dir>/projects/<slug>/memory/user_shell.md
```

Same shape Phase 0 documented on 2.1.42. **No emergency**, and worth stating
because the binary also contains a newer, unrelated `agent-memory` feature
(`~/.claude/agent-memory/<agentType>/` for user scope, `.claude/agent-memory/`
for project, `.claude/agent-memory-local/` for local) which is *not* what
Recall syncs and does not replace it.

## 2. `MEMORY.md` is an index, and it is the entry point

What Claude wrote, unprompted, for a one-line instruction:

```markdown
- [User shell preference](user_shell.md) — prefers fish shell
```

and the topic file it pointed at carried front matter:

```yaml
---
name: user-shell
description: "User's preferred shell is fish"
metadata:
  node_type: memory
  type: user
---
```

Two things follow. Claude Code already classifies memories by type — `type:
user` for a fact about the person, in a directory scoped to one project,
which is precisely the gap the global scope closes. And a file that
`MEMORY.md` does not link came back `UNKNOWN`, so a synced file needs a link
or it may as well not be on disk.

## 3. Subdirectories are fine

A file at `sub/editor.md`, linked from `MEMORY.md`, was read. So was one at
`global/editor.md`. Links resolved both relative to the linking file and
relative to the memory root.

## 4. Retrieval is probabilistic, and that swallowed a lot of time

**The same files and the same question do not always produce the same
answer.** This is the finding that matters most, because it invalidates the
method used to reach the other ones.

Raw counts from this work, each row the same content every run:

| Configuration | Read |
|---|---|
| Linked file at the memory root | 3 of 3 |
| Linked file in a subdirectory | 3 of 3 |
| `MEMORY.md` → `global/INDEX.md` → file | 1 of 5 |
| Cross-project end-to-end, files written by `recall pull` | 0 of 5 |
| The same configuration, hand-written instead | 3 of 3 |

That last pair is the honest one. The two setups were diffed byte for byte:
identical except for one apostrophe in the prose (`The user's` versus `The
user`) and the presence of `.recall-state.json` one directory up. Neither
plausibly affects anything. The split is chance.

Two conclusions were drawn from single runs during this work, and both were
wrong:

| Claimed, from one run | What further runs showed |
|---|---|
| A directory named `global` is special-cased and never read | False. A probe with `global/` and `shared/` in one session read both. |
| An extra hop through an index never resolves | False. It resolved on re-test. |

Retrieval is a model choosing which files to open from their one-line
glosses, not a loader walking a tree. So:

- **A single failed probe proves nothing.** Repeat before concluding, and
  prefer within-session comparisons — two files, one question — over
  comparing separate runs.
- **The gloss carries the weight.** It is what the model sees when deciding
  what to open, so it should describe the content, not the mechanism.
- **Separate what is deterministic from what is not.** Whether the right
  bytes land at the right path under the right key is deterministic, and
  Recall's own tests cover it. Whether Claude opens the file on a given turn
  is not, and no amount of engineering here makes it so.

## What this means for Recall

- Writing files is necessary but not sufficient; `recall pull` also maintains
  the links in `MEMORY.md`. See `recall_hooks::index`.
- Each link carries the file's own front-matter `description`, because that
  is what Claude Code puts there for this purpose.
- `recall status` reports whether the links are present, separately from
  whether the files are — they are different failures.
- The 190 tests verify the deterministic half: routing to the right key, the
  right path on disk, the links maintained in `MEMORY.md`. They cannot verify
  that Claude reads any of it, and should not pretend to.

## Re-running this

```sh
ls scripts/probes/
bash scripts/probes/<name>.sh
```

Each script prints what it planted and what the session answered. Run any of
them more than once before believing the result.
