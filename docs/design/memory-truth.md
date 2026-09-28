# Design: reviewing whether memory is still true

Status: **agreed 2026-09-28**, not built yet. Nothing here describes today's behaviour;
when a part of it is built, [`../reference/`](../reference/) and the
[CHANGELOG](../../CHANGELOG.md) become the authority for it and this file
moves to `history/` as the record of why. It answers the open entry in
[`ROADMAP.md`](../../ROADMAP.md) that begins "Recall moves memory faithfully
and has no idea whether any of it is still true".

## Why

Recall is transport. On 2026-09-22 this project's own memory was read
against the live system. `project_phase1_deploy.md`, synced faithfully for
six weeks and loaded at every session start, announced a server at
`recall.pimlabs.id` behind OrbStack and a Cloudflare Tunnel; production is
Traefik on a VPS at `recall-server.pimlabs.id`. It named `lib.sh` and
`hooks/recall-pull`, both gone since the Rust rewrite, and a trade-off
resolved weeks before. Its last paragraph held the one value nobody could
reason out, `CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/user/.claude`, and nobody
read it, because the heading above it was false. Three of the four files
were mostly right: the errors were a line or two inside notes that
otherwise held.

0.4.5's evaluation reports ([PR 6](part5-plan.md#pr-6-evaluation)) already
cover part of this, from the server side. The worker finds secrets,
duplicate paragraphs, broken `MEMORY.md` links, `type: user` notes in a
project, and notes that name a path or command and have not changed in
`RECALL_EVAL_STALE_DAYS`; with `--contradictions`, one `claude` call per
project for notes that contradict each other. What it cannot do is look at
anything but memory. It has no repository, no `recall doctor`, no view of
which server the machines talk to, so its `stale` finding can say a note is
**old**, never that it is **wrong**. This design is the other half: a
client command that checks what a note *claims* against what the machine
it runs on can see.

## What it reviews: the claim, not the file

A tool that offers "keep or discard" per file discards true things with
false ones. So the unit is the **claim**: one list item, or one sentence
of a paragraph, with the line range it occupies. Fenced blocks are one
claim each. Front matter and `MEMORY.md` are not reviewed; the worker
already checks the index's links.

Extraction is cheap and local. A claim carries **anchors** when it names
something checkable: inline code, a path (`/`, `~/`, `./`, or a word with a
file extension), a hostname or URL, an `UPPER_SNAKE` variable with or
without `=value`, a SemVer version, or a `recall` subcommand or flag.
Claims without anchors reach only layer 3, and only when asked for.

Each claim is then **classified**, before anything judges it:

| Class | Recognised by | Can be `stale`? |
|---|---|---|
| `present` | Present-tense statements of state: "is at", "lives in", "runs on", "deployed via", "is live", an imperative naming an artefact ("run `x`") | Yes |
| `record` | A heading such as History, Previously or Correction; a change verb near the anchor (moved, renamed, replaced, deleted, retired, no longer, used to, until, formerly); the anchor negated ("not `lib.sh`"); a date or version beside a change | **Never** |
| `rule` | Any claim in a `type: feedback` or `type: user` note: the owner's instruction or preference | Only its anchors are checked, never its substance |
| `unsure` | None of the above | Not by layers 1 and 2 |

The heuristic leans toward `record` on purpose. Calling a record present
pushes toward deleting history, which makes memory worse; calling a present
claim a record only misses a stale line, and the worker's age signal still
sees the file. **A claim about the present can go stale; a record of what
changed cannot**, and the review never suggests removing one.

A line the merge marked as a conflict (it writes these inline, in the words
`claude` chose) gets a class of its own, `conflict`, and is reported, not judged.
The ROADMAP's 2026-09-22 correction is why: since 0.3.1 a push names its
`base_sha256`, so a correction sticks, and a conflict mark that is still
there is news in itself.

The report gives every reviewed `present` claim, and every anchor of a
`rule`, one verdict, each with the evidence that decided it:

- **`stale`**: evidence contradicts it.
- **`still_true`**: evidence confirms it. Named, not summarised away: the
  danger is discarding a true line alongside a false one.
- **`cant_tell`**: nothing here can decide it, or it concerns another
  machine. Evidence that was gathered is still shown.

The report never edits anything. An edit is a separate command the owner
runs, finding by finding.

## Layers, cheapest first

### 1. Dead artefact references: a candidate filter

**In:** each claim's path anchors. **Out:** the claims whose referent is
missing, marked `candidate`, and nothing else.

A repository path is looked up in the checkout, a `~/` or absolute path on
this machine; a missing machine path is `cant_tell`, since the claim may be
about another machine. This layer gives **no verdict**. Tried right after
the four files were corrected, it fired three times, all false positives,
because a note that correctly records something as dead has to name it:
"Not the old `hooks/recall-*` scripts, and not `lib.sh`; both were deleted
in the Rust rewrite" is that file's most useful sentence. So a candidate in
a `record` is dropped, one in a `present` claim goes to layer 2 for its
history, and an `unsure` one waits for layer 3. Term matching, tried first,
was rejected: it measures whether a word still appears, not whether a
claim holds.

### 2. Claims against current truth

**In:** every `present` and `rule` claim with anchors, and the sources
below, each read once per run. **Out:** `stale`, `still_true` or
`cant_tell`, with the observed value as evidence.

| Source | How it is read | Claim shapes it decides |
|---|---|---|
| The checkout and its git history | `git ls-files`; `git log --diff-filter=D -1 --format='%h %cs' -- <path>`; `git grep -wF`; `git tag` | A path exists at `HEAD`; it was deleted, in which commit and on what date (which also *confirms* a record); a named script, crate or variable is still referenced by the code; the newest release tag |
| `status::Report`, what `recall doctor --json` is a reading of | In process, never a second collection (doctor's own rule) | Which `RECALL_URL` is in effect and where it comes from; where the credential comes from, never its value; the memory directory; hooks wired; global and machine scopes; whether this is a cloud session |
| `GET /health` on the configured server | The client `recall doctor` already uses | The configured host answers; the commit it runs; merge enabled or degraded; last backup and off-box copy |
| `GET /.well-known/recall` | The same | Server version, protocols, auth methods (is the shared token still accepted), capabilities such as `evaluation` |
| Compose files in the checkout (`deploy/docker-compose*.yml`) | Parsed as YAML: services, images, labels, passed variables | Which ingresses the project supports, service and image names, which variables reach the server. **Not** which file production runs: that lives on the server, out of the client's reach |
| The environment | Names; values only for the non-secret variables `recall status` already prints | `$HOME`, `CLAUDE_CODE_REMOTE`, `CLAUDE_CODE_REMOTE_MEMORY_DIR`, and only for the kind of machine the review runs on |

Two limits are stated rather than papered over. The server does not report
its hostname: "the server is at H" is decided by comparing H with the
configured URL's host and by that host answering, and a named host that is
*not* configured is fetched only with `--probe-hosts` (open decision 3).
And a claim about another kind of machine (a cloud session's `$HOME`, read
from a laptop) is `cant_tell` here, and decided where it applies.

The 2026-09-22 file, reviewed from a cloud session, would read as below
(line numbers and the commit are illustrative):

```text
project_phase1_deploy.md
  stale       L3   live at `recall.pimlabs.id`, deployed via OrbStack + Cloudflare Tunnel
              RECALL_URL (credentials.toml) is https://recall-server.pimlabs.id; /health answers there
  stale       L7   `lib.sh`, `hooks/recall-pull`
              git: both deleted in <sha> (<date>), neither at HEAD
  cant_tell   L9   only runs while this Mac is up; a VPS is the fallback
  still_true  L14  CLAUDE_CODE_REMOTE_MEMORY_DIR=/home/user/.claude
              environment: set to /home/user/.claude; CLAUDE_CODE_REMOTE=true, HOME=/root
```

### 3. The local `claude` CLI, over what survives

**In:** only `unsure` claims, and `present` claims layer 2 left
`cant_tell`, from files changed since the last review; each with its
paragraph for context and a **fact sheet** of what layers 1 and 2 observed.
**Out:** a class and a verdict per claim, each citing a fact-sheet entry.

- **Opt-in** with `--claude`, as `--contradictions` is for `recall eval`:
  it spends the owner's Claude usage.
- **No API key, anywhere.** It runs the `claude` binary under whatever
  `claude login` the machine has, through `recall_worker::merge::Merger::ask`,
  the call merge and the contradiction check already make: no tools, one
  turn, no MCP servers, nothing persisted, a neutral working directory.
  Recall neither reads, sets nor asks for a key.
- **Bounded.** One call per file, at most `--max-calls` calls (default 10),
  each prompt under 64 KiB; what does not fit is listed as skipped, never
  silently cut. Claims and fact sheet pass through the evaluation's secret
  redactor first.
- **Evidence or nothing.** A `stale` or `still_true` verdict that cites no
  fact-sheet entry is recorded as `cant_tell`, with `claude`'s reason
  shown and labelled as its reading, not evidence.
- **No silent writes.** It may suggest an edit, which is checked like any
  other (below).

## The command

```text
recall review run [FILE]... [--all] [--claude] [--max-calls N] [--probe-hosts] [--json]
recall review show [--json]
recall review apply <t-id> [--yes]
```

Shaped like `recall eval`, so the two read as siblings. `run` reviews the
current project's memory, plus the global and machine scopes when they are
on, and prints the report; `show` prints the last one again; `apply` makes
one finding's suggested edit.

**Incremental.** Layers 1 and 2 are cheap (a few `stat` calls, a handful of
`git` runs, two GETs), so they cover every file on every run: the world
moves even when memory does not. Layer 3 runs only on files whose
`content_sha256` changed since the last review (every file, the first
time), or on all of them with `--all`. The state is `.recall-review.json`, beside `.recall-state.json` in
Claude Code's project directory: per project and per machine, outside the
memory directory so it is never pushed, and holding each file's reviewed
hash and the last report.

**Output.** Human text grouped by file, stale first, then `cant_tell`, then
`still_true`, one line of evidence each; records are counted, not listed.
`--json` carries `reviewed_at`, the `evidence` sources read (repository
`HEAD`, server version and commit, and which sources were unavailable and
why), and `claims[]` with `id` (`t1`, `t2`, … so they never collide with a
report's `f<n>`), `file`, `lines`, `class`, `verdict`, `layer`,
`evidence[]`, `eval[]` and an optional `suggested_edit`. Under the
[Versioning rules](../reference/releasing.md#versioning) the `--json`
fields are a contract from their first release. The exit code is `0`
whenever the review ran, whatever it found (open decision 4).

**Acting on a finding.** `recall review apply t3` reuses what `recall eval
apply` does, moved into a module both share: the edit is the wire's
`SuggestedEdit`, refused unless the file is still the `base_sha256` it was
made against, read again after the owner answers, and pushed through
`recall_hooks::push`, which sends the file's own base so the server takes
it as the next edit rather than merging it. Before any of that, an edit is
refused if its range covers a `record` or `still_true` line that the
replacement does not keep verbatim. The suggestion `claude` is asked for
is a rewrite into a record ("Until 2026-09, the server was at X; it is at
Y"), not a deletion.

**Beside the worker's reports, not instead of them.** The review does not
look for secrets, duplicates, dead index links, wrong scope or
contradictions. When this machine can read reports (an admin device, or
`RECALL_TOKEN`), `run` fetches the newest finished one and shows each of
its findings beside the claims whose lines it covers (`eval_7c2kq9 f4`).
A worker `stale` finding says a note is old; the review says whether it is
also wrong, and that finding's reasoning gains a sentence pointing at
`recall review run`.

## Where it runs, and what it never does

**On the client, because the crate split decides it.** `recall-server`
depends on `recall-wire` and, for the merge alone, `recall-worker` without
its client feature; neither reads a repository or `~/.claude`, and the
worker sees memory only as a job hands it over. The repository,
`status::Report` and the environment exist only where `recall` runs. The
redactor and `Merger::ask` move to a part of `recall-worker` its `client`
feature does not gate, so `recall` can use them without the job loop
(open decision 2).

It never:

- runs a model pass on the server, or sends the server anything it does
  not already get: the review is not stored, logged or audited there;
- runs as a hook. A model pass over every note at every session start
  would be slow, costly and would fail quietly, which is the failure
  `recall doctor` exists to end;
- suggests deleting a `record`, or writes anything without `apply`;
- reads a credential's value, or touches production: no SSH, no reading of
  the server's compose file or `deploy/.env`.

**Privacy.** Nothing leaves the machine except requests the client already
makes to its own server, host probes only with `--probe-hosts`,
and, with `--claude`, masked claims handed to the owner's own `claude`
CLI.

## Tests

In the repository's style: each row a property a test pins, and a mutation
that must make it fail.

| Property | Mutation that must fail it |
|---|---|
| `project_phase1_deploy.md` as it stood on 2026-09-22, rebuilt from the ROADMAP's quotes as a fixture under `crates/recall/tests/`, beside a scratch git repository whose history deletes `lib.sh` and `hooks/recall-pull`, gets the verdicts above, line for line | Drop the git-history lookup |
| The corrected versions of the four files get no `stale` claim, and the "Not the old `hooks/recall-*` scripts" sentence is a `record` | Treat a layer 1 candidate as a verdict |
| The memory directory line is `still_true` in a fake cloud environment and `cant_tell` on a fake laptop | Decide environment claims on any machine |
| A `record` is never `stale`, whatever the evidence | Let layer 2 judge records |
| Every `still_true` claim appears in both outputs | Print only problems |
| Layers 1 and 2 make no `claude` call, and none is made without `--claude`, counted by a fake | Call it for `unsure` claims by default |
| A `claude` verdict with no fact-sheet citation becomes `cant_tell` | Trust the verdict |
| A planted token in a note never reaches the fake `claude`'s stdin | Build the prompt unmasked |
| An edit over a `record` or `still_true` line is refused; one made against an older `base_sha256` is refused | Check only the base |
| A file unchanged since the last review gets no layer 3 call, and a changed one does | Key the state on the path rather than the content hash |
| No request goes to a host other than the configured server without `--probe-hosts` | Probe every named host |

## Plan

Each pull request ships and is useful alone.

| # | Pull request | Depends on | Version |
|---|---|---|---|
| 1 | `cli`: `recall review run` and `show`, claim extraction and classification, layers 1 and 2 over the checkout and git, `.recall-review.json`, human and `--json` output, the fixtures | | patch: a new command and a new file |
| 2 | `cli`: layer 2's other sources: `status::Report`, `/health`, discovery, compose files, the environment; `--probe-hosts` | 1 | patch |
| 3 | `worker`: move the redactor and `Merger::ask` out of the `client` gate; no behaviour change | | patch |
| 4 | `cli`: layer 3: `--claude`, `--max-calls`, the fact sheet and the citation rule | 1, 3 | patch: new flags |
| 5 | `cli`: `recall review apply`, sharing `recall eval apply`'s edit path, with the record guard | 4 | patch: a new subcommand |
| 6 | `cli`: show the newest evaluation report's findings beside the claims; `worker`: point its `stale` reasoning at `recall review` | 1 | patch: a new optional `--json` field and human text |

Nothing here is on the breaking list: no command, flag, field, variable,
wire contract or on-disk format that exists today changes.

## Decisions

Decided by the owner on 2026-09-28: every recommendation below was
accepted as written.

1. **Name.** `recall review`, or `recall truth`. **Decided:**
   `review`: it says what the owner does with it, and `truth` promises a
   verdict the design deliberately withholds.
2. **Where the shared code lives.** Ungate part of `recall-worker`, or a
   new small crate for the redactor and the `claude` call.
   **Decided:** ungate. `recall-server` already depends on that
   crate the same way, and a crate per helper is structure without a
   boundary behind it.
3. **Probing hosts named in memory.** **Decided:** only with
   `--probe-hosts`, and only `GET /.well-known/recall`. It would have
   caught `recall.pimlabs.id` now serving the install URL, but a note's
   text should not decide by default where this machine sends requests.
4. **Exit code on stale claims.** **Decided:** `0`. The review
   offers evidence, and `status`, not `doctor`, is its model; a script can
   read `--json`. Revisit if the owner wants it in CI.
5. **Layer 3 by default.** **Decided:** off, behind `--claude`, as
   the contradiction check is. It spends Claude usage, and layers 1 and 2
   already cover the case that started this.
6. **Review state per machine or synced.** **Decided:** per
   machine. What a machine can decide differs (the cloud session decides
   the environment claims), and syncing it would put a second, unmerged
   file on the write path.
