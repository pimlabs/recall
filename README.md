# Recall

A minimal, self-hosted sync service for Claude Code's **auto memory** — the notes Claude writes about itself as it works (`~/.claude/projects/<project>/memory/`). Works from your laptop, your other laptop, and any fresh ephemeral cloud session, with no device pairing required.

## Why

Claude Code's auto memory is explicitly machine-local by design (see Anthropic's own docs — and a related feature request on `anthropics/claude-code` closed as "not planned"). `CLAUDE.md` already syncs fine via git; auto memory doesn't. Existing community tools (`claude-sync`, `claude-brain`, and similar) all assume a fixed set of named "devices" that pair with each other — that model breaks the moment one of your environments is an ephemeral cloud session that's never seen your other machines and won't exist tomorrow.

Recall exists for that specific gap: **a central service any environment can talk to, with no prior introduction.**

## What it is not

- Not a replacement for git-based `CLAUDE.md` sync — that's already solved, don't touch it.
- Not a multi-user product. Personal tool, single owner, no Anthropic API key, no auth system beyond a personal token. See `CLAUDE.md`'s Ground rules.
- Not append-only. Merge works like `claude-brain`'s does: a semantic merge via the local `claude` CLI, not naive line-dedup — implemented and verified live, see `ROADMAP.md` Phase 2.

## How it plugs into Claude Code

No custom client daemon. Claude Code's own hook system does the work:

- **Push**: a `PostToolUse` hook matching `Edit|Write` runs `recall push`, which checks whether the edited file is a memory file and, if so, sends it to Recall's API. (An earlier design assumed a `FileChanged` event and a declarative `http` hook type — neither exists in the installed CLI; see [`docs/history/phase-0-findings.md`](docs/history/phase-0-findings.md).)
- **Pull**: a `SessionStart` hook runs `recall pull`, which fetches the latest synced state before Claude loads context.

Neither can break a session: an unreachable server or an unconfigured machine warns on stderr and exits 0.

Both hooks live in the **project's own `.claude/settings.json`**, checked into git — so any environment that clones the repo (laptop or fresh cloud session) picks up sync automatically. See `ARCHITECTURE.md`.

## Start here

Recall is a single Rust binary that is both halves: `recall serve` runs the
server, everything else runs beside your editor. Which half you need depends
on what you already have.

### A. You don't have a server yet — [**set one up**](deploy/README.md)

Do this first. Every client instruction below asks for a `RECALL_URL` and a
`RECALL_TOKEN`, and both come from the server; there is nothing to connect to
until it exists.

What it costs, before you start: a host that stays up (a small VPS is the
usual answer), Docker with Compose v2, a clone of this repository on that
host — both compose files build from source — and either a domain you control
or a Cloudflare account, depending on which ingress you pick.
[`deploy/README.md`](deploy/README.md) walks the whole thing, ingress first,
because that is the choice everything else follows from.

### B. You have a server — [**connect a machine to it**](docs/reference/install.md)

Install the binary, export two variables, and opt each project in:

```sh
npm install -g @pimlabs/recall                    # or bun, or pnpm
brew tap pimlabs/recall https://github.com/pimlabs/recall && brew install pimlabs/recall/recall
curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
cargo install recall-sync
```

```sh
export RECALL_URL="https://recall.yourdomain.com"
export RECALL_TOKEN="..."                    # docs/reference/token-setup.md

recall init                                  # wires .claude/settings.json
git add .claude/settings.json && git commit  # so fresh clones get it too
recall status                                # confirm it's actually working
```

**"Connect" means your own second machine**, or a fresh cloud session — not
someone else's. Recall is single-owner by design: one token, no accounts, no
per-user anything. See `CLAUDE.md`'s ground rules for why that is a decision
rather than an omission.

What each install channel actually does, and the rest of the client story, is
in [`docs/reference/install.md`](docs/reference/install.md).

### Then, whichever door you came through

Notes about *you* rather than the repo can follow you into every project: set
`RECALL_GLOBAL_KEY`, then `recall promote <file>` moves one there. And where
the `owner/repo` key derived from the git remote is wrong — a repo with no
remote, a monorepo, a fork — `RECALL_PROJECT_KEY` declares it instead. Both
are in [`docs/reference/install.md`](docs/reference/install.md).

To talk to the server directly, see the
[HTTP API reference](docs/reference/api.md).

## Status

Phases 0 through 9 done. Recall is one Rust binary
(`docs/history/rust-rewrite.md`) that runs both halves: the push/pull round-trip
proven from a genuine claude.ai cloud session, conflicting edits semantically
merged rather than last-write-wins, multi-project isolation verified, and a
global scope so a note about *you* is not stuck in whichever repository Claude
happened to learn it in.

**The Rust server runs in production as of 2026-09-14** (Phase 9), behind
Traefik, reading the database the Node implementation wrote — no migration,
which is what the frozen schema was always for.

The Node server and the bash hooks it replaced are **gone** from the tree
(Phase 7) — git history is the rollback path, and what they were really
carrying is now `fixtures/node-written.db`, a database the Node server
actually wrote, so `scripts/compat-check.sh` still proves this server reads
production's rows.

`ROADMAP.md` has the evidence behind each phase, including what was measured
rather than assumed and two conclusions that turned out wrong.

## Project docs

**[`docs/`](docs/README.md) is the index.** The short version:

| Start here | For |
|---|---|
| [`deploy/README.md`](deploy/README.md) | **Door A** — standing the server up, ingress first |
| [`docs/reference/install.md`](docs/reference/install.md) | **Door B** — installing the CLI and opting a project in |
| [`docs/reference/token-setup.md`](docs/reference/token-setup.md) | The token both doors need, onto every machine, laptop and cloud |
| [`docs/reference/releasing.md`](docs/reference/releasing.md) | Cutting a release across all four channels |
| [`docs/reference/api.md`](docs/reference/api.md) | The HTTP API: endpoints, schemas, status codes, examples |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How it works, and the code map |
| [`docs/history/rust-rewrite.md`](docs/history/rust-rewrite.md) | Why Rust, honestly — including every bug this project has shipped |
| [`docs/history/memory-loading-findings.md`](docs/history/memory-loading-findings.md) | What Claude Code actually does with memory files, and why one probe proves nothing |
| [`ROADMAP.md`](ROADMAP.md) | Every phase, the evidence behind it, and what is deliberately deferred |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Building, testing, and what not to "clean up" |
| [`CHANGELOG.md`](CHANGELOG.md) | What changed between releases, for someone deciding whether to upgrade |

And the tree:

| Path | What's in it |
|---|---|
| `crates/` | The binary. `recall-wire` (frozen contract) · `recall-paths` · `recall-hooks` (client) · `recall-server` · `recall-sync` (`main`, for both halves — `serve` included). See ARCHITECTURE's code map. |
| `deploy/` | The image, and a Compose file per ingress — Cloudflare Tunnel or an existing Traefik. Runs on anything with Docker. |
| `scripts/` | `compat-check.sh` (the cutover matrix), `api-doc-check.sh`, `trusted-ip-check.sh`, `release.sh`, and `probes/`. |
| `npm/`, `Formula/`, `install.sh` | Three of the four install channels. The fourth, `cargo install recall-sync`, needs no file here. |
| `fixtures/` | A database the retired Node server actually wrote, so the cutover stays testable without it. |

## License

MIT — see `LICENSE`.
