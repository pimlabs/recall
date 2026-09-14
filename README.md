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

- **Push**: a `PostToolUse` hook matching `Edit|Write` runs `recall push`, which checks whether the edited file is a memory file and, if so, sends it to Recall's API. (An earlier design assumed a `FileChanged` event and a declarative `http` hook type — neither exists in the installed CLI; see [`docs/phase-0-findings.md`](docs/phase-0-findings.md).)
- **Pull**: a `SessionStart` hook runs `recall pull`, which fetches the latest synced state before Claude loads context.

Neither can break a session: an unreachable server or an unconfigured machine warns on stderr and exits 0.

Both hooks live in the **project's own `.claude/settings.json`**, checked into git — so any environment that clones the repo (laptop or fresh cloud session) picks up sync automatically. See `ARCHITECTURE.md`.

## Quick start

Recall is a single Rust binary — the same artifact runs the server and the client. Install once per machine, whichever way suits it:

```sh
npm install -g @pimlabs/recall                    # or bun, or pnpm
brew tap pimlabs/recall https://github.com/pimlabs/recall && brew install pimlabs/recall/recall
curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
cargo install recall-sync
```

Details, and what each one actually does, in [`docs/install.md`](docs/install.md).

> **No release is tagged yet.** npm, Homebrew and `install.sh` all download a
> published binary, so until the first `v*` tag they have nothing to fetch.
> Build from source or use `cargo install --git` in the meantime — see
> [`docs/install.md`](docs/install.md#releases).

Set `RECALL_URL` and `RECALL_TOKEN` in your shell profile ([`docs/token-setup.md`](docs/token-setup.md)), then, in each project you want synced:

```sh
recall init                                  # wires .claude/settings.json
git add .claude/settings.json && git commit  # so fresh clones get it too
recall status                                # confirm it's actually working
```

Notes about *you* rather than the repo can follow you into every project:
set `RECALL_GLOBAL_KEY`, then `recall promote <file>` moves one there. And
where the `owner/repo` key derived from the git remote is wrong — a repo with
no remote, a monorepo, a fork — `RECALL_PROJECT_KEY` declares it instead.
Both are in [`docs/install.md`](docs/install.md).

Standing up the server itself is `recall serve`, in practice via [`deploy/`](deploy/README.md). To talk to it directly, see the [HTTP API reference](docs/api.md).

## Status

Phases 0 through 9 done. Recall is one Rust binary
(`docs/rust-rewrite.md`) that runs both halves: the push/pull round-trip
proven from a genuine claude.ai cloud session, conflicting edits semantically
merged rather than last-write-wins, multi-project isolation verified, and a
global scope so a note about *you* is not stuck in whichever repository Claude
happened to learn it in.

**The Rust server runs in production as of 2026-09-14** (Phase 9), behind
Traefik, reading the database the Node implementation wrote — no migration,
which is what the frozen schema was always for.

The Node server and the bash hooks it replaced are **gone** from the tree
(Phase 7) — git history is the rollback path, and what they were really
carrying is now `tests/fixtures/node-written.db`, a database the Node server
actually wrote, so `scripts/compat-check.sh` still proves this server reads
production's rows.

`ROADMAP.md` has the evidence behind each phase, including what was measured
rather than assumed and two conclusions that turned out wrong.

## Project docs

**[`docs/`](docs/README.md) is the index.** The short version:

| Start here | For |
|---|---|
| [`docs/install.md`](docs/install.md) | Installing the CLI and opting a project in |
| [`docs/releasing.md`](docs/releasing.md) | Cutting a release across all four channels |
| [`docs/token-setup.md`](docs/token-setup.md) | Getting a token onto every machine, laptop and cloud |
| [`docs/api.md`](docs/api.md) | The HTTP API: endpoints, schemas, status codes, examples |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How it works, and the code map |
| [`docs/rust-rewrite.md`](docs/rust-rewrite.md) | Why Rust, honestly — including every bug this project has shipped |
| [`docs/memory-loading-findings.md`](docs/memory-loading-findings.md) | What Claude Code actually does with memory files, and why one probe proves nothing |
| [`ROADMAP.md`](ROADMAP.md) | Every phase, the evidence behind it, and what is deliberately deferred |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | Building, testing, and what not to "clean up" |

And the tree:

| Path | What's in it |
|---|---|
| `crates/` | The binary. `recall-wire` (frozen contract) · `recall-paths` · `recall-hooks` (client) · `recall-server` · `recall-sync` (the CLI). See ARCHITECTURE's code map. |
| `deploy/` | The image, and a Compose file per ingress — Cloudflare Tunnel or an existing Traefik. Runs on anything with Docker. |
| `scripts/` | `compat-check.sh` (the cutover matrix), `api-doc-check.sh`, `trusted-ip-check.sh`, `release.sh`, and `probes/`. |
| `npm/`, `Formula/`, `install.sh` | Three of the four install channels. The fourth, `cargo install recall-sync`, needs no file here. |
| `tests/fixtures/` | A database the retired Node server actually wrote, so the cutover stays testable without it. |

## License

MIT — see `LICENSE`.
