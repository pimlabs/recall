# Roadmap

## Phase 0 — Prove the round-trip, no merge logic yet

- Stand up the server with `POST /sync` / `GET /sync` against SQLite, no merge — last write simply overwrites.
- Add the `FileChanged`/`SessionStart` hooks to one real project's `.claude/settings.json`.
- Confirm empirically (don't assume): does `FileChanged` catch dynamically-named topic files under `memory/`, or only literal filenames? Pick the push hook type based on the answer (see `ARCHITECTURE.md`).
- Confirm the exact project-key derivation that matches how Claude Code itself scopes auto memory.

**Done when:** editing a memory file on one machine, then starting a fresh session (ideally an actual ephemeral cloud session) on the same project, shows the updated content — with zero manual setup on the second environment beyond having cloned the repo.

## Phase 1 — Real merge

- Replace last-write-wins with the `claude -p` semantic merge described in `ARCHITECTURE.md` for memory file content.
- Handle the server needing its own logged-in `claude` CLI to do this — figure out what that operationally requires (a persistent host, not a serverless function that spins up cold each request).

**Done when:** two environments editing the same topic file with genuinely different information both end up represented after a sync, not just the more-recent one.

## Phase 2 — Multiple projects, token/auth hardening

- Confirm the server correctly separates memory by `project_key` for more than one repo.
- Bearer token setup made boring: a short doc on generating and installing `RECALL_TOKEN` per environment (laptop shell profile, cloud session secrets).

**Done when:** two different projects synced through the same Recall server never cross-contaminate memory.

## Phase 3 — Operational polish (open-ended)

- Basic observability: last-synced-at per project, simple health check.
- Decide whether `recall-pull` should be a single static binary vs. a script needing a runtime — revisit once Phase 0's actual implementation choice is known to work.

## Explicitly deferred

- Anything in `PROMPT.md`'s non-goals list. Multi-user/hosted-for-others is a different project — don't fold it in incrementally.
