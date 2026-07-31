# Recall — Build Brief

Kickoff prompt for building Recall. Written to stand alone in a fresh Claude Code session.

## What this is

Recall is a small, self-hosted **sync service for Claude Code's auto memory** — the per-project notes Claude writes for itself (`~/.claude/projects/<project>/memory/MEMORY.md` + topic files). It lets that memory follow the user across machines *and* into ephemeral cloud sessions, without any two environments needing to know about each other in advance.

## Why it exists, and the constraint that follows from it

`CLAUDE.md` already syncs perfectly well via git — nothing to build there. Auto memory doesn't, by design (confirmed via Anthropic's own docs, and a GitHub feature request on `anthropics/claude-code` closed "not planned"). Every existing community tool for this (`claude-sync`, `claude-brain`, etc.) is peer-to-peer or device-registry based: machines "introduce" themselves to each other via git remotes, Dropbox folders, or Syncthing pairing.

That model doesn't fit the actual use case: **one of the environments is routinely a fresh, ephemeral cloud Claude Code session that has never seen any other machine and won't exist after this session ends.** There's no window to pair devices. Recall's reason to exist is specifically this gap — a central service any environment can pull from and push to cold, with no prior handshake.

This produces the same kind of load-bearing constraint Fleet has, and for the same underlying reason (read `pimlabs/fleet`'s `PROMPT.md` if you want the fuller reasoning — it's the same owner, same logic, applied to a different tool):

- **No Anthropic API key anywhere in this app.** Any LLM-assisted work (see "Merge strategy" below) goes through the local `claude` CLI (`claude -p ...`), which authenticates via the machine's own `claude login` / subscription session — never a raw `/v1/messages` call with a key.
- **Single owner, no multi-user auth.** Recall's server is self-hosted by and for one person. There's no signup flow, no OAuth-for-other-users, no billing. Auth is a single personal bearer token the owner generates once.
- If a future task pushes toward multi-tenant auth, a hosted "Recall as a service for others" product, or an Anthropic API key anywhere in the request path — **stop and ask the user first.** That's a different project with different compliance implications, not an incremental feature.

## Requirements established during design

1. **Sync all memory topic files**, not just `MEMORY.md` — the whole `~/.claude/projects/<project>/memory/` directory for a given project.
2. **Not append-only.** Two devices writing similar-but-differently-worded notes shouldn't just pile up as duplicates forever. Merge should behave more like `claude-brain`'s approach (semantic merge via a local `claude -p` call) than like naive line-hash dedup.
3. **Must work from ephemeral cloud sessions with zero prior setup on that machine**, beyond the repo itself being cloned. This is the requirement that rules out every pure peer-to-peer tool and is why Recall needs an actual backend, not a P2P protocol.
4. **Project identity** should be derived the same way Claude Code itself scopes auto memory — from the project's git remote — so a laptop and a cloud session both working on the same repo agree on which memory they're syncing without any manual configuration.

## Where the hook config has to live (read before changing this)

The hook wiring (see `ARCHITECTURE.md`) has to be declared in the **target project's own `.claude/settings.json`**, committed to that project's repo — not in `~/.claude/settings.json`. A fresh cloud session has no access to the user's home-directory config; it only has whatever's in the repo it cloned. If the hooks lived in user-level settings, cloud sessions would silently get no sync at all, defeating the entire point. This is not a style preference — it's the one non-negotiable design decision Recall is built around.

## Non-goals

- Syncing anything other than auto memory (CLAUDE.md, skills, settings, sessions — leave those to git or to not existing as a problem in the first place).
- Real-time collaborative editing between two humans.
- A GUI. This is a backend + a couple of hook scripts.
- Supporting Windows without WSL, unless it turns out to be trivial.

## Where to go next

Read `ARCHITECTURE.md` for the concrete shape (endpoints, merge logic, project-key derivation, the settings.json snippet a project needs to opt in), then `ROADMAP.md` for build order. Start at Phase 0 — prove the push/pull round-trip on one project before worrying about merge quality.
