# Recall documentation

Start at the [README](../README.md) if you just want to know what this is.
Everything else is here, grouped by what you're trying to do.

## Using it

| Document | Read it when |
|---|---|
| [`install.md`](install.md) | Installing the `recall` CLI (npm, Homebrew, curl, cargo) and opting a project in with `recall init`. |
| [`token-setup.md`](token-setup.md) | Generating `RECALL_TOKEN` and installing it on every environment — including the extra variable a claude.ai cloud session needs. |
| [`api.md`](api.md) | Talking to the server directly: endpoints, schemas, status codes, `curl` examples. |

## Running it

| Document | Read it when |
|---|---|
| [`../deploy/README.md`](../deploy/README.md) | Standing the server up (OrbStack + Cloudflare Tunnel), including enabling merge. |
| [`github-actions-deploy.md`](github-actions-deploy.md) | Wiring CI checks on every PR and auto-deploy to a VPS on push to `main`. |
| [`releasing.md`](releasing.md) | Cutting a release across all four install channels — what the tag automates, and what only the owner can push. |

## Understanding it

| Document | Read it when |
|---|---|
| [`../ARCHITECTURE.md`](../ARCHITECTURE.md) | Hook wiring, the code map, project-key derivation, merge strategy, and what is deliberately absent. |
| [`rust-rewrite.md`](rust-rewrite.md) | Why Rust, honestly — what it cost, what it caught, and the nine bugs this project has actually shipped and fixed. |
| [`phase-0-findings.md`](phase-0-findings.md) | What the Claude Code CLI *actually* does, verified by running it, versus what the original design assumed. Still the source of several load-bearing constraints. |
| [`../ROADMAP.md`](../ROADMAP.md) | The phased build plan, the evidence behind each phase, and what is explicitly deferred. |

Generated API docs for the Rust crates:

```sh
cargo doc --workspace --no-deps --open
```

## Working on it

| Document | Read it when |
|---|---|
| [`../CONTRIBUTING.md`](../CONTRIBUTING.md) | Dev workflow: building, testing, the compatibility matrix. |
| [`../CLAUDE.md`](../CLAUDE.md) | How Claude Code sessions should work in this repo — worktrees, the task list, PR policy, and the ground rules that are not up for negotiation. |

## Historical

| Document | What it is |
|---|---|
| [`go-rewrite-design.md`](go-rewrite-design.md) | The Go port that preceded the Rust one. Removed from the tree; the staged-migration reasoning in it is still what's in use. |
