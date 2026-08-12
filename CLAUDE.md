# CLAUDE.md

Instructions for Claude Code sessions working in this repo. For **what** to build, read `ARCHITECTURE.md` and `ROADMAP.md` first — this file is about **how** to work here.

## Work in a worktree, not directly on `main`

Before any non-trivial change, call `EnterWorktree`. Don't commit feature work straight onto `main`. Branch from the default branch (`worktree.baseRef: fresh`) unless the user explicitly wants to branch off local HEAD. `keep` the worktree if there's a PR to open or more to do later; `remove` once merged/abandoned.

## Work as a team via the task list

For multi-part work — a `ROADMAP.md` phase, or any multi-part request — use `TaskCreate`/`TaskList`/`TaskUpdate` instead of a private todo list. One task per independently-completable unit; use `addBlockedBy`/`addBlocks` for real sequencing. Claim via `owner` before starting; only mark `completed` when actually done. Check `TaskList` before picking up new work — multiple worktree sessions may be active concurrently.

## Pick the workflow shape per task

A single well-scoped change → one agent, one worktree, straight through. Several genuinely independent pieces → split onto the task list, parallel worktree sessions. Real sequencing (e.g., project-key derivation before the pull script that depends on it) → encode with `blockedBy`, don't force parallelism onto sequential work.

## PRs and merging

- All work lands via PR from a worktree branch — never a direct push to `main`.
- Merge **squash-only**: `merge_method: "squash"` always, never `merge` or `rebase`.
- No `Co-Authored-By: Claude ...` (or similar AI-attribution) footer in any commit or merge-commit message.
- Repo-level enforcement (set once in GitHub, not settable via API here): **Settings → General → Pull Requests** — enable only "Allow squash merging"; disable the other two. Optionally enable "Automatically delete head branches."

## Ground rules

These were the project's original build-brief constraints (`PROMPT.md`, retired 2026-08-12 once the project it was written to kick off was actually built — see `ROADMAP.md`'s "Explicitly deferred" for the fuller reasoning behind the multi-user one). They're still load-bearing:

- No Anthropic API key anywhere in this codebase — LLM-assisted merge goes through the local `claude` CLI only, authenticated via whatever `claude login` session is already on the machine running it.
- No multi-user auth/billing — single owner, one bearer token. Recall is self-hosted by and for one person: no signup flow, no OAuth-for-other-users.
- Hook config for a synced project lives in *that project's* `.claude/settings.json`, never user-level — this is why Recall works from fresh cloud sessions. Don't "simplify" this into `~/.claude/settings.json` — it would silently break the entire point.

If a future task pushes toward multi-tenant auth, a hosted "Recall as a service for others" product, or an Anthropic API key anywhere in the request path — stop and ask the user first. That's a different project with different compliance implications (other people's data, billing, a privacy policy), not an incremental feature.
