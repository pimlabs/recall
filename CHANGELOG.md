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

- **`recall connect` enrols this machine as a device** when the server
  supports it (this release's server does). It makes an Ed25519 key pair,
  shows a code and the key's fingerprint, and is approved either from a
  machine already enrolled as admin (`recall devices approve <code>`) or,
  for your first machine, with the server's `RECALL_TOKEN` after asking,
  which makes it an admin device. From then on the machine signs every
  request and sends no token, and the token `connect` had saved in
  `~/.recall/credentials.toml` is removed; it still works on the server.
  `--yes` and `--name` still work for scripts. Against an older server,
  `connect` saves the token exactly as before.
- **The device key is a file, `~/.recall/device.key`**, created readable by
  you only, one key per server. Not the OS keychain: a hook runs on every
  memory write and must never stop for a keychain dialog, which macOS shows
  after an upgrade. `recall disconnect` removes it too.
- **Cloud sessions enrol themselves with `RECALL_AUTHKEY`.** Set an
  authkey on the cloud environment instead of `RECALL_TOKEN`, and
  each session's first `recall pull` enrols it (approved at once,
  ephemeral) and carries on. Nothing is typed, and a failure falls back to
  `RECALL_TOKEN` or leaves memory untouched, as a pull always has.
- **A revoked or swept device enrols again by itself** when
  `RECALL_AUTHKEY` is set: the hook that is refused enrols once and
  retries. Without it, the hook says to run `recall connect`, and the
  session still starts.
- **New command: `recall devices`.** `list` (scope, ephemeral, last seen,
  agent), `approve <code>` (shows the machine's name, agent and fingerprint
  and asks first; `--fingerprint` refuses a key with any other, `--admin`
  gives the admin scope) and `revoke <name>`. `--yes` and `--json` for
  scripts.
- **New command: `recall authkey`.** `create --tag cloud --expires 90d`
  (the key is shown once), `list` and `revoke <id>` (`--revoke-devices`
  revokes what it enrolled too), with `--json`.
- **`recall doctor` reports the device**: its name, scope and where its
  key lives, checked with the server. It warns while a machine the server
  could enrol still uses the shared token, and while a token is kept that
  an enrolled machine no longer sends. `RECALL_TOKEN` unset is no longer a
  failure on a machine with a device key or `RECALL_AUTHKEY`.
- **`recall status --json` gains** `auth` (`device`, `bearer` or `none`),
  `device` (id, name, scope, ephemeral, key storage and file, and whether
  the server confirmed it), `device_file`, `device_error`,
  `device_file_exposed`, `enroll_key_set` and `server_devices`. Every
  existing field is unchanged.

- **The server enrols devices.** A machine can now be enrolled with a key
  pair of its own and sign its requests (RFC 9421, Ed25519) instead of
  sending `RECALL_TOKEN`: it asks `POST /v1/devices/enroll` for a short
  code, the owner approves the code, and the machine is a device that can
  be listed and revoked on its own. Cloud sessions can enrol with an
  expiring authkey instead of a code. See "Devices" in
  `docs/reference/api.md`.
- **`RECALL_TOKEN` works exactly as before**, on every route, and is how
  the first device is approved. Nothing that worked stops working.
- **A device's pushes carry its own name.** A push a device signed is
  stored with that device's name as `source_env`, whatever the push says,
  so one machine cannot write as another. Pushes with `RECALL_TOKEN` keep
  the name they send.
- **New routes:** `POST /v1/devices/enroll`, `POST /v1/devices/enroll/poll`,
  `GET /v1/devices/me` for a device to check the server knows it, and, for
  the owner, `GET /v1/devices/pending/{user_code}` (what a code would
  approve, name and key fingerprint, before approving it),
  `POST /v1/devices/approve` (which can name the fingerprint it expects),
  `POST /v1/devices/deny`, `GET /v1/devices`, `POST /v1/devices/{id}/revoke`,
  `POST /v1/authkeys`, `GET /v1/authkeys` and
  `POST /v1/authkeys/{id}/revoke`.
- **Device names are plain and unique.** A name with control, format or
  invisible characters (a zero-width space, a right-to-left override) is
  refused, no two unrevoked devices share a name or names that read alike
  (`Laptop`, or `lаptop` with a Cyrillic `а`, beside `laptop`), and a
  device an authkey enrols is named by the server after the key's
  tag. A name already taken is refused when its code is approved, not when
  the machine enrols, so the unauthenticated enrol route says nothing about
  which names exist.
- **Authkeys** enrol ephemeral devices unless told otherwise, enrol
  at most 25 unrevoked devices unless `max_devices` says otherwise, and can
  be revoked together with every device they enrolled.
- **`GET /admin/stats` needs the `admin` scope from a device.** Nothing
  changes for `RECALL_TOKEN`, which is all anything uses today.
- **Signed requests are checked before their body is read**, the
  enrolment routes take 8 KiB bodies, one address may have five
  enrolments waiting, and a signature dated up to five seconds after the
  server started is refused, since the nonces that would catch its replay
  went with the process before. A signature's `created` may be a minute
  behind the server's clock but only five seconds ahead of it.
- **The rate limit counts an IPv6 client by its /64**, on every route,
  `/sync` included, rather than address by address, so one machine
  cannot give itself a fresh bucket per request. An IPv4 address written
  as IPv6 counts as the IPv4 address. Clients on IPv4 see no change.
- **`GET /.well-known/recall` lists `device-sig-v1`** after `bearer` in
  `auth.methods`, and a new `devices` capability.
- **New setting: `RECALL_EPHEMERAL_DEVICE_TTL_HOURS`** (default 24). An
  ephemeral device, one a cloud session enrolled with an ephemeral
  authkey, is removed after that long without a signed request.
- **Three new tables in the database**, `devices`, `device_enrollments`
  and `authkeys`, created on start. `memory_files` is untouched, and an
  older server ignores the new tables, so rolling back still works.

## 0.4.0 — 2026-09-23

- **Breaking: the server is its own binary, `recall-server`.** `recall serve`
  is gone from the client; typing it now says where the server went and
  exits 1. A server run from this repository's Docker setup needs nothing
  done by hand, since the image runs `recall-server` itself. A server started
  as `recall serve` some other way: install `recall-server` (each release
  publishes it for Linux amd64 and arm64, and `cargo install recall-server`
  builds it anywhere) and run it with the same environment. Nothing about
  what it serves, stores or accepts changed.
- **The client is smaller**: 3.2 MB instead of 4.5 MB on Linux x86_64. It
  no longer carries a web server or SQLite, and installs from npm, Homebrew,
  curl and crates.io get only the client.
- **The server image runs the release's own binary.** `deploy/Dockerfile`
  downloads the `recall-server` the release published and checks it against
  the release's `checksums.txt` instead of compiling, so a server build takes
  a minute rather than several and needs no memory to compile. The version
  comes from the checkout's `Cargo.toml`, or `RECALL_VERSION`.
  `RECALL_SOURCE=source` still builds from the checkout.
- **A server runs releases, not `main`.** Each release deploys itself, and
  any released version can be deployed from Actions → Deploy, which is also
  how to roll back. A push to `main` no longer deploys anything. A
  deployment whose SSH key has a forced `command=` that pulls `main` has to
  drop it; see `docs/reference/github-actions-deploy.md`.
- **`GET /health` names the commit a release binary was built from** even
  when `RECALL_GIT_COMMIT` is not set.
- **`recall connect` says less.** Every line is shorter ("Connected to
  recall.example.com", "Saved token OK", "Sync recall?", "Uploaded 5 files",
  "Connected as jarvis"), and the commit hint is just the two git commands.
  Steps are marked with round bullets: filled for the question being asked,
  hollow once answered, a cross when something failed; the token is masked
  with bullets too. Nothing about what it does or asks has changed.
- **Plainer wording in `doctor`, `status`, `init` and `backfill`.** Shorter
  sentences, no dashes. `doctor` now says where the URL came from ("saved in
  ~/.recall/config.toml") instead of just "set", and its token advice names
  `credentials.toml` rather than 0.3.0's `credentials.json`. `status`
  suggests `recall connect` for the machine scope instead of
  `RECALL_MACHINE_KEY`.
- **The server says what it is: `GET /.well-known/recall`.** An
  unauthenticated document with the protocol versions it speaks, its
  version, whether it is a release build, the oldest client it accepts, how
  clients authenticate, and what it can do. A release reports its release;
  any other build reports a pre-release of the next patch, such as
  `0.3.3-dev+ge100cfd`, and `recall version` says "dev build" too. Unknown
  keys are to be ignored and nothing is removed within a protocol version,
  so older clients keep reading newer servers. See `docs/reference/api.md`.
- **Every request says which protocol and build sent it**, as
  `Recall-Protocol: 1` and `User-Agent: recall/<version> (<os>-<arch>)`. A
  server refuses a protocol it does not speak with `400` and names the one
  it does; a request without the header is protocol 1, which is what every
  earlier client speaks, so nothing already installed is affected.
- **`recall doctor` and `recall status` show the server's version**, and
  `doctor` fails when this client is older than the server accepts or the
  two share no protocol. Against a server older than the discovery document
  they report the commit as before. `status --json` gains `server_version`,
  `server_channel`, `server_protocols`, `min_client` and `client_version`.
- **A Windows client.** `recall` ships for Windows on x64 and arm64.
  Install it with `irm https://recall.pimlabs.id/install.ps1 | iex`, or with
  `npm install -g @pimlabs/recall`, which now supports Windows. Git for
  Windows is required: Claude Code runs hook commands through Git Bash there,
  and without it falls back to PowerShell, which cannot run the hooks
  `recall init` writes. `recall doctor` checks for it and says what to do.
  The server stays Linux-only, and winget is not there yet. See the Windows
  section of `docs/reference/install.md`, including what is a documented
  guess rather than verified on a real machine.
- **npm on macOS and Linux runs the binary directly.** The installer puts
  the verified binary where npm's `recall` link points, instead of leaving a
  wrapper script in between, so a hook call through an npm install starts
  one process rather than two.

## 0.3.2 — 2026-09-23

- **`recall connect` is the whole setup, one question at a time.** The
  token, then a name for this machine (suggested from the hostname), then —
  from inside a project — wiring its hooks and sending its existing memory.
  Each step is skipped when there is nothing to do: a saved token that still
  works is not asked for again, and a wired project is not offered. A
  mistyped token is asked for again rather than ending the run. The URL is
  optional once one is saved, `--name` and `--yes` answer the questions for a
  script, and anything left in the shell profile (`RECALL_SOURCE_ENV`,
  `RECALL_MACHINE_KEY`, `RECALL_TOKEN`) is named with what to do about it.
  `recall init` now points at `recall connect` instead of at exports for a
  shell profile.
- **`recall doctor` and `recall status` are easier to read.** `doctor`
  groups its checks — Connection, This project, Scopes, Backup — marks them
  `✓ ! ✗ ○`, puts each fix on its own line, and writes `~` for your home
  directory; `status` dims its labels and colours what is wrong. Colour
  switches itself off when the output is not a terminal or `NO_COLOR` is set.
  `--json` is unchanged, and remains the output to script against.

- **One place for a machine's settings: `~/.recall/`.** `recall connect`
  now writes two TOML files instead of `credentials.json`:

  ```text
  ~/.recall/config.toml        0644  server = "…", and [machine] name = "…"
  ~/.recall/credentials.toml   0600  [servers."<url>"] token = "…"
  ```

  Two files so the one you open, edit and back up never holds a secret.
  Tokens are still kept per server, so `recall connect` to a second server
  and back again does not ask for the first one's token twice.

  **The machine has one name, used twice.** `[machine] name` both labels the
  files this machine syncs and names its machine scope, `machine:<name>`.
  Those were two variables, `RECALL_SOURCE_ENV` and `RECALL_MACHINE_KEY`,
  that had to agree and nothing checked. Both still work and still win over
  the file; `recall doctor` now warns when one disagrees with the name.
  Naming the machine turns the machine scope on, just as setting
  `RECALL_MACHINE_KEY` did.

  **Nothing to do on upgrade.** The first command that reads configuration
  moves 0.3.0's `credentials.json` into the two files and removes it; if
  that fails, the old file is still read and `recall doctor` says why. A
  `config.toml` key Recall does not know is reported rather than ignored, and
  a file written by a newer Recall is refused rather than overwritten.

  `recall status --json` adds `config_file`, `machine_source`,
  `config_problems` and `overridden`, and `url_source` can now read
  `config_file` — the URL comes from there after `recall connect`.

## 0.3.1 — 2026-09-22

- **Deleting something from a memory file now sticks.** Every push that
  differed from what the server held went through the semantic merge, and
  the merge keeps every distinct fact from both versions — so a line removed
  on purpose was a fact from the stored side and came back. The same kept a
  resolved `CONFLICT` marker in place however many times it was removed,
  which is how this was found.

  Pushes now carry `base_sha256`, the hash of the version the edit started
  from: what this machine last pulled or pushed for the file. When the
  server still holds exactly that, nothing happened in between and the push
  replaces it. Only an edit made against something else — a concurrent edit
  from another machine — is merged, which is what the merge was for.
  `recall promote` benefits most: it pushes `MEMORY.md` specifically to
  remove a link, and without a base the merge put the link back.

  Compatible in both directions. The field is optional, an older server
  ignores it, and an older client — which sends none — is merged exactly as
  before. Upgrade the server and the client to get the fix; either alone
  changes nothing. The client records bases in the baseline file it already
  keeps, and a baseline from an older version loads with none.

## 0.3.0 — 2026-09-22

- **`recall connect <url>` puts the token somewhere better than a shell
  profile.** It reads the token from the terminal without echoing it,
  checks it against the server — `/health` for reachable, `/admin/stats` for
  accepted — and only then writes `~/.recall/credentials.json`, mode `0600`
  in a `0700` directory, atomically. A wrong token or a dead server saves
  nothing. Tokens are kept per server URL, normalised so `https://x`,
  `https://x/` and `HTTPS://X` are one server; the most recent one is also
  the default URL, so after `connect` there is nothing to export at all.
  `recall disconnect [url]` removes it, and says plainly that the token
  still works on the server until it is rotated there.

  Nothing about an existing setup changes. `RECALL_URL` and `RECALL_TOKEN`
  keep working and **outrank** the file whenever they are set — right for a
  cloud environment or CI, whose variables are a secret store — and
  `connect` refuses to run in a cloud session, whose container is thrown
  away. `RECALL_HOME` moves the directory, the way `CARGO_HOME` does.

  `recall status` now says where the URL and token came from, and `recall
  doctor` warns — on a laptop only — when the token comes from the
  environment, naming the settings file if one supplied it and saying "your
  shell" rather than guessing a profile if not. It also warns about a
  credentials file other users can read. Doctor's advice for an unset
  variable on a laptop is now `recall connect <url>`.

- **The push hook no longer goes silent when it skips a memory file that
  belongs to another project.** `recall push` resolves the project from the
  current directory. Inside a git worktree that is a different directory and
  therefore a different memory directory, even though `project_key` is
  unchanged — it comes from the git remote, which a worktree shares. A memory
  file edited while standing there failed the "is this mine" check and the
  hook exited `0` without a word, after which the next `recall pull` restored
  the server's older copy over the edit.

  Found by losing three real edits to it. Skipping an unrelated source file
  silently is right and still happens; this was never an unrelated file. The
  hook now names the project the file actually belongs to and says nothing
  was pushed. It still exits `0` — a hook must not be the reason a session
  breaks.

  Nothing else changes, and the fix is only a message: edit memory from the
  main checkout and the behaviour is what it always was.

- **`GET /health` reports `last_offbox_at`, and `recall doctor` watches it.**
  The server's own snapshots already surfaced as `last_backup_at` — but they
  sit on the disk they protect. The copy that survives losing the machine runs
  from cron, and nothing watched it at all.

  That is not theoretical. A transient `403` from the bucket made the off-box
  copy fail on this project's own server; the script died, cron mailed the
  error into a mailbox nobody reads, and everything looked normal. A backup can
  stop for days that way, and you find out when you need a restore.

  `deploy/backup-offbox.sh` now writes a stamp **after** `rclone check` passes,
  so it means "a copy reached the remote and matched" rather than "the script
  got this far" — a failed verify deliberately leaves no stamp. The server
  reads it per request (the writer is another process, so a cached value would
  report a stopped backup as current) and `recall doctor` warns once it is more
  than two days old.

  Nothing to do if you have no off-box backup: with no stamp, nothing is
  reported. Existing deployments start reporting after their next successful
  run.

- **`recall doctor` checks everything sync needs and exits non-zero when any
  of it is broken.** `recall status` reports; this judges, and the exit code
  is the whole point.

  The two were never redundant, they were incomplete. `status` prints
  `(unset)` and exits `0` because it is informational. `recall pull` warns on
  stderr and exits `0` because a hook that fails your session is worse than
  one that does nothing. Both are right, and between them an environment that
  has never synced anything is indistinguishable from one with nothing new to
  sync — which is how this repository's own cloud environment ran for a day
  with the hooks wired, the binary installed, and no `RECALL_URL` set,
  syncing nothing and saying nothing.

  `FAIL` means memory is not syncing, or is syncing somewhere you did not ask
  for: no URL or token, an unreachable server, hooks unwired *in a project*, a
  miscased `global/`, a variable set to a value Recall refused, a settings file
  that is not readable JSON, a scope whose files `MEMORY.md` links none of, or
  `CLAUDE_CODE_REMOTE_MEMORY_DIR` unset *in a remote session* — where Claude
  Code's auto-memory is off entirely and nothing syncs however correct the rest
  is. `warn` never changes the exit code — files under a switched-off scope, a
  server up but falling back to last-write-wins — because a check that fails on
  taste gets `|| true` appended to it.

  Both italics above are load-bearing, and both are read from the environment
  rather than guessed. Unwired hooks outside a git repository are not a
  failure: checking your connection from your home directory is an ordinary
  thing to do. An unset `CLAUDE_CODE_REMOTE_MEMORY_DIR` on a laptop is correct
  and goes unmentioned, rather than warning forever about something that is
  fine.

  Every finding that is not `ok` names what to do about it, including the one
  value in the whole setup that cannot be reasoned out: the cloud
  environment's `CLAUDE_CODE_REMOTE_MEMORY_DIR` is `/home/user/.claude`, not
  `$HOME`. `recall doctor --json` emits the findings for a CI step.

  Nothing about `recall status` changed. It still exits `0`, and its `--json`
  shape is unchanged apart from one added field.

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

  `recall promote <file> --to machine` moves a note there, the same way
  promoting has always moved one into the global scope. `--to` defaults to
  `global`, so anything you already type keeps working.

  `recall pull` links these from `MEMORY.md` the way it links global ones —
  Claude Code opens what that file links and nothing else, so a scope whose
  files are never linked is memory that syncs and is never read.

  `recall status` gains a `machine` line, which reports the link state as well
  as the count for exactly that reason; `recall backfill` now names the
  variable that would sync a skipped file instead of always suggesting
  `RECALL_GLOBAL_KEY`.
## 0.2.0 — 2026-09-18

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
