# Installing and using Recall

Recall is a single Go binary. The same artifact runs the server
(`recall serve`) and everything on a developer machine — install once per
machine, then run `recall init` once per project.

## Install

**npm** (or bun/pnpm):

```sh
npm install -g @pimlabs/recall
```

Downloads the prebuilt binary for your platform and verifies it against the
release's checksums. Nothing about the runtime is Node — that's just the
delivery mechanism.

**Homebrew** — the tap needs its URL given explicitly, since this repo isn't
named `homebrew-recall`:

```sh
brew tap pimlabs/recall https://github.com/pimlabs/recall
brew install pimlabs/recall/recall
```

**Plain curl**, for machines with neither:

```sh
curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
```

Installs to `~/.local/bin/recall`. Override with `RECALL_BIN_DIR`, or pin a
version with `RECALL_VERSION=v0.1.0`.

**From source**, if you have Go:

```sh
go build -o recall ./cmd/recall
```

Supported: macOS and Linux, x64 and arm64. Windows needs WSL. There are no
runtime dependencies — no `jq`, no `curl`, no Node — except on the server,
where the semantic merge shells out to the `claude` CLI.

## Set the environment variables

Once per machine, in your shell profile:

```sh
export RECALL_URL="https://your-recall-host"
export RECALL_TOKEN="<your token>"
```

See `token-setup.md` for generating the token and for what a claude.ai
cloud environment additionally needs.

## Enable sync for a project

From inside the project:

```sh
recall init
```

That merges the hook wiring into the project's own `.claude/settings.json`
— idempotently, appending to any hooks already there rather than replacing
them, and preserving the file's existing key order so the diff stays small.
Then commit it:

```sh
git add .claude/settings.json
git commit -m "Enable Recall memory sync"
```

**The commit is the point, not a formality.** The hook config has to be in
the project's repo for a fresh clone — especially an ephemeral cloud
session that has never seen your machine — to pick sync up with no setup of
its own. See `CLAUDE.md`'s ground rules.

## Check it's working

```sh
recall status
```

Reports, for the project you're standing in: the derived `project_key`,
where Claude Code's memory directory actually is on this machine, how many
memory files exist locally, whether the hooks are wired, whether
`RECALL_URL`/`RECALL_TOKEN` are set, whether the server answers, whether
merge is actually configured server-side, and how many files the server
holds for this project.

`recall status --json` prints the same thing machine-readably.

## Cloud environments need the binary too

A claude.ai cloud session runs the hooks from the repo it cloned, but
`recall` itself has to already be on `PATH` there — it isn't part of the
repo. Install it as part of that environment's setup, the same one-time
place you set `RECALL_TOKEN` and `CLAUDE_CODE_REMOTE_MEMORY_DIR`:

```sh
npm install -g @pimlabs/recall
```

This is per-environment, not per-project or per-session.

## Running the server

The same binary:

```sh
RECALL_TOKEN=... RECALL_DB_PATH=/data/recall.db recall serve
```

In practice it runs in Docker behind a Cloudflare tunnel — see
`../deploy/README.md`, which also covers the one-time `claude setup-token`
step that enables semantic merge.

## Releases

Binaries are published by a GitHub Actions workflow when a `v*` tag is
pushed. Until the first tag exists, install via Homebrew `--HEAD` or build
from source; npm and `install.sh` both need a published release to download
from, and both say so plainly rather than failing obscurely.

## The shell implementation this replaces

`hooks/recall-push` and `hooks/recall-pull` are still in the repository.
They remain the rollback path for a machine not yet on the binary, and any
project already wired to `$CLAUDE_PROJECT_DIR/hooks/recall-push` keeps
working unchanged — the wire format is identical in both directions. New
projects should use `recall init`, which wires `recall push` / `recall pull`
instead so nothing needs copying into the project at all.
