# Installing and using Recall

Recall is a single Rust binary. The same artifact runs the server
(`recall serve`) and everything on a developer machine — install once per
machine, then run `recall init` once per project.

## Install

Four channels, all delivering the same binary. Pick whichever your machine
already has.

| Channel | Command |
|---|---|
| **npm** / bun / pnpm | `npm install -g @pimlabs/recall` |
| **Homebrew** | `brew tap pimlabs/recall https://github.com/pimlabs/recall`<br>`brew install pimlabs/recall/recall` |
| **curl** | `curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh \| bash` |
| **cargo** | `cargo install recall-sync` |

Supported: macOS and Linux, x64 and arm64. Windows needs WSL. There are no
runtime dependencies — no `jq`, no `curl`, no Node — except on the server,
where the semantic merge shells out to the `claude` CLI.

### npm (or bun, or pnpm)

```sh
npm install -g @pimlabs/recall
bun install -g @pimlabs/recall     # works the same way
```

A `postinstall` script downloads the prebuilt binary for your platform and
**verifies its SHA-256 against the release's `checksums.txt`** before making
anything executable. Nothing about the runtime is Node — that is only the
delivery mechanism.

### Homebrew

The tap needs its URL given explicitly, since this repo isn't named
`homebrew-recall` and Homebrew only infers that convention:

```sh
brew tap pimlabs/recall https://github.com/pimlabs/recall
brew install pimlabs/recall/recall          # latest tagged release
brew install --HEAD pimlabs/recall/recall   # straight from main
```

Built from source rather than pulling a release binary — Homebrew already has
a Rust toolchain available as a build dependency, and it means `--HEAD` works
against `main` between releases.

### curl

For a machine with neither npm nor Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/pimlabs/recall/main/install.sh | bash
```

Installs to `~/.local/bin/recall`. Override with `RECALL_BIN_DIR`, or pin a
version with `RECALL_VERSION=v0.1.0`. It will tell you if that directory
isn't on your `PATH`.

### cargo

```sh
cargo install recall-sync                                   # from crates.io
cargo install --git https://github.com/pimlabs/recall recall-sync   # from main
```

The crate is `recall-sync`; the binary it installs is **`recall`**. They
differ because both `recall` and `recall-cli` were already taken on crates.io
by unrelated projects — a crate name is global and first-come, while a binary
name is only yours to collide with. `fd-find` installing `fd` is the same
situation.

The `--git` form needs no release, so it is also the answer for anything
unreleased.

### From a clone

```sh
git clone https://github.com/pimlabs/recall && cd recall
cargo build --release -p recall-sync
# binary at target/release/recall
```

The first build is slow — `rusqlite` compiles SQLite from C.

## Set the environment variables

Once per machine, in your shell profile:

```sh
export RECALL_URL="https://your-recall-host"
export RECALL_TOKEN="<your token>"
```

See `token-setup.md` for generating the token and for what a claude.ai
cloud environment additionally needs.

## Memories that follow you into every project

By default Recall syncs each project's memory under its own key, and a note
about *you* — your preferred editor, how you like commits worded — is stuck
in whichever repository you happened to be in when Claude wrote it down.

Set one more variable, on every machine, to fix that:

```sh
export RECALL_GLOBAL_KEY="your-name"      # any stable string; the same one everywhere
```

Anything in `<memory dir>/global/` is then synced under that key instead of
the project's, and pulled into **every** project you have wired. `recall
status` shows the key, the file count, and whether `MEMORY.md` links them.

A few things worth knowing:

- **`global/` is reserved.** A project topic file must not live there; with
  global sync on it would be shared with every project, and with it off it is
  ignored rather than swept into the current project.
- **Turn it on everywhere or nowhere.** `recall pull` maintains links in
  `MEMORY.md`, and `MEMORY.md` is itself synced per project, so a machine
  with global off will carry links to files it never fetches.
- **Writing the file is not enough for Claude to read it** — it has to be
  linked from `MEMORY.md`, which Recall does for you. Why, and how that was
  established, is in [`memory-loading-findings.md`](memory-loading-findings.md).

Nothing changes if you leave `RECALL_GLOBAL_KEY` unset.

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
