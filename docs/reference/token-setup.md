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

From inside a project you want synced:

```sh
recall connect https://recall.yourdomain.com
```

Paste the value from step 1 at the prompt; it is not echoed. `connect`
checks it against the server before saving anything — a mistyped paste is
asked for again — then asks for a name for this machine, suggesting one
from the hostname. After saving it offers to wire the project you are in
and send its existing memory; answer no to either and `recall init` /
`recall backfill` do the same later. It writes two files (set `RECALL_HOME`
to put them elsewhere):

```text
~/.recall/config.toml        0644  the server, and this machine's name
~/.recall/credentials.toml   0600  the token, one per server
```

Both the URL and the token come from there, so there is nothing to add to a
shell profile. They are two files so that the one you open and edit — to
name the machine, or keep in a dotfiles repository — never has a secret in
it. A machine that ran `recall connect` on 0.3.0 has a `credentials.json`
instead; the first `recall` command after upgrading moves it into these two
and removes it.

**Why not the shell profile, which is what this page used to say.** A token
in the environment is inherited by every process started from that shell,
including the install script of any package in any project; shell profiles
are among the most commonly published files there are, in dotfiles
repositories; and `export RECALL_TOKEN=…` typed once stays in shell history.

**Already have it in your profile?** Run `recall connect`, then delete the
`export RECALL_…` lines and open a new terminal. Until you do, the exported
values win — anything the environment sets outranks the saved files — and
`recall connect` names each one that is still set, including
`RECALL_SOURCE_ENV` and `RECALL_MACHINE_KEY`, which the machine name
replaces. `recall doctor` warns about a shell token on a
laptop; it cannot tell which file exported it, so it will not guess one.

The layering, lowest first: the saved file, then your shell, then any
`.claude/settings.json` `env` block, which Claude Code applies over the shell.
A settings file is the right home for a value that describes the *project*
(see [`install.md`](install.md)), never for a token — committing one would
publish it. `recall status` names where each value came from.

## 3. Install it on a claude.ai cloud environment

Cloud sessions don't read anything from your laptop — each environment
has its own secrets, set once and reused by every session spawned from
it. Here the variables *are* the right place: they are a secret store, and
`recall connect` refuses to run in a cloud session because the container it
would save into is thrown away. In that environment's settings (the "Add/Edit cloud environment"
dialog):

**Environment variables** — four of them:

| Name | Value |
|---|---|
| `RECALL_URL` | the same as step 2 |
| `RECALL_AUTHKEY` | an authkey, below; or `RECALL_TOKEN`, the same as step 2, against a server older than 0.4.1 |
| `CLAUDE_CODE_REMOTE_MEMORY_DIR` | `/home/user/.claude` |
| `RECALL_GLOBAL_KEY` | the same as step 2, or leave it out entirely |

**`RECALL_AUTHKEY` rather than the token**, from 0.4.1. Make one on a
machine enrolled as admin:

```sh
recall authkey create --tag cloud --expires 90d
```

It is shown once. It can only enrol `sync` devices, never read or write
memory itself, and it expires. Each session's first `recall pull` uses it to
enrol that session as an ephemeral device, `cloud-…`, which then signs its
requests; the server removes the device a day after its last request. A
leaked key is `recall authkey revoke <id>` (add
`--revoke-devices` to cut off what it already enrolled), and no laptop has
to change. The token in the environment's variables, by contrast, is the
whole server's secret. Once sessions enrol, remove `RECALL_TOKEN` from
them; `recall doctor` warns while a session still uses it.

**`/home/user/.claude` is not `$HOME`.** In a cloud session `$HOME` is
`/root`, so anyone reasoning it out arrives at the wrong answer — and the
wrong answer behaves exactly like leaving it unset, which is to say Claude
Code's auto-memory never switches on and Recall has nothing to sync. Nothing
errors. Paste the value rather than deriving it. (`../ARCHITECTURE.md` has
why the variable is required at all; the memory path derivation prefers the
explicit override, which is why a value that does not match `$HOME` still
resolves.)

**`RECALL_GLOBAL_KEY` must match your laptop's, or be absent.** It is on or
off per environment, and a mixture leaves `MEMORY.md` linking files that
environment never fetches.

**`RECALL_MACHINE_KEY` is the one to leave out.** Every session here is a new
machine, so the facts your laptop filed under its key do not describe this
one. Unset means `machine/` is ignored rather than filed under the project.
Both scopes are in [`install.md`](install.md).

**Network access**: set to **Custom** and add the server's domain under
**Allowed domains** — confirmed live (see `ROADMAP.md` Phase 1) that the
default network policy blocks a self-hosted domain otherwise. Without this,
everything above is correct and nothing reaches the server.

Environment variables are read when a session's container starts, so editing
this dialog does not reach a session already running. Start a new one to pick
up a change.

This is per-environment, not account-wide. A new cloud environment for a
different project needs this repeated. The values are the same either way:
the token and URL don't change per project, and `project_key` — the one
thing that does — is derived from the project's git remote rather than set
here. A project that needs to declare its key instead does it in its own
committed `.claude/settings.json`, which every environment already reads;
see [`install.md`](install.md).

## 4. Verify

The short version, from any environment with the token installed, is
`recall doctor`: it checks every variable above, says which are missing, and
exits non-zero if sync is actually broken — including the case this page
exists to prevent, an unset `CLAUDE_CODE_REMOTE_MEMORY_DIR` in a remote
session. See [`install.md`](install.md).

By hand:

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
stale token just gets `401`s until updated, nothing worse). On a laptop
that is `recall connect` again: it finds the saved token rejected, asks for
the new one, and replaces it only once that is accepted. There's no
urgency to rotate on a schedule for a single-owner personal server; do it
if the token leaks (e.g. committed by accident — check `git log -p` for
`RECALL_TOKEN` if ever unsure) or when a device permanently retires.

## Multiple projects, one token, no cross-contamination

The same token authenticates every project on the server — it doesn't
scope which `project_key` a request can touch (see `CLAUDE.md`: single
owner, no multi-user auth, so there's no "which projects can this token
see" question to answer). What's actually verified is that different
projects never see each other's content: pushing conflicting content
under two different `project_key`s (from each project's own git remote, or
whatever it declared — see `ARCHITECTURE.md`) and reading each back,
including a delete in one, confirmed live to leave the other completely
untouched. The isolation comes from `project_key` being part of the
primary key on every row and every query being scoped by it — not from
anything token-related.
