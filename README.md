# Recall

A minimal, self-hosted sync service for Claude Code's **auto memory** — the notes Claude writes about itself as it works (`~/.claude/projects/<project>/memory/`). Works from your laptop, your other laptop, and any fresh ephemeral cloud session, with no device pairing required.

## Why

Claude Code's auto memory is explicitly machine-local by design (see Anthropic's own docs — and a related feature request on `anthropics/claude-code` closed as "not planned"). `CLAUDE.md` already syncs fine via git; auto memory doesn't. Existing community tools (`claude-sync`, `claude-brain`, and similar) all assume a fixed set of named "devices" that pair with each other — that model breaks the moment one of your environments is an ephemeral cloud session that's never seen your other machines and won't exist tomorrow.

Recall exists for that specific gap: **a central service any environment can talk to, with no prior introduction.**

## What it is not

- Not a replacement for git-based `CLAUDE.md` sync — that's already solved, don't touch it.
- Not a multi-user product. Personal tool, single owner, no Anthropic API key, no auth system beyond a personal token. See `CLAUDE.md`'s Ground rules.
- Not append-only. Merge works like `claude-brain`'s does: a semantic merge via the local `claude` CLI, not naive line-dedup — implemented and verified live, see `ROADMAP.md` Phase 2.

## How it plugs into Claude Code

No custom client daemon. Claude Code's own hook system does the work:

- **Push**: a `PostToolUse` hook matching `Edit|Write` runs `recall push`, which checks whether the edited file is a memory file and, if so, sends it to Recall's API. (An earlier design assumed a `FileChanged` event and a declarative `http` hook type — neither exists in the installed CLI; see `docs/phase-0-findings.md`.)
- **Pull**: a `SessionStart` hook runs `recall pull`, which fetches the latest synced state before Claude loads context.

Both hooks live in the **project's own `.claude/settings.json`**, checked into git — so any environment that clones the repo (laptop or fresh cloud session) picks up sync automatically. See `ARCHITECTURE.md`.

## Quick start

Recall is a single Rust binary — the same artifact runs the server and the client. Install once per machine (npm, Homebrew, or curl — see `docs/install.md`):

```sh
npm install -g @pimlabs/recall
```

Set `RECALL_URL` and `RECALL_TOKEN` in your shell profile (`docs/token-setup.md`), then, in each project you want synced:

```sh
recall init                                  # wires .claude/settings.json
git add .claude/settings.json && git commit  # so fresh clones get it too
recall status                                # confirm it's actually working
```

Standing up the server itself is `recall serve`, in practice via `deploy/`.

## Status

Phases 0 through 4 done, and Recall is now a single Rust binary (`docs/rust-rewrite.md`): deployed for real behind a Cloudflare tunnel, the push/pull round-trip proven from a genuine claude.ai cloud session, conflicting edits semantically merged rather than last-write-wins, multi-project isolation verified, and the whole client and server sharing one tested implementation. The Node server and shell hooks remain in the tree as the rollback path until the Rust binary has run in production for a while. See `ROADMAP.md` for the evidence behind each phase.

## Project docs

| File | What it's for |
|---|---|
| `ARCHITECTURE.md` | Hook wiring, backend shape, merge strategy, project-key derivation. |
| `ROADMAP.md` | Phased build plan, current status, and what's explicitly deferred. |
| `CONTRIBUTING.md` | Dev workflow. |
| `CLAUDE.md` | How Claude Code sessions should work in this repo (worktrees, task list, PR policy, ground rules). |
| `docs/install.md` | Installing the `recall` CLI (npm / Homebrew / curl) and opting a project in with `recall init`. |
| `docs/phase-0-findings.md` | Empirical findings from Phase 0 — what the docs above assumed vs. what the installed Claude Code CLI actually does. |
| `docs/token-setup.md` | Generating and installing `RECALL_TOKEN` on every environment (laptop, cloud). |
| `docs/github-actions-deploy.md` | Optional: CI checks on every PR, plus auto-deploy to a VPS over SSH on push to `main`. |
| `docs/rust-rewrite.md` | The current implementation: why Rust, what it costs, and what's frozen. |
| `docs/go-rewrite-design.md` | Historical: the Go port that preceded it, and the staged-migration reasoning still in use. |
| `crates/` | The binary: `wire` (shared contract), `paths`, `hooks`, `server`, `cli`. |
| `npm/`, `Formula/`, `install.sh` | The three install channels. |
| `server/` | Dockerfile and entrypoint for the deployed server. `index.js` is the superseded Node implementation, kept as the rollback path. |
| `hooks/` | The superseded shell hooks, kept working for projects already wired to them. |
| `deploy/` | OrbStack + Cloudflare Tunnel deployment (docker-compose based), including enabling merge. |

## License

MIT — see `LICENSE`.
