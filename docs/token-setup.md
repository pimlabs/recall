# Setting up `RECALL_TOKEN` (and friends) per environment

One shared secret, generated once, installed on every environment that
should push or pull. This is the whole auth story — no accounts, no
per-user tokens (see `CLAUDE.md`'s ground rules on why).

## 1. Generate it once

```sh
openssl rand -hex 32
```

Put the result in `deploy/.env` as `RECALL_TOKEN` (see `deploy/README.md`)
— that's the value the server checks against. Every environment below
needs to send this same value.

## 2. Install it on your laptop

Add to your shell profile (`~/.zshrc`, `~/.bashrc`, whichever your shell
reads):

```sh
export RECALL_TOKEN="<the value from step 1>"
export RECALL_URL="https://recall.yourdomain.com"
```

Open a new terminal (or `source` the file) so the hooks pick it up —
`recall push` / `recall pull` read these from the environment,
not from a config file.

## 3. Install it on a claude.ai cloud environment

Cloud sessions don't read your laptop's shell profile — each environment
has its own secrets, set once and reused by every session spawned from
it. In that environment's settings (the "Add/Edit cloud environment"
dialog):

- **Environment variables**: add `RECALL_TOKEN` and `RECALL_URL` with the
  same values as step 2, plus `CLAUDE_CODE_REMOTE_MEMORY_DIR` (see
  `../ARCHITECTURE.md` for why that one's required, not optional, here).
- **Network access**: set to **Custom** and add the server's domain under
  **Allowed domains** — confirmed live (see `ROADMAP.md` Phase 1) that the
  default network policy blocks a self-hosted domain otherwise.

This is per-environment, not account-wide. A new cloud environment for a
different project needs this repeated (same values — the token and URL
don't change per project, only `project_key` does, and that's derived
automatically from the project's git remote, not something you set).

## 4. Verify

From any environment with the token installed:

```sh
curl -H "Authorization: Bearer $RECALL_TOKEN" "$RECALL_URL/health"
# expect: {"status":"ok", ...}
curl "$RECALL_URL/health"  # no header
# also 200 — /health is intentionally unauthenticated
curl "$RECALL_URL/sync?project_key=test"
# expect: 401, without the header
```

If push/pull aren't working and it's not obviously a token typo, check
`GET /health`'s `merge.claude_cli` (Phase 2 merge status) and the
environment's Network access setting (step 3) before assuming the token
itself is wrong — both produce failures that look similar from the hook's
side (a failed `curl`).

## Rotating the token

Not automated — this is a single shared secret, so rotating it means:
generate a new one, update `deploy/.env` and restart the server, then
update every environment from steps 2-3 before their next push/pull (a
stale token just gets `401`s until updated, nothing worse). There's no
urgency to rotate on a schedule for a single-owner personal server; do it
if the token leaks (e.g. committed by accident — check `git log -p` for
`RECALL_TOKEN` if ever unsure) or when a device permanently retires.

## Multiple projects, one token, no cross-contamination

The same token authenticates every project on the server — it doesn't
scope which `project_key` a request can touch (see `CLAUDE.md`: single
owner, no multi-user auth, so there's no "which projects can this token
see" question to answer). What's actually verified is that different
projects never see each other's content: pushing conflicting content
under two different `project_key`s (derived automatically from each
project's own git remote — see `ARCHITECTURE.md`) and reading each back,
including a delete in one, confirmed live to leave the other completely
untouched. The isolation comes from `project_key` being part of the
primary key on every row and every query being scoped by it — not from
anything token-related.
