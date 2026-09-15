# Changelog

What changed for someone who already installed Recall. Not a commit log —
`git log` is better at that, and the GitHub release notes already list every
merged pull request.

**What belongs here:** anything that changes what the binary does, what the
server stores or answers, what an install or upgrade requires of you, or what
a setting means. **What does not:** refactors, documentation, CI, and tests,
however large — a reader of this file is trying to find out whether upgrading
will change something under them.

Versions follow [semver](https://semver.org). Below 1.0 the minor number is
where breaking changes live, and this project has exactly one user, so a
break will be described here in full rather than smoothed over.

## Unreleased

Nothing that changes the binary. The work since v0.1.0 has been the release
pipeline and the shape of the tree:

- The macOS build jobs moved off `macos-13`, which GitHub retired — the label
  is no longer served at all, so the job queued forever and no release was
  ever published. arm64 moved off the now-deprecated `macos-14` at the same
  time.
- Released binaries stamp the commit they were **built** from rather than the
  one that triggered the workflow. The two differ whenever a tag is rebuilt
  by hand, and v0.1.0's first set of archives named the wrong commit because
  of it. They were rebuilt; `recall version` now prints `d85d225`, which is
  what `v0.1.0` points at.
- **The `curl` install has a short URL:**

  ```sh
  curl -fsSL https://recall.pimlabs.id/install | bash
  ```

  A Cloudflare Worker that fetches the same `install.sh` from `main` on every
  request, so nothing is copied and nothing can fall behind. The old
  `raw.githubusercontent.com/pimlabs/recall/main/install.sh` keeps working —
  it is the file the Worker serves.

- **Homebrew is one line and one download:**

  ```sh
  brew install pimlabs/tap/recall
  ```

  Two changes in one. The formula moved to `pimlabs/homebrew-tap`, whose name
  lets Homebrew resolve `pimlabs/tap` without a URL, so the separate
  `brew tap pimlabs/recall <url>` step is gone. And the release install now
  takes the prebuilt archive — the same one npm and `install.sh` fetch —
  instead of compiling Rust and SQLite's C amalgamation locally. Seconds
  rather than minutes, and no Rust toolchain needed.

  `brew install --HEAD pimlabs/tap/recall` still builds from `main`, since
  there is nothing prebuilt for `main` to point at.

  If you installed the old way, `brew untap pimlabs/recall` after switching;
  nothing breaks if you don't, but the old tap will never update again.

## 0.1.0 — 2026-09-14

First release. Everything before this lived only in the repository.

Recall syncs Claude Code's auto-memory between machines and fresh cloud
sessions, through a server you host yourself. One Rust binary is both halves:
`recall serve` runs the server, everything else runs beside your editor.

- **Hooks, in the project's own `.claude/settings.json`.** `recall init`
  wires them and you commit the result, which is why a fresh clone in a cloud
  session picks sync up with no setup at all. `PostToolUse` pushes a memory
  file when Claude edits it; `SessionStart` pulls.
- **Semantic merge.** Two machines editing the same memory file are
  reconciled by the local `claude` CLI rather than by last-write-wins. It
  needs a logged-in CLI on the server; without one it degrades to
  last-write-wins and says so at `GET /health`.
- **Scopes.** A note about *you* rather than about a repository can live in
  the global scope and follow you into every project — `recall promote` moves
  one there.
- **`RECALL_PROJECT_KEY`** declares the key a project syncs under, for the
  cases no derivation reaches: a repository with no remote, a monorepo's
  sub-projects, a fork that wants to keep reading upstream's memory.
- **Four install channels** — npm, Homebrew, `install.sh`, and
  `cargo install recall-sync`. The binary is `recall` in all four.

### Frozen from here

The SQLite schema, the HTTP JSON down to field order and the
`null`-versus-`""` tombstone distinction, the timestamp format, and the
environment variable names. These are not style: a database written by an
earlier implementation is already in production, and the checkers in
`scripts/` assert every one of them on each CI run. Changing any of them is a
breaking change and will be described here as one.
