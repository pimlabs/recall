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
- [x] **Set `CLAUDE_CODE_REMOTE_MEMORY_DIR` as a real environment secret** on a real claude.ai cloud environment for `pimlabs/recall` (`/home/user/.claude` — that sandbox's `$HOME` is actually `/root`, but `hooks/lib.sh` prioritizes the explicit override so the memory path still resolves correctly).
- [x] **Wire `hooks/settings.snippet.json` into one real project's `.claude/settings.json`** — wired into `pimlabs/recall` itself, dogfooding.
- [x] **Run the real test**: verified 2026-08-12 in a genuine claude.ai cloud session — SessionStart's `recall-pull` synced all 3 existing memory files with zero manual intervention, then editing one of them triggered `recall-push` automatically and the change showed up server-side seconds later.
- [x] **Fix whatever breaks under real conditions**: two surprises, both fixed. (1) A trailing-newline round-trip bug in `recall-push`/`recall-pull` (fixed, see the newline-fidelity fix commit). (2) claude.ai cloud environments block egress to custom domains by default — `recall.pimlabs.id` needed adding under that environment's **Network access → Custom → Allowed domains**, not something a local simulation could have caught.

**Done when:** the exact "done when" from Phase 0 is true for real — a genuine second environment, zero simulation. **Done, 2026-08-12.**

## Phase 2 — Real merge

- Replace last-write-wins with the `claude -p` semantic merge described in `ARCHITECTURE.md` for memory file content.
- Handle the server needing its own logged-in `claude` CLI to do this — figure out what that operationally requires. (Phase 1's hosting choice should already make this straightforward — see above.)

**Done when:** two environments editing the same topic file with genuinely different information both end up represented after a sync, not just the more-recent one.

## Phase 3 — Multiple projects, token/auth hardening

- Confirm the server correctly separates memory by `project_key` for more than one repo.
- Bearer token setup made boring: a short doc on generating and installing `RECALL_TOKEN` per environment (laptop shell profile, cloud session secrets).

**Done when:** two different projects synced through the same Recall server never cross-contaminate memory.

## Phase 4 — Operational polish (open-ended)

- [x] Basic observability: `GET /health` (unauthenticated) reports server status, start time, and the most recent sync across all projects. Global rather than per-project — good enough to answer "is this thing alive" from outside without a token; per-project last-synced-at can wait until it's actually needed. Also reports the deployed `git_commit` (baked in via a Docker build arg), added after deciding formal semver/CHANGELOG wasn't worth it for a single-owner tool deployed straight from `main` — the real risk was deployment drift (`main` has a fix, the running container doesn't yet), not version compatibility between independent consumers.
- [x] **Automatic backups.** Found during an architecture/security pass (2026-08-12) that there was no backup story at all — a real gap given cloud sessions are ephemeral by design, so the server can be the *only* surviving copy of content that originated there. Server now runs `VACUUM INTO` on an interval (default 24h, keeps last 7), writing to a host-mounted `deploy/backups/` folder so external backup tooling can reach it without going through Docker. See `deploy/README.md`.
- [x] **Delete/tombstone support.** Same pass found `recall-push` silently no-ops on a local delete — the server never learned a file was removed, so it came back on the next pull. Fixed with a tombstone design (content preserved in the row, `deleted` flag set, `GET /sync` withholds content for tombstoned rows so a pull can't resurrect them). The harder half: there's no hook event for a delete at all (a `Bash rm` doesn't match the `Edit|Write` matcher even if there were one), so `recall-push` reconciles instead — every run compares the current directory listing against a small state file (`hooks/lib.sh:recall_state_file`, next to the memory dir, not inside it) and reports anything missing as a delete. Propagation is "next edit to any memory file in the project," not instant, since there's nothing to make it instant. Verified end to end: local delete + editing an unrelated file correctly produced a tombstone; a fresh pull skipped the tombstoned file; a stale local copy got actively removed on pull.
- [ ] **Non-root container user.** `server/Dockerfile` has no `USER` directive, so the process runs as root inside the container. Cheap defense-in-depth fix, not done yet.
- [ ] **Rate limiting on `/sync`.** No rate limiting exists today. Low severity (the bearer token is 256-bit, not guessable), but there's an open resource-exhaustion surface with nothing in front of it. Not done yet.
- [x] Decide whether `recall-pull` should be a single static binary (e.g. Go) vs. a script needing a runtime — revisited 2026-08-12 now that Phase 1's real-world deployment is known to work: staying with bash + curl + jq. Both dependencies have now been proven present and working on every real environment tested (this laptop, and a genuine claude.ai cloud sandbox), the one real bug they caused (trailing-newline fidelity) is fixed, and a compiled-binary rewrite would trade "clone and it just works" for per-platform binary distribution to fix a problem that hasn't actually recurred. Revisit again only if curl/jq turn out missing on some future environment, or if the hooks' logic grows past what a shell script should reasonably hold.

## Explicitly deferred

- **Multi-user / a hosted "Recall as a service for others" product.** Raised and discussed 2026-08-12, shelved: use Recall personally for a while first to get real signal before committing to this. The technical shape is already mapped out if it comes back — it needs deciding on demand, not feasibility:
  - The VPS-hosting requirement is real friction, but a SaaS trades it for a different one (trusting a third party with potentially sensitive memory content), not eliminating friction outright.
  - Current auth (one shared bearer token, readable across every `project_key`) would need a full rewrite for real per-user isolation, not an extension.
  - Phase 2's planned merge shells out to a locally-logged-in `claude` CLI, which doesn't scale to many users' merges and directly conflicts with the no-API-key rule in `CLAUDE.md` — a SaaS needs a different answer to this specifically, independent of the auth question.
  - Cross-device project-path differences (raised as a concern, turned out already solved) are *not* a blocker: `project_key` derives from the git remote, not the local checkout path, proven live during the Phase 1 cloud test. The one real gap is a project with no git remote at all, which falls back to a path-based key that won't agree across machines — a known Phase 0 limitation, not new.
- Syncing anything other than auto memory (`CLAUDE.md`, skills, settings, sessions — leave those to git, or to not existing as a problem in the first place).
- Real-time collaborative editing between two humans.
- A GUI. This is a backend + a couple of hook scripts.
- Supporting Windows without WSL, unless it turns out to be trivial.
