# Roadmap

## Phase 0 — Prove the round-trip, no merge logic yet — done

- [x] Stood up the server with `POST /sync` / `GET /sync` against SQLite (`server/`), no merge — last write simply overwrites. Zero external dependencies (`node:http` + `node:sqlite`).
- [x] Built the `PostToolUse`/`SessionStart` hooks (`hooks/recall-push`, `hooks/recall-pull`, `hooks/settings.snippet.json`) — see below for why `PostToolUse` instead of `FileChanged`.
- [x] Confirmed empirically: **`FileChanged` doesn't exist** in the installed Claude Code CLI (v2.1.42), and neither does hook type `"http"`. Used `PostToolUse` matching `Edit|Write` (`type: "command"`) instead. Confirmed live — not just by reading the source — that this **does** catch dynamically-named topic files (e.g. `debugging.md`), because the matcher only filters on tool name and the script itself checks the actual file path per call. Full writeup: `docs/phase-0-findings.md`.
- [x] Confirmed the project-key derivation does **not** match Claude Code's own scoping (which is local-filesystem-path-based, not git-remote-based) — and that this divergence is intentional/necessary. `project_key` derives from the git remote's `owner/repo`; the local memory directory is computed separately by replicating Claude Code's own path-slug algorithm. Details and edge cases (proxied remotes, GitLab subgroups) in `docs/phase-0-findings.md` §6.
- Also surfaced, not originally in scope but load-bearing: auto memory is **off by default in remote/cloud sessions** unless `CLAUDE_CODE_REMOTE_MEMORY_DIR` is set (`docs/phase-0-findings.md` §5) — this has to be configured as an environment secret alongside `RECALL_TOKEN`.

**Done when:** editing a memory file on one machine, then starting a fresh session (ideally an actual ephemeral cloud session) on the same project, shows the updated content — with zero manual setup on the second environment beyond having cloned the repo. **Proven at the mechanism level**: two simulated machines (different filesystem paths, different git-remote URL shapes) round-tripped `MEMORY.md` and a dynamically-named topic file through a live server instance byte-for-byte (`docs/phase-0-findings.md`, "Round-trip proof"). Deploying a real server and validating against an actual second ephemeral cloud session is the remaining operational step, not a mechanism gap — which is exactly what Phase 1 below is for.

## Guiding rule from here on

**Prototype works, then features get added — not the other way around.** Phase 0 proved every piece works in isolation/simulation. Before building anything new (merge quality, multi-project isolation, polish), Phase 1 makes the whole thing run for real, on real infrastructure, between a real laptop and a real fresh cloud session, with today's simplest behavior (last-write-wins). Every later phase assumes Phase 1's deployed server and wired-up hooks already exist and already work — they're additive on top of a working thing, not prerequisites to having one.

## Phase 1 — Make the prototype actually work, end to end

No new features. The server and hooks from Phase 0 already do everything needed — this phase is entirely about closing the gap between "proven in a simulated sandbox" and "a real person's memory actually syncs."

- [x] **Deploy the server somewhere it stays running.** Done via OrbStack + Cloudflare Tunnel (`deploy/`) — `recall-server` and `cloudflared` containers running on the owner's Mac, public hostname `recall.pimlabs.id`. Verified from outside: `GET /sync?project_key=smoke-test` returns `{"project_key":"smoke-test","files":[]}` with a valid bearer token, `401` without one.
- [x] **Generate the real `RECALL_TOKEN`** — generated, stored in `deploy/.env` (gitignored, not committed).
- [ ] **Set `CLAUDE_CODE_REMOTE_MEMORY_DIR` as a real environment secret** on the actual cloud environment(s) used — this was the load-bearing gap Phase 0 found; without it, auto memory silently never activates there.
- [x] **Wire `hooks/settings.snippet.json` into one real project's `.claude/settings.json`** — wired into `pimlabs/recall` itself, dogfooding.
- [ ] **Run the real test**: edit a memory file on one real machine, start an actual second fresh session (ideally a genuine ephemeral cloud session, not this same one) on the same project, confirm the content shows up with zero manual setup beyond the env vars above already being configured.
- **Fix whatever breaks under real conditions** that a local simulation can't catch — network reachability from a cloud sandbox to wherever the server lives, `curl`/`jq` actually present in the target image, TLS, hook execution latency, token handling. Expect at least one surprise here; that's the point of doing this before adding more moving parts.

**Done when:** the exact "done when" from Phase 0 is true for real — a genuine second environment, zero simulation.

## Phase 2 — Real merge

- Replace last-write-wins with the `claude -p` semantic merge described in `ARCHITECTURE.md` for memory file content.
- Handle the server needing its own logged-in `claude` CLI to do this — figure out what that operationally requires. (Phase 1's hosting choice should already make this straightforward — see above.)

**Done when:** two environments editing the same topic file with genuinely different information both end up represented after a sync, not just the more-recent one.

## Phase 3 — Multiple projects, token/auth hardening

- Confirm the server correctly separates memory by `project_key` for more than one repo.
- Bearer token setup made boring: a short doc on generating and installing `RECALL_TOKEN` per environment (laptop shell profile, cloud session secrets).

**Done when:** two different projects synced through the same Recall server never cross-contaminate memory.

## Phase 4 — Operational polish (open-ended)

- [x] Basic observability: `GET /health` (unauthenticated) reports server status, start time, and the most recent sync across all projects. Global rather than per-project — good enough to answer "is this thing alive" from outside without a token; per-project last-synced-at can wait until it's actually needed.
- Decide whether `recall-pull` should be a single static binary vs. a script needing a runtime — revisit once Phase 1's real-world deployment is known to work.

## Explicitly deferred

- Anything in `PROMPT.md`'s non-goals list. Multi-user/hosted-for-others is a different project — don't fold it in incrementally.
