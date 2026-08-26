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

- **Push**: a `PostToolUse` hook matching `Edit|Write` runs a script (`hooks/recall-push`) that checks whether the edited file is a memory file and, if so, `curl`s it to Recall's API. (An earlier design assumed a `FileChanged` event and a declarative `http` hook type — neither exists in the installed CLI; see `docs/phase-0-findings.md`.)
- **Pull**: a `SessionStart` hook runs a small script (`hooks/recall-pull`) that fetches the latest synced state before Claude loads context.

Both hooks live in the **project's own `.claude/settings.json`**, checked into git — so any environment that clones the repo (laptop or fresh cloud session) picks up sync automatically. See `ARCHITECTURE.md`.

## Status

Phases 0 through 3 done: server + hooks exist, deployed for real (OrbStack + Cloudflare Tunnel, `deploy/`), the push/pull round-trip is proven from a genuine claude.ai cloud session, conflicting edits now get a real semantic merge instead of last-write-wins, and multi-project isolation plus token setup are documented and verified live. See `ROADMAP.md` for the evidence behind each. Phase 4 (operational polish) is open-ended and mostly already underway.

## Project docs

| File | What it's for |
|---|---|
| `ARCHITECTURE.md` | Hook wiring, backend shape, merge strategy, project-key derivation. |
| `ROADMAP.md` | Phased build plan, current status, and what's explicitly deferred. |
| `CONTRIBUTING.md` | Dev workflow. |
| `CLAUDE.md` | How Claude Code sessions should work in this repo (worktrees, task list, PR policy, ground rules). |
| `docs/phase-0-findings.md` | Empirical findings from Phase 0 — what the docs above assumed vs. what the installed Claude Code CLI actually does. |
| `docs/token-setup.md` | Generating and installing `RECALL_TOKEN` on every environment (laptop, cloud). |
| `docs/github-actions-deploy.md` | Optional: CI checks on every PR, plus auto-deploy to a VPS over SSH on push to `main`. |
| `server/` | The sync backend. |
| `hooks/` | `recall-push`/`recall-pull` scripts + the settings.json snippet to opt a project in. |
| `deploy/` | OrbStack + Cloudflare Tunnel deployment (docker-compose based), including enabling merge. |

## License

MIT — see `LICENSE`.
