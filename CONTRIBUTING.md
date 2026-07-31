# Contributing

Solo/personal project; light process to keep the history useful.

## Layout (once code exists)

```
server/         the sync backend (POST/GET /sync, SQLite storage, merge logic)
hooks/          recall-pull script + the .claude/settings.json snippet to opt a project in
PROMPT.md       build brief — read before structural changes
ARCHITECTURE.md hook wiring, backend shape, merge strategy, project-key derivation
ROADMAP.md      phased build order
```

## Ground rules (from PROMPT.md)

- No Anthropic API key anywhere in this codebase. LLM-assisted merge goes through the local `claude` CLI, never a raw API call with a key.
- No multi-user auth/billing — single owner, one bearer token, self-hosted.
- Hook config for a synced project lives in *that project's* `.claude/settings.json`, not user-level config — this is the whole reason Recall works in fresh cloud sessions.

Touching any of the above? Stop and confirm with the user first — see `PROMPT.md` → "Why it exists."

## Commit messages

State the why, not just the what.
