# Roadmap

## Phase 0 — Prove the round-trip, no merge logic yet — done

- [x] Stood up the server with `POST /sync` / `GET /sync` against SQLite (`server/`), no merge — last write simply overwrites. Zero external dependencies (`node:http` + `node:sqlite`).
- [x] Built the `PostToolUse`/`SessionStart` hooks (`hooks/recall-push`, `hooks/recall-pull`, `hooks/settings.snippet.json`) — see below for why `PostToolUse` instead of `FileChanged`.
- [x] Confirmed empirically: **`FileChanged` doesn't exist** in the installed Claude Code CLI (v2.1.42), and neither does hook type `"http"`. Used `PostToolUse` matching `Edit|Write` (`type: "command"`) instead. Confirmed live — not just by reading the source — that this **does** catch dynamically-named topic files (e.g. `debugging.md`), because the matcher only filters on tool name and the script itself checks the actual file path per call. Full writeup: `docs/phase-0-findings.md`.
- [x] Confirmed the project-key derivation does **not** match Claude Code's own scoping (which is local-filesystem-path-based, not git-remote-based) — and that this divergence is intentional/necessary. `project_key` derives from the git remote's `owner/repo`; the local memory directory is computed separately by replicating Claude Code's own path-slug algorithm. Details and edge cases (proxied remotes, GitLab subgroups) in `docs/phase-0-findings.md` §6.
- Also surfaced, not originally in scope but load-bearing: auto memory is **off by default in remote/cloud sessions** unless `CLAUDE_CODE_REMOTE_MEMORY_DIR` is set (`docs/phase-0-findings.md` §5) — this has to be configured as an environment secret alongside `RECALL_TOKEN`.

**Done when:** editing a memory file on one machine, then starting a fresh session (ideally an actual ephemeral cloud session) on the same project, shows the updated content — with zero manual setup on the second environment beyond having cloned the repo. **Proven at the mechanism level**: two simulated machines (different filesystem paths, different git-remote URL shapes) round-tripped `MEMORY.md` and a dynamically-named topic file through a live server instance byte-for-byte (`docs/phase-0-findings.md`, "Round-trip proof"). Deploying a real server and validating against an actual second ephemeral cloud session is the remaining operational step, not a mechanism gap.

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
