# Recall documentation

Start at the [README](../README.md) if you just want to know what this is.

Everything here is split by **how it ages**, because that turned out to be
the question people actually need answered:

- **[`reference/`](reference/)** must be true. If it disagrees with the code,
  the document is wrong and should be fixed.
- **[`history/`](history/)** is a record of what was found and decided at a
  point in time. It is not maintained, and it should not be updated to match
  today's code — a finding rewritten after the fact stops being evidence.

An audit in September 2026 found four documents stale in the same way:
written when they were true, never revisited. Nothing about the directory
they sat in said which kind they were, so nobody knew which ones to check.

## Reference — kept true

Two things people arrive wanting, in the order they have to happen. There is
no server-shaped step in the second list and no client-shaped step in the
first, which is the point: the server is a prerequisite, not an alternative.

### Setting up the server

| Document | Read it when |
|---|---|
| [`../deploy/README.md`](../deploy/README.md) | **Start here if you have no server.** Standing it up in Docker behind either ingress (Cloudflare Tunnel or an existing Traefik), enabling merge, backups, and cleaning up a project stored under the wrong key. |
| [`reference/token-setup.md`](reference/token-setup.md) | Generating `RECALL_TOKEN` — the server checks it, every environment sends it. Step 1 belongs to the server; the rest belongs to the machines. |

### Connecting a machine

Your own second machine, or a fresh cloud session. Recall is single-owner by
design, so there is no third party here.

| Document | Read it when |
|---|---|
| [`reference/install.md`](reference/install.md) | Installing the `recall` CLI (npm, Homebrew, curl, cargo), opting a project in with `recall init`, sending memory that predates the install with `recall backfill`, declaring a `project_key` by hand, and promoting a note into the global scope. |
| [`reference/api.md`](reference/api.md) | Talking to the server directly instead: endpoints, schemas, status codes, `curl` examples. |

### Keeping it running

| Document | Read it when |
|---|---|
| [`reference/github-actions-deploy.md`](reference/github-actions-deploy.md) | CI checks on every PR, and deploying each release to a server (automatically, or a chosen version by hand). |
| [`reference/releasing.md`](reference/releasing.md) | Cutting a release across all four install channels — what the tag automates, what only the owner can push, and what to do when a build job never starts. |

`reference/api.md` is the one with teeth: `scripts/api-doc-check.sh` asserts
it against a running server on every CI run, so it cannot quietly drift.

## History — a record, not a manual

| Document | What it records |
|---|---|
| [`history/rust-rewrite.md`](history/rust-rewrite.md) | Why Rust, honestly — what it cost, what it caught, and the bugs this project has actually shipped and fixed. |
| [`history/memory-loading-findings.md`](history/memory-loading-findings.md) | What Claude Code actually does with memory files — the entry point, subdirectories, and why a single failed probe means nothing. |
| [`history/phase-0-findings.md`](history/phase-0-findings.md) | What the Claude Code CLI *actually* does, verified by running it, versus what the original design assumed. Still the source of several load-bearing constraints. |

These are cited from the code: `recall-hooks` points at `phase-0-findings.md`
for why its path derivation looks the way it does. That is the value of not
maintaining them — a comment can point at what was known *then*, and the
answer stays put.

## Design — proposals under discussion

| Document | What it proposes |
|---|---|
| [`design/handshake.md`](design/handshake.md) | Version discovery (`/.well-known/recall`), server identity for release-based deploys, per-device keys in place of the one shared token, a passkey admin page, splitting out `recall-server`, and encrypted storage with a worker that merges and evaluates memory. Not built. |

A proposal is neither kept true nor a record yet. When one is built, the
reference docs become the authority for what shipped and the proposal moves
to `history/`.

## Neither — the shape of the thing

| Document | Read it when |
|---|---|
| [`../ARCHITECTURE.md`](../ARCHITECTURE.md) | Hook wiring, the code map, project-key derivation, merge strategy, and what is deliberately absent. |
| [`../ROADMAP.md`](../ROADMAP.md) | The phased build plan, the evidence behind each phase, and what is explicitly deferred. |

`ARCHITECTURE.md` is reference and must stay true. `ROADMAP.md` is mostly
record — completed phases are not rewritten — with the open questions at the
end being the part that still moves.

Generated API docs for the Rust crates:

```sh
cargo doc --workspace --no-deps --open
```

## Working on it

| Document | Read it when |
|---|---|
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | Dev workflow: building, testing, where a test goes, the checkers that gate a release. |
| [`../CHANGELOG.md`](../CHANGELOG.md) | What changed between releases. Reference, and the one document written for someone who is *already* running Recall. |
| [`../CLAUDE.md`](../CLAUDE.md) | How Claude Code sessions should work in this repo — worktrees, the task list, PR policy, and the ground rules that are not up for negotiation. |
| [`../scripts/probes/README.md`](../scripts/probes/README.md) | Running the probes that establish what Claude Code does with memory files. Each one costs a real API call, and one run proves nothing. |
