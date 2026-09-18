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

- **`RECALL_MACHINE_KEY` adds a third scope, for memories true of one machine
  and no other.** Files under `<memory dir>/machine/` sync under
  `machine:<key>` and come back only on a machine declaring the same key. The
  global scope was the only place for this content and is actively wrong for
  it: "this machine has 8 GB" is false on the next machine, and memory that is
  confidently wrong is worse than none.

  **Off unless you set it**, and on an ephemeral cloud session you should not:
  it is a new machine every time, and facts about your laptop do not describe
  it. With no key set, `machine/` is ignored — not filed under the project.

  If you were naming a machine through `RECALL_PROJECT_KEY`, that still works
  and is still right for a directory of general work with no repository. The
  new scope is for the case it cannot cover: machine facts sitting *beside* a
  real project's memory.

  `recall status` gains a `machine` line; `recall backfill` now names the
  variable that would sync a skipped file instead of always suggesting
  `RECALL_GLOBAL_KEY`.
- **A memory directory whose global folder is spelled `Global/` no longer
  syncs into the project's history.** It used to, with global sync on or off
  — nothing under it matched the reserved `global/`, so the project scope,
  which matches everything left, took it. On macOS that folder *is* the
  global one: the default filesystem is case-insensitive, so what a user and
  Claude Code both see as the global directory was being filed into one
  repository's memory, where every future session on that project would read
  it.

  Recall now refuses the path instead of guessing, because the right answer
  depends on a filesystem it cannot see — on Linux `Global/` really is a
  separate directory. `recall backfill` skips the file and names it, and
  `recall status` reports the directory with what to rename it to. **If you
  have such a directory, rename it to `global/` and those files sync as you
  meant.** Anything already pushed under the wrong scope stays where it is;
  Recall will not move it for you.

  Only the directory name is reserved, and only at the top of the memory
  directory: `globalish/`, `Global.md` and `topics/Global/` are untouched.
- **`recall --version` and `recall -V` work.** They used to fail with
  `error: unexpected argument '--version' found` and exit 2, because the flag
  was disabled in favour of the `recall version` subcommand. Both exist now
  and print the same bytes, including the commit the binary was built from.
  `recall version` is unchanged, so nothing that used it needs to move.

- **The crate is `recall`, and the version jumps to 0.2.0.** Installing from
  crates.io is now:

  ```sh
  cargo install recall
  ```

  Through v0.1.0 it was `cargo install recall-sync`, because `recall` and
  `recall-cli` were both taken on crates.io when that release's preflight ran.
  The name has since been transferred, and this is a **break** rather than a
  tidy-up, described here as the file's own rule asks: **`cargo install
  recall-sync` stops working.** Anyone tracking that crate has to move to
  `recall`. In practice that is one person, which is why it was worth doing at
  all.

  The way it stops is worth stating, because the error will not name the crate
  you typed. `recall-sync` itself stays on the index; what is yanked is
  `recall-paths` 0.1.0, one of the four libraries it depends on, and 0.1.0 was
  that library's only version. Cargo will not pick a yanked version for a
  fresh resolution, so the install fails while resolving dependencies. The
  library is yanked because 0.2.0 folded it into `recall-hooks` and it will
  never be published again; leaving a crate on the index that nothing will
  ever update is worse than withdrawing it.

  The binary has always been `recall` and still is. Nothing about the CLI, the
  hook commands in a committed `.claude/settings.json`, the HTTP surface or the
  database changes, so npm, Homebrew and `install.sh` are unaffected — only the
  cargo line moves.

  **0.2.0, not 0.1.1**, and the reason generalises: crates.io never lets a
  version number be reused, and `recall` 0.1.0 belongs permanently to the
  unrelated crate that published it in 2019. 0.1.x was never available to take.
  A transferred name arrives with someone else's history attached.

- **`recall backfill` sends memory that predates Recall.** The push hook only
  ever sent the file it was handed, so a project whose memory directory
  already held files when Recall arrived kept them to itself: each one
  reached the server only if Claude happened to edit it again, and a `touch`
  from the shell fires no hook at all. Catching up meant one `curl` per file.
  Recall could protect only the memory written after it was installed.

  It sends less than everything on disk, on purpose. `POST /sync` overwrites
  in place — no timestamp comparison, no conflict — so a bulk push of a local
  directory is safe on the first machine and destructive on every other one.
  `backfill` asks what the server holds first and sends only what is missing.
  A file the server has with different content is left alone and named in the
  output; so is one the server has tombstoned, because re-sending that undoes
  a delete rather than filling a gap. Files under `global/` with
  `RECALL_GLOBAL_KEY` unset belong to no scope and are refused rather than
  filed into the project's history.

  Two kinds of refusal are treated differently, because treating them alike
  meant one awkward filename made every file sorted after it permanently
  unsendable. A file the server will never accept — a name its validator
  rejects, a body past its size limit — is reported and stepped over. A
  refusal about the run, the rate limit above all, ends it and says where and
  how many files went unreached: every file is one request against a budget of
  60 a minute per address by default, shared with the hooks in your session.
  Re-running carries on and re-sends nothing.

  A run that finishes leaves behind the baseline that makes deletes
  detectable, which a project that never completed a pull did not have. A run
  that stopped early deliberately does not, and says so.

- **`recall status` no longer disagrees with the hooks it diagnoses.** Claude
  Code applies a settings file's `env` block to the processes it spawns and
  *replaces* what your shell exported. So a `RECALL_PROJECT_KEY` declared in
  a project's `.claude/settings.json` — which is where these docs tell you to
  put it, because it is the one place that travels with a repository — was
  the key the hooks actually synced under, while `recall status`, typed into
  a shell, read the process environment and reported the derived one.

  Every command now resolves configuration the way a hook does, each layer
  over the last: the user-level `settings.json`, then the project's
  `.claude/settings.json`, then its untracked `.claude/settings.local.json`,
  all of them above the shell. That includes `recall promote`, which is also
  typed by hand and had the same bug — on a project with a declared key it
  would have pushed the move into the derived key's history. And `recall
  init` stops warning that `RECALL_URL` or `RECALL_TOKEN` is unset when a
  settings file already supplies it.

  Status also names the file each value came from, says when a settings file
  is overriding your shell, flags a variable declared as an empty string
  (Recall reads empty as unset, so such a declaration turns the setting off
  *and* hides the shell value behind it), names a variable whose value is not
  a string and so could not be set at all, and reports a settings file that
  exists but cannot be read — bad JSON, JSON that is not an object, or a
  permissions problem. Claude Code cannot read that one either, so nothing it
  declares is in effect anywhere. `--json` gains `declared_env`,
  `ignored_env` and `unreadable_settings`, each omitted when empty. None of
  them carries a value: `RECALL_TOKEN` is one of the variables reported.

The rest of the work since v0.1.0 has been the release pipeline and the shape
of the tree:

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
