# Connecting a machine to Recall

This document is the **client** half: getting the binary onto a machine and
opting projects into sync. It assumes a server already exists, because every
step below needs a `RECALL_URL` and a `RECALL_TOKEN` that only a server can
give you. If you do not have one yet, start at
[`../../deploy/README.md`](../../deploy/README.md) and come back.

"Machine" means yours — a second laptop, a desktop, a fresh claude.ai cloud
session. Recall is single-owner by design: one token, no accounts. Nothing
here sets up access for anyone else.

Recall is a single Rust binary, and the same artifact runs both halves
(`recall serve` is the server). Install once per machine, then run
`recall init` once per project.

## Install

Four channels, all delivering the same binary. Pick whichever your machine
already has.

| Channel | Command |
|---|---|
| **npm** / bun / pnpm | `npm install -g @pimlabs/recall` |
| **Homebrew** | `brew install pimlabs/tap/recall` |
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

```sh
brew install pimlabs/tap/recall          # latest release
brew install --HEAD pimlabs/tap/recall   # built from main, between releases
```

No separate `brew tap` step and no URL: the formula lives in
`pimlabs/homebrew-tap`, and Homebrew resolves `pimlabs/tap` to a repository
named `homebrew-*` on its own. A formula kept in this repository could not be
found that way, since `recall` is not named `homebrew-recall`.

The release install is a **download**, not a build — the same archive npm and
`install.sh` fetch, with the same checksum verified by Homebrew. `--HEAD`
still compiles, because there is nothing prebuilt for `main` to point at, and
that is the one case where you need a Rust toolchain.

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

## When the derived project key is wrong

Recall files a project's memory under `owner/repo`, taken from the git
remote. That is right nearly always. Four cases where it isn't:

- **No remote at all** — a local-only repo, or a directory that is not one.
  The key falls back to `local:<this checkout's path>`, so the same project
  on a second machine is a second project with a second history.
- **A monorepo** whose sub-projects should each keep their own memory — or,
  the other way round, should share one.
- **A fork** that wants to keep reading the upstream's memory.
- **Nested groups** (GitLab subgroups), where two groups holding a repo of
  the same name collapse onto one key.

Declare the key instead of deriving it:

```sh
export RECALL_PROJECT_KEY="acme/app"
```

It is trimmed and lowercased, so `Acme/App` on one machine and `acme/app` on
another are one key rather than two. A value that is empty, contains
whitespace, or starts with `global:` is refused — the derived key stands and
`recall status` says the declaration was ignored, rather than letting it pass
silently.

**This is per-project, not per-machine.** Exporting it from your shell
profile would give *every* project the same key and pool their memory into
one bucket. The right home is the project's own `.claude/settings.json` —
the same file `recall init` writes, committed for the same reason:

```json
{
  "env": {
    "RECALL_PROJECT_KEY": "acme/app"
  }
}
```

Committed, it is the one form that reaches every machine and every fresh
cloud session without per-machine setup. (`recall init` does not write it for
you: which projects share a key is a decision only you can make.)

**Changing it on a project that has already synced orphans that project's
memory.** The server files every file under the key it was pushed with and
moves nothing, so the old history stays where it is and the new key starts
empty. Nothing is lost, and nothing follows either —
[`deploy/README.md`](../../deploy/README.md) has the SQL for renaming or
removing a key you no longer want.

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
  established, is in [`memory-loading-findings.md`](../history/memory-loading-findings.md).

Nothing changes if you leave `RECALL_GLOBAL_KEY` unset.

### Putting a note there: `recall promote`

Claude writes a note about *you* while you happen to be working in one
repository. Move it into the global scope:

```sh
recall promote topics/user.md
```

The path is relative to the memory directory (`recall status` prints where
that is), or absolute. The note is stored under the global key, tombstoned
under this project's key, moved into `global/` on disk, and linked from
`MEMORY.md`. Every other wired project picks it up at its next session start.

It is a move, not a copy. A note left in both scopes would be pulled twice
into every future session of this project, and the two copies would drift
apart the first time either was edited.

It keeps only its file name — `topics/user.md` becomes `global/user.md`.
A project's own folders (`topics/auth.md` is auth *in this repository*) mean
nothing in a scope that has no project.

The project's `MEMORY.md` loses its link to the old path, since that link is
now dead and Recall is what killed it — and that one removal is pushed, so it
does not come back on the next pull or linger on your other machines. Every
other line in the file is left exactly as it was.

Where it refuses rather than guesses:

| It says | Because |
|---|---|
| global sync is not configured | `RECALL_GLOBAL_KEY` is unset, so there is nowhere to promote to. |
| … is already a global memory | It is already under `global/`. |
| `MEMORY.md` is this project's index | It is regenerated after every push and pull, so the move would quietly undo itself — while a copy of one repo's index sat in every other project. |
| … already exists and holds a different note | A different note holds that name globally. Rename yours and run it again: overwriting is the only one of the options that cannot be undone. |

Interrupted runs are safe to repeat. Nothing is sent or moved until the
store succeeds, the new copy is written before the old one is removed, and a
re-run that finds an identical copy already in `global/` finishes the move
instead of refusing it.

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

**Every machine still needs the binary.** The hook command is guarded — a
machine without Recall installed silently does nothing rather than erroring
on every edit — so a clone is never *broken* by the missing binary, but it
does not sync either. On a claude.ai cloud environment that means installing
Recall as part of that environment's setup, alongside setting the variables
in [`token-setup.md`](token-setup.md).

**The commit is the point, not a formality.** The hook config has to be in
the project's repo for a fresh clone — especially an ephemeral cloud
session that has never seen your machine — to pick sync up with no setup of
its own. See `CLAUDE.md`'s ground rules.

## Check it's working

```sh
recall status
```

Reports, for the project you're standing in: the `project_key` and whether
it was derived or declared, where Claude Code's memory directory actually is
on this machine, how many memory files exist locally, whether the hooks are
wired, the global scope and whether `MEMORY.md` links it, whether
`RECALL_URL`/`RECALL_TOKEN` are set, whether the server answers, whether
merge is actually configured server-side, and how many files the server
holds for this project.

Anything you set that Recall could not use is called out here too — a
refused `RECALL_PROJECT_KEY` or `RECALL_GLOBAL_KEY` still leaves sync
working, which is exactly why it would otherwise go unnoticed.

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

In practice it runs in Docker behind an ingress — a Cloudflare Tunnel or an
existing Traefik, one compose file each. See
[`deploy/README.md`](../../deploy/README.md), which also covers backups and
the one-time `claude setup-token` step that enables semantic merge.

## Releases

Binaries are published by a GitHub Actions workflow when a `v*` tag is
pushed: four archives — macOS and Linux, x64 and arm64 — plus a
`checksums.txt` that npm's installer verifies against. `v0.1.0` was the
first, on 2026-09-14.

npm and `install.sh` download those archives, so both are tied to a released
version; npm's `postinstall` looks for a release named after its *own*
version and says so plainly rather than failing obscurely. Homebrew and
`cargo install --git` build from source and need no release at all, which is
why `brew install --HEAD` worked before any tag existed.

`CHANGELOG.md` says what changed between versions, and what deliberately does
not appear there.

## If a project is still wired to the old shell hooks

The bash implementation that preceded this one has been removed from the
repository. A project whose `.claude/settings.json` still calls
`$CLAUDE_PROJECT_DIR/hooks/recall-push` will stop working once that project
no longer carries those scripts.

The fix is one command in the project:

```sh
recall init
```

That rewrites the hooks to `recall push` / `recall pull`, which need nothing
copied into the project at all. The wire format is identical in both
directions, so there is nothing to migrate server-side. The old scripts are
still in git history if you need them (`git log -- hooks/`).
