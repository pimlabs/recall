# CLAUDE.md

Instructions for Claude Code sessions working in this repo. For **what** to build, read `PROMPT.md`, `ARCHITECTURE.md`, `ROADMAP.md` first — this file is about **how** to work here.

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

## Ground rules carried over from `PROMPT.md`

- No Anthropic API key anywhere in this codebase — LLM-assisted merge goes through the local `claude` CLI only.
- No multi-user auth/billing — single owner, one bearer token.
- Hook config for a synced project lives in *that project's* `.claude/settings.json`, never user-level — this is why Recall works from fresh cloud sessions. Don't "simplify" this into `~/.claude/settings.json` — it would silently break the entire point.

If a change would touch any of these, stop and confirm with the user first.
