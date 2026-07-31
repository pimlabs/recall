# Recall

A minimal, self-hosted sync service for Claude Code's **auto memory** — the notes Claude writes about itself as it works (`~/.claude/projects/<project>/memory/`). Works from your laptop, your other laptop, and any fresh ephemeral cloud session, with no device pairing required.

## Why

Claude Code's auto memory is explicitly machine-local by design (see Anthropic's own docs — and a related feature request on `anthropics/claude-code` closed as "not planned"). `CLAUDE.md` already syncs fine via git; auto memory doesn't. Existing community tools (`claude-sync`, `claude-brain`, and similar) all assume a fixed set of named "devices" that pair with each other — that model breaks the moment one of your environments is an ephemeral cloud session that's never seen your other machines and won't exist tomorrow.

Recall exists for that specific gap: **a central service any environment can talk to, with no prior introduction.**

## What it is not

- Not a replacement for git-based `CLAUDE.md` sync — that's already solved, don't touch it.
- Not a multi-user product. Personal tool, single owner, no Anthropic API key, no auth system beyond a personal token. See `PROMPT.md`.
- Not append-only. Merge is closer to what `claude-brain` does (semantic merge via the local `claude` CLI) than to naive line-dedup.

## How it plugs into Claude Code

No custom client daemon. Claude Code's own hook system does the work:

- **Push**: a `FileChanged`/`PostToolUse` hook (type `http`) fires straight to Recall's API when a memory file changes.
- **Pull**: a `SessionStart` hook runs a small script that fetches the latest merged state before Claude loads context.

Both hooks live in the **project's own `.claude/settings.json`**, checked into git — so any environment that clones the repo (laptop or fresh cloud session) picks up sync automatically. See `ARCHITECTURE.md`.

## Status

Pre-implementation. This repo currently holds the build brief and architecture docs — see below before writing code.

## Project docs

| File | What it's for |
|---|---|
| `PROMPT.md` | Build brief — read this first. |
| `ARCHITECTURE.md` | Hook wiring, backend shape, merge strategy, project-key derivation. |
| `ROADMAP.md` | Phased build plan. |
| `CONTRIBUTING.md` | Dev workflow. |
| `CLAUDE.md` | How Claude Code sessions should work in this repo (worktrees, task list, PR policy). |

## License

MIT — see `LICENSE`.
