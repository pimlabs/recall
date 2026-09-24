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

- **The client keeps the server's audit checkpoints.** Every pull saves
  the `Recall-Audit-Checkpoint` it receives in a new file,
  `~/.recall/audit.json`, per server, each as the three lines of a C2SP
  tlog-checkpoint note. The hooks only save; they make no extra request
  and never fail for it.
- **`recall doctor` and `recall status` check them.** They ask the server
  for an RFC 9162 consistency proof from each saved checkpoint to its log
  now, and `recall doctor` fails a new `audit log` check when the log no
  longer extends one: history rewritten, or rolled back by a restore. The
  finding is kept in `audit.json`: `doctor` keeps failing, `status` keeps
  showing it, and every session's pull warns about it (still exiting 0),
  until `recall audit reset`. `recall status --json` gains an `audit`
  object.
- **Checkpoints waiting to be checked are kept, all of them**, up to
  4096; past that the ones dropped are counted and `doctor` fails until a
  reset. A check proves the newest checkpoint already proven first and
  keeps nothing until that proof holds, so a server showing one history
  to one check and another to the next cannot get both marked proven;
  after it, each is written down as proven the moment its proof verifies,
  so a check the server's rate limit or a deadline cuts short carries on
  next time. The pull says so once more than 16 wait. `doctor` also fails
  when `audit.json` cannot be read, when the server answers with anything
  but a proof, and, while anything is saved, once no check has finished
  in a week, the oldest waiting is a week old, or ten checks in a row
  went unanswered; a check the server did not answer this once only
  warns, and so does one it refused this machine's credential for (401,
  403), with what to do about the credential. `status` and `doctor` stop
  waiting on a slow server after 90 seconds, and give the audit check 20
  more of its own, so a slow server cannot keep it from being asked.
- **`recall audit`**, with three commands. `export` writes the server's
  whole log (admin device or `RECALL_TOKEN`) in the format
  `scripts/audit-verify.py` reads, and checks the saved checkpoints
  against it. `verify FILE` checks an export offline with the script's
  checks, in Rust, against every checkpoint saved here; with no file it
  asks the server for the proofs, as `doctor` does. `reset` forgets what
  was saved for the server. Exit codes are the script's: 0, 1 when
  something does not check out (a server that no longer keeps a log this
  machine saw it keep included, for `export` too), 2 when it could not be
  checked (`reset` included, when `audit.json` cannot be read).

## 0.4.2 — 2026-09-24

- **Merging can move out of the server, into `recall-worker`.** A new
  binary and a second compose service with no port at all: an enrolled
  device with a new `worker` scope, which claims conflicting pushes from
  a queue, merges them with its own `claude` CLI, and posts the result
  back. The `claude` login then lives on the worker's volume, not in the
  container the internet reaches. The service is opt-in: it starts only
  with `COMPOSE_PROFILES=worker` in `deploy/.env`, and even then nothing
  changes until you approve its code with `"scope": "worker"`; see "The
  merge worker" in `deploy/README.md`. Without an approved worker the
  server merges inline exactly as before.
- **`recall-worker` is told its server by `RECALL_WORKER_SERVER`**, and
  never reads the client's `RECALL_URL`; with only that set it refuses to
  start. `RECALL_WORKER_DIR` (the image sets `/data`) is required too. It
  asks for `/.well-known/recall` first and works only for a server that
  lists `merge_queue`, records that server in its identity file and refuses
  any other, and treats a refusal no retry will change (a `404` on
  enrolment among them) as final: it says why once and idles, rather than
  exiting into a restart loop.
- **Every merge, the server's inline one included, runs `claude` with no
  tools, one turn and no saved session** (`--tools "" --max-turns 1
  --no-session-persistence`), and both images pin the CLI to 2.1.280, the
  version those flags were checked against, rather than whatever npm
  resolves on the day of the build.
- **With a worker, a conflicting push is answered at once.** It is stored
  as sent, `merged: false`, with a new `merge_job` field naming the queued
  job, and the merged file arrives with the next pull. A merge is written
  only if the file has not changed since; otherwise it is merged again
  with the newer version. An empty merge of two versions that were not
  both empty is never written: it counts as an error and is retried.
  Deleting a file closes its waiting jobs in the same transaction, so a
  file made again after a delete never has the deleted notes merged back
  in. `merge_job` is omitted when no job was queued, so a server without a
  worker answers byte for byte as before.
- **Nothing a worker leaves is stranded.** Revoking the last worker puts
  merging back in the server with the jobs it left: each is merged by the
  server's own `claude` CLI, through the same check that the file has not
  changed, or, when that CLI cannot merge, marked failed, visibly, for a
  retry later. A worker that has not asked for work in two minutes, or a
  full queue (1000 jobs), no longer means last-write-wins while the
  server's CLI is logged in: the server merges new conflicts itself.
- **New routes:** `POST /v1/jobs/claim` and `POST /v1/jobs/{id}/result` for
  the worker (a worker device only, not even `RECALL_TOKEN`), and
  `GET /v1/jobs` and `POST /v1/jobs/{id}/retry` for the owner. A worker
  device can use nothing else: it cannot pull or push memory, and an
  authkey never makes one.
- **`/health`'s `merge` gains `worker` and `queue`**: `worker` while one
  is enrolled, and `queue` then and whenever the queue holds a job,
  failed ones included. `claude_cli` is the worker's CLI while there is
  one. What `last_merge_error` says of a job names the job and never a
  project or a file, since `/health` answers anyone; `GET /v1/jobs` has
  those. A worker's error, and its CLI's, are kept to 500 bytes. Discovery
  lists a new `merge_queue` capability.
- **`recall status` and `recall doctor` show the merge queue**, and warn
  when the worker has not asked for work in two minutes (`status` then
  says `stalled` rather than `ready`, whatever the worker last reported),
  when any merge has failed, and once the oldest waiting merge is an hour
  old. `recall status --json` gains `merge_worker`, `merge_queue` and
  `merge_worker_seen_at`.
- **`POST /v1/devices/approve` accepts `"scope": "worker"`**, and its
  refusal of an unknown scope now says `scope must be sync, admin or
  worker`. `recall devices approve <code> --worker` approves one.
- **Two database changes, both made on start:** a `jobs` table, and the
  `devices` table rebuilt so its scope may be `worker`, keeping every row.
  0.4.1 ignores `jobs` and refuses a worker device, whose scope it does
  not know, so rolling back to it still works; revoke the worker first
  all the same (see "Revoking it" in `deploy/README.md`).
- **Releases publish `recall-worker`** for Linux amd64 and arm64, static,
  beside `recall-server`, in the same `checksums.txt`, and on crates.io.
- **The `/admin` page manages devices, and signs in with a passkey.** A new
  Devices tab approves a machine by its code (showing its name, agent and
  key fingerprint first, and with the `sync`, `admin` or `worker` scope),
  lists and revokes devices, and makes and revokes authkeys, so an owner
  with only a phone can do everything `RECALL_TOKEN` could. The page signs
  in with a passkey; the token still works on it as before.
- **New setting: `RECALL_PUBLIC_URL`**, such as
  `https://recall.example.com`. Passkeys are bound to that address, so it
  must be a full domain name with no trailing dot (`http://` only for
  `localhost`, with a warning that such passkeys work on that machine
  only). Unset or unusable, passkey sign-in is off and the page says why;
  nothing else changes. All three compose files pass it through from
  `deploy/.env`.
- **The first passkey takes `RECALL_TOKEN` and a one-time bootstrap
  code.** The server prints the code to its log when it starts with
  passkey sign-in on and no passkey registered; it works for an hour, and
  once. So a copy of the token that leaked cannot plant a passkey of its
  own. Once a passkey exists the token cannot register another, whatever
  it is shown; further passkeys are added from a signed-in session. Lost
  every passkey? `recall-server reset-passkeys`, run on the server as the
  database's owner, clears them and prints a new code. See step 6 of
  `deploy/README.md`.
- **Adding or removing a passkey, and signing out the other sessions, need
  a sign-in in the last five minutes**, so a copied session cookie cannot
  lock the owner out; the page asks for the passkey again first. A new
  **Sign out other sessions** button ends every session but the current
  one, and signing in again from a browser replaces the session it had.
- **New routes** under `/admin`: `GET /admin/session`,
  `POST /admin/bootstrap/register` and `…/finish`, `POST /admin/login/start`
  and `…/finish`, `POST /admin/logout`, `POST /admin/logout/others`,
  `GET /admin/passkeys`, `POST /admin/passkeys/register` and `…/finish`,
  and `POST /admin/passkeys/{id}/remove`. The device and authkey routes
  that took `RECALL_TOKEN` or an admin device also take the page's session
  (with its `X-Recall-CSRF` header on a `POST`); `/sync` and `/v1/jobs`
  never do. A request with an `Authorization` header or a signature is
  judged by that alone, whatever cookie it also carries.
- **Starting a passkey sign-in makes the server hold nothing.** A
  ceremony's state is sealed into the `ceremony_id` the page sends back,
  so strangers starting sign-ins cannot fill anything and keep the owner
  out. Every ceremony's start and finish must be
  `Content-Type: application/json`, or it is `415`, which a page on
  another site cannot send blind.
- **`GET /admin` is served with a stricter CSP**: the page's inline script
  and stylesheet are allowed by hash rather than `'unsafe-inline'`, plus
  `X-Frame-Options: DENY` and `Referrer-Policy: no-referrer`.
- **Three more tables**, `admin_credentials`, `admin_sessions` and
  `admin_bootstrap`, created on start. An older server ignores them.
- **`recall-server version` prints a second line, `features: passkeys`**,
  and a release refuses to publish a server binary that does not say it.
- **The server binary is about 5 MiB larger** (3.0 MB to 8.3 MB, static
  x86_64 musl): it now carries OpenSSL, built in, and webauthn-rs, which
  the passkey verification needs. The client does not.
  Building the server from source needs perl and make as well as a C
  compiler, and Rust 1.88.
- **The server keeps an audit log**: a Merkle tree (RFC 9162) over every
  authenticated push, pull, delete, and change to a device or authkey,
  including a device an authkey enrols, and over the merge queue's claims,
  results and retries, the server's own when it merges without a worker,
  and over the admin page's passkeys: each one added or removed, other
  sessions signed out, a bootstrap code issued, and `recall-server
  reset-passkeys`. What a passkey session does is credited to its passkey,
  never to the token. A push queued for the worker names its job. Each
  leaf records who acted, what changed, and a content hash — never the
  content itself. A change and its leaf commit in one transaction, and
  the table refuses any `UPDATE`, `DELETE`, or insert that is not the
  next leaf.
- **What it protects against today, and what it does not.** A checkpoint
  (the tree's size and root) that the owner saves somewhere the server
  cannot reach lets `scripts/audit-verify.py` show later that the log
  still extends it: removing or rewriting anything before that checkpoint,
  by the server or by anyone with its database, is caught. Nothing saves
  checkpoints automatically yet — the client does not keep them — so until
  it does, history no one saved a checkpoint of can be rewritten
  unnoticed. A device's leaves carry its signed request and the log
  carries its public key, so what a device sent can be checked from the
  log alone, even after the device is revoked or swept; what the server
  says it did with a push is still the server's word (see "Audit" in
  `docs/reference/api.md`).
- **New routes:** `GET /v1/audit/checkpoint` (the tree's size and root)
  and `GET /v1/audit/consistency` (the proof that one size extends
  another), any credential; `GET /v1/audit/entries` (the leaves, 1,000 or
  2 MiB at a time), `RECALL_TOKEN`, an admin device or the admin page's
  session only, since the leaves name every project, file, device and
  authkey. `GET /sync` answers now carry a `Recall-Audit-Checkpoint`
  header.
- **`GET /.well-known/recall` lists an `audit` capability**, `{
  "leaf_version": 1, "max_page": 1000, "max_page_bytes": 2097152 }`.
- **`scripts/audit-verify.py`** checks an exported log offline: the tree
  with nothing but Python's standard library, and every device signature,
  with the `cryptography` package when it works and a built-in Ed25519
  otherwise (slower, never skipped unless `--no-signatures` says so). Page
  through `GET /v1/audit/checkpoint` and `/entries` into a file (one
  checkpoint line, then one leaf per line) and run
  `python3 scripts/audit-verify.py that-file --checkpoint SIZE:ROOT` with
  each checkpoint you saved. A CLI command that does the paging itself is
  client work, not this release's.
- **A new table, `audit_log`**, created on start, and read back in full
  when the server starts: a leaf missing, out of place, or no longer
  matching its hash stops the server from starting until the database is
  restored from a backup. An older server ignores the table, so rolling
  back still works. Restoring a backup truncates the log, which any
  checkpoint saved since will report as a rewrite; see `deploy/README.md`.
- **Tighter limits on requests the log records.** A push's `base_sha256`
  must be 64 hex digits (what every client has always sent), `project_key`
  and `file_path` at most 4096 bytes, and a body on the admin routes at
  most 8 KiB. Revoking an authkey's devices after the key was revoked on
  its own now revokes them.
- **`recall-server admin rename`, `remove` and `restore` close the merge
  jobs still open for the rows they change**, in the same transaction: a
  rename or a remove every job under the key, a restore every job for a
  file it writes. A worker's result posted afterwards is answered as
  already recorded and writes nothing, so it can no longer be queued again
  onto a file pushed under the old key later, or over a restored version.
  The plan and `--dry-run` say how many jobs each change closes, and the
  check after the merge window names any job queued for those rows since.
- **`recall-server admin`'s changes are in the audit log.** Each rename,
  remove or restore that commits appends one leaf in the same transaction,
  credited to the `host`: `admin_rename`, `admin_remove` or
  `admin_restore`, with the keys, the row counts, the jobs it closed and
  the backup's file name, never a path or content. A running server reads
  it in before its own next leaf, so the log stays one tree. `list`, dry
  runs and refused changes append nothing. `scripts/audit-verify.py`
  accepts these leaves, checks their shape, and refuses one that closes a
  job it could not have, or a rename or remove that leaves a job of its key
  open.

## 0.4.1 — 2026-09-23

0.4.0 was tagged but never published: its release build stopped at a
packaging check before anything reached a registry or a server. Everything
listed under 0.4.0 below ships for the first time in this release, together
with what is listed here.


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
  after an upgrade. `recall disconnect` removes it too. A hook that finds
  it readable by other users makes it yours alone and says so, and a write
  to `~/.recall` narrows the directory to `0700` if it was wider.
- **A `device.key` that cannot be read stops the hooks** with a line
  saying so, rather than sending `RECALL_TOKEN` in its place; `recall
  connect` refuses until it is fixed or moved aside, and `recall doctor`
  fails it.
- **Cloud sessions enrol themselves with `RECALL_AUTHKEY`.** Set an
  authkey on the cloud environment instead of `RECALL_TOKEN`, and
  each session's first `recall pull` enrols it (approved at once,
  ephemeral) and carries on. Nothing is typed, and a failure falls back to
  `RECALL_TOKEN` or leaves memory untouched, as a pull always has.
- **A swept cloud session enrols again by itself** when `RECALL_AUTHKEY`
  is set: a hook refused as "unknown device" with an ephemeral key enrols
  once, replacing that server's key only, and retries. Hooks that start at
  once enrol one device between them. **A revoked device is never enrolled
  again by a hook**, authkey or not: the hook says so, keeps the key, and
  the session still starts, so revoking a device cuts that machine off.
  A lasting device the server does not know, and any machine without an
  authkey, is told to run `recall connect`.
- **`recall connect` checks a device key it holds** with the server
  whatever the discovery document says, and never falls back to saving
  `RECALL_TOKEN` for a server it has a device key for; a discovery
  document that fails, other than with a `404`, stops it as unreachable.
- **Redirects are not followed.** A `3xx` from the server is reported as
  "the server redirected to …; update RECALL_URL", and nothing, the body
  included, is sent where it pointed.
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
  `device_file_exposed`, `authkey_set` and `server_devices`. Every
  existing field is unchanged: a device key that cannot be used is
  reported in `device_error`, and `server_ok` still says only whether
  `GET /health` answered.

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
- **A device whose scope the server does not know is refused** (403),
  rather than treated as a `sync` device. A later version adds scopes
  that must not reach memory; if the server is ever rolled back to this
  one, such a device can do nothing until it is revoked.
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
- **`recall-server admin`: rename, remove or restore a project from the
  server's host.** `list` shows every project key with its files, tombstones
  and last update (or one key's files, or what a backup holds, with
  `--backup`); `rename <from> <to>`, `remove <key>` and `restore
  <backup-file> <key>` replace the hand-written SQL in `deploy/README.md`.
  Each change names its keys exactly, is confirmed by typing each key back,
  a rename's target included (or `--yes`), takes a backup first into
  `backups/admin/` (never rotated) and checks it holds the rows shown, and
  runs in one transaction that commits only if exactly the rows it showed
  changed; `--dry-run` shows the change and makes none. A rename refuses a
  target key that holds any rows, and points to the safe way to fold one key
  into another; `remove` warns how many files hold content no other key has.
  A restore never overwrites a differing row without `--overwrite`, and never
  turns a live file into a tombstone without `--restore-deletions` as well.
  It runs with the server up: after committing, it waits out the server's
  merge window and checks that no push already in flight partly undid the
  change, and says what to run if one did (exit status 3). In Docker:
  `docker exec -it -u node recall-server recall-server admin list`. There is
  still no HTTP route that can delete or move a project: the commands open no
  listener and need no token. A backup that fails part way, the server's
  periodic ones included, is now deleted instead of left as a partial file
  among the good ones.
  Nothing else about the server changed, and `recall-server` with no
  arguments still serves.
- **`recall-server` can terminate TLS itself now**, for a machine with no
  ingress in front of it: `RECALL_TLS_CERT`/`RECALL_TLS_KEY` for a
  certificate already on disk, or `RECALL_TLS_ACME_DOMAINS`/
  `RECALL_TLS_ACME_EMAIL` for one it gets and renews on its own from Let's
  Encrypt. Off by default; the two existing ingress-based deployments are
  unaffected. See `deploy/README.md` and `deploy/docker-compose.direct.yml`.
  With TLS on, the server hardens its own connections the way an ingress
  otherwise would: a cap on open connections (`RECALL_TLS_MAX_CONNECTIONS`,
  default 512), and deadlines for the TLS handshake, request headers and
  idle connections. `RECALL_TLS_REQUIRED=true`, which the direct compose
  file sets, refuses to start without TLS rather than falling back to plain
  HTTP. A certificate from files is reloaded on `SIGHUP` and every 12
  hours.

## 0.4.0 — 2026-09-23 (tagged, never published; shipped in 0.4.1)

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
- **winget publishing, set up but not live yet.** Every release can now
  keep a `PimLabs.Recall` package in `microsoft/winget-pkgs` current, once
  that package exists. The first version needs a one-time manual
  submission, so `winget install PimLabs.Recall` does not work yet. See the
  winget section of `docs/reference/releasing.md` for that submission;
  `docs/reference/install.md` lists the channel once it is accepted.
- **`deploy/backup-offbox.sh init` sets up the off-box backup.** It asks for
  a provider (S3-compatible, Cloudflare R2, Backblaze B2 through S3, or an
  existing remote you already configured) and its credentials, creates the
  raw and `crypt` remotes non-interactively, generates and shows the crypt
  password(s) once, runs an encrypted write-read-delete round-trip test, runs
  the first real copy, and installs the cron line for the user actually
  running it. It refuses, with an explanation, if `rclone config` and
  `crontab` would end up belonging to different users, such as under `sudo`.
  Every answer can also come from a flag or an environment variable, and
  without a terminal it never prompts: anything missing stops it before any
  remote is created, and names what to pass. Re-running it is safe. See
  "Off-box" in `deploy/README.md`.

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
