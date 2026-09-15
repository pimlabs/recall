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
`recall init` and `recall backfill` once per project.

## Install

Four channels, all delivering the same binary. Pick whichever your machine
already has.

| Channel | Command |
|---|---|
| **npm** / bun / pnpm | `npm install -g @pimlabs/recall` |
| **Homebrew** | `brew install pimlabs/tap/recall` |
| **curl** | `curl -fsSL https://recall.pimlabs.id/install \| bash` |
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
curl -fsSL https://recall.pimlabs.id/install | bash
```

Installs to `~/.local/bin/recall`. Override with `RECALL_BIN_DIR`, or pin a
version with `RECALL_VERSION=v0.1.0`. It will tell you if that directory
isn't on your `PATH`.

That URL is a Cloudflare Worker that fetches `install.sh` from `main` on
every request, so what runs is whatever this repository says right now —
there is no second copy to fall behind. Read it before running it if you
would rather not pipe a stranger's script into a shell:

```sh
curl -fsSL https://recall.pimlabs.id/install | less
```

The same bytes, with their history, are at
[`install.sh`](https://github.com/pimlabs/recall/blob/main/install.sh) —
that is the file the Worker serves, and
[`install-worker.js`](https://github.com/pimlabs/recall/blob/main/install-worker.js)
is the Worker itself.

**It is not your server's address.** `recall.pimlabs.id` is where this
project publishes its installer; `RECALL_URL` is the host *you* deploy to,
and the two have nothing to do with each other.

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
another are one key rather than two. A value that contains whitespace or
starts with `global:` is refused — the derived key stands and `recall status`
says the declaration was ignored, rather than letting it pass silently.

An empty value is a different case: it reads as *unset* before anything gets
the chance to refuse it, so the derived key stands with nothing to report.
`recall status` can still see it when it was declared in a settings file, and
says so — that is the case where something was plainly meant and did nothing.

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

**Declared here, it beats the `export` above.** Claude Code applies an `env`
block to every process it spawns and *replaces* the value inherited from the
shell, so the file wins wherever the two disagree — on this machine and on
every other one.

Four places can set any of Recall's variables. Lowest precedence first:

1. your shell
2. `~/.claude/settings.json` — yours, every project
3. the project's committed `.claude/settings.json` — this project, every machine
4. the project's `.claude/settings.local.json` — this project, this machine

The last one that names a variable wins, and `recall status` prints which file
that was. The fourth is worth knowing about even if you never write one: it is
untracked, so it is invisible to everyone else working on the repository and it
outranks the file they can see. **Add it to `.gitignore` if you use it** —
nothing does that for you, and it is the natural place to accidentally commit a
token.

**Changing it on a project that has already synced orphans that project's
memory.** The server files every file under the key it was pushed with and
moves nothing, so the old history stays where it is and the new key starts
empty. Nothing is lost, and nothing follows either — but the files are still
on your disk, so `recall backfill` refills the new key from them.
[`deploy/README.md`](../../deploy/README.md) has the SQL for renaming or
removing the rows left under the old one, which is only needed to tidy up.

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

Nothing syncs differently if you leave `RECALL_GLOBAL_KEY` unset — though
`recall backfill` will then name anything in `global/` as belonging to no
scope, because with the key unset it does.

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

### Memory that was already here: `recall backfill`

Wiring a project does not send anything. The push hook fires when Claude
edits a memory file, so a project that already had memory when Recall
arrived keeps it to itself: each file reaches the server only if Claude
happens to edit it again, and a `touch` from the shell fires no hook at all.
Until it is sent, that memory is exactly as safe as the one machine it is on.

Run this once, from inside the project:

```sh
recall backfill
```

**On a machine that is not your first, pull before you backfill** — start a
Claude session, which runs the pull hook. Otherwise every file the server
already has is reported as a disagreement, and you will be reading a long
list of things that are not actually wrong. After a pull, what is left to
send is only what this machine has and the server has never seen.

It asks the server what it already holds, then sends what is missing. It
prints a count and then names anything it left behind.

**It deliberately sends less than everything on your disk.** `POST /sync`
overwrites in place: no timestamp comparison, no conflict, whatever arrives
last wins. On a second machine, "push my whole directory" is not a way to
rescue memory — it is a way to delete the first machine's newer work. So:

| It leaves alone | Because |
|---|---|
| A file the server holds with different content | That is a real disagreement, not a gap — see below. |
| A file the server has tombstoned | It was deleted on another machine. Sending it back is a resurrection, not a sync — `recall pull` will remove it here. |
| Anything under `global/` when `RECALL_GLOBAL_KEY` is unset | It belongs to no scope. Filing personal notes into one repository's history is the one outcome worth refusing. |
| Anything that is not text | It cannot be sent at all. This is also how a `.DS_Store` or a pasted screenshot is quietly skipped. |

**Nothing on this machine reconciles a held-back file**, and the obvious
moves all pick a winner rather than merging:

- `recall pull` overwrites your local copy with the server's, without
  comparing them. Copy yours aside first if you want to keep it.
- **Deleting your local copy deletes the server's too.** Deletes propagate,
  so that is the one move that loses both versions instead of one.
- Editing the file in your editor sends nothing at all: the push hook is
  `PostToolUse`, so it fires when *Claude* writes a memory file, not when you
  do.

The one thing that actually merges is the server. Ask Claude to edit the
file, and the push that follows is merged against the stored version — when
`recall status` reports `merge: ready`. When it reports that merge is not
configured, the server keeps whatever arrived last instead.

**A file the server will never accept is skipped; a refusal about the run
stops it.** Those are different things, and treating them alike would mean
one awkward filename made every file sorted after it permanently unsendable.
A name the server's validator rejects, or a file past its 5 MiB limit, is
reported and stepped over. The rate limit is the one that ends the run: every
file is its own request, the server allows 60 a minute per address by
default, and that budget is shared with the push and pull hooks running in
the session you typed this in. Stopping leaves the rest of it for them.
Re-running carries on and costs nothing for what already went — the second
run finds those files on the server and skips them. A directory much larger
than the limit therefore takes a few runs, a minute apart.

**What it sends is decided from one snapshot**, taken when it asked. If
another machine pushes a file *during* a long run, that file is not in the
snapshot and can still be overwritten. The HTTP API has no conditional write,
so nothing here can close that window — it is small, and worth knowing about
rather than pretending away.

It exits `0` when it finished, `2` when it could not read the server or
stopped early, and `1` when `RECALL_URL` or `RECALL_TOKEN` is not set. A `2`
from a stopped run means *not done yet*, not broken — worth knowing before
you put this in a `set -e` setup script.

A run that finishes also leaves behind the baseline that makes deletes
detectable; until a project has one, `recall push` cannot tell a file you
deleted from one that was never there. A run that stopped early does **not**
write one, and says so: the baseline is a claim that this disk and the server
have been compared, and half a comparison is not that. And with global sync
on, it refreshes `MEMORY.md`'s links to your `global/` files before sending —
after asking the server, so a run that cannot reach it changes nothing at all.

## Check it's working

```sh
recall status
```

Reports, for the project you're standing in: the `project_key` and whether
it was derived or declared, where Claude Code's memory directory actually is
on this machine, how many memory files exist locally, whether the hooks are
wired, which variables a settings file declares and which of those are
overriding your shell, the global scope and whether `MEMORY.md` links it,
whether `RECALL_URL`/`RECALL_TOKEN` are set, whether the server answers,
whether merge is actually configured server-side, and how many files the
server holds for this project.

It reports them as the *hooks* would see them, not as your shell holds them
— which is the same thing on most machines and emphatically not the same
thing on a project that declares anything in `.claude/settings.json`.

Anything you set that Recall could not use is called out here too — a
refused `RECALL_PROJECT_KEY` or `RECALL_GLOBAL_KEY` still leaves sync
working, which is exactly why it would otherwise go unnoticed. Two more of
that kind: a variable declared as an empty string, which turns the setting
off *and* hides whatever the shell had behind it; a variable whose value is
not a string, such as `"RECALL_PROJECT_KEY": 12345`, which cannot become an
environment variable at all; and a settings file that exists but cannot be
read — bad JSON, JSON that is not an object, or a permissions problem. Claude
Code cannot read that last one either, so nothing it declares reaches the
hooks at all.

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
