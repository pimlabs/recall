# Setting up Recall's server

This document is the **server** half, and it comes first: the `RECALL_URL`
and `RECALL_TOKEN` that every machine needs are produced here. Once it
answers, connect your machines with
[`../docs/reference/install.md`](../docs/reference/install.md).

Docker Compose, on anything that runs Docker. A small always-on VPS is the
usual answer; a desktop or a laptop works too, with one caveat below. Nothing
in this directory is specific to a host OS or a Docker distribution.

The choice that actually matters is not the machine but the **ingress**, and
there are two — see the next section.

Either way the client side needs nothing special. Laptops and cloud sessions
just `curl` a normal HTTPS URL, which is what keeps Recall's "zero prior setup
on a fresh environment" property intact.

**The caveat:** the server is only reachable while its machine is up. On a
laptop that means memory stops syncing when the lid closes — fine for proving
it works, merely annoying in daily use. An always-on host is the answer, and
nothing here changes except where you run it.

## Which ingress

Two compose files, same server, same image, same volume:

| File | Ingress | Use it when |
|---|---|---|
| `docker-compose.yml` | Cloudflare Tunnel | The machine runs nothing else public. Brings its own ingress and needs no open ports. |
| `docker-compose.traefik.yml` | An existing Traefik | The machine already routes other services through Traefik. One ingress you understand beats a second one you have to remember. |

Whichever you pick, two properties have to hold together, and neither is
optional:

1. **The container has no published port.** Both files use `expose`, never
   `ports`. The origin must be unreachable except through the ingress.
2. **`RECALL_TRUSTED_IP_HEADER` names the header that ingress sets** —
   `cf-connecting-ip` for the tunnel, `x-real-ip` for Traefik.

They are one property, really. Rate limiting keys off that header and runs
*before* auth, so that a flood of invalid tokens is limited too. If a client
can reach the origin directly, or can supply the trusted header itself, it
can rotate the value, get a fresh bucket per request, and have unlimited
attempts at guessing the token. Exactly one header is read, so anything sent
under another name is ignored — but only the missing `ports:` keeps the
ingress in the path at all.

Running Recall directly on the host instead of in a container is a worse
trade than it looks: the semantic merge shells out to the `claude` CLI, which
is a Node package, so "just one Rust binary" is not what gets installed. The
container also already solves the CLI's login living on the data volume,
running as non-root over a pre-existing root-owned volume, and rollback.


## Prerequisites

Both ingresses need:

- **Docker Engine with Compose v2** (`docker compose`, not the old
  `docker-compose`). Any distribution of it will do.
- **A clone of this repository on that machine, at a release tag.** The
  compose files and the Dockerfile come from the clone; the server binary
  does not. The image downloads the `recall-server` that the GitHub Release
  published and checks it against the release's `checksums.txt`, so nothing
  is compiled on the server:

  ```sh
  git clone https://github.com/pimlabs/recall && cd recall
  git checkout v0.4.0      # the release to run; see the Releases page
  ```

  The version comes from that checkout's `Cargo.toml`. `RECALL_VERSION=0.4.0`
  names one explicitly instead. Servers before 0.4.0 were built from source,
  and so are those tags.

- **Linux on amd64 or arm64.** Those are the two server builds a release
  publishes. Anything else can still build from source with
  `RECALL_SOURCE=source`, which compiles Rust and SQLite's C amalgamation in
  the image: give it memory, since a small instance fails by having the
  compiler killed (`signal: 9, SIGKILL: kill`) rather than by saying why.

The Cloudflare Tunnel option additionally needs a domain on your Cloudflare
account (the free plan is enough). That is for a **stable** hostname —
Cloudflare's zero-config "Quick Tunnels" hand out a random
`*.trycloudflare.com` URL that changes on every restart, which would mean
updating `RECALL_URL` everywhere each time. Not worth it beyond a
five-minute smoke test.

The Traefik option needs a Traefik already running on that machine, and a DNS
record for your chosen hostname pointing at it. Set the DNS up first —
Let's Encrypt cannot issue a certificate until it resolves.

## 1. Set up the ingress

### If you are using Cloudflare Tunnel

1. Open the [Zero Trust dashboard](https://one.dash.cloudflare.com/) →
   **Networks → Tunnels → Create a tunnel**.
2. Choose the **Cloudflared** connector type, name it (e.g. `recall`).
3. On the install-command step, copy just the **token** value (the long
   string after `--token`) — nothing needs installing on the host itself,
   `docker compose` runs the connector in a container.
4. Still in the wizard, add a **Public Hostname**: pick a subdomain (e.g.
   `recall.yourdomain.com`), type **HTTP**, and service URL
   `recall-server:8787` — that's the other container's name and port on
   the Compose network, not `localhost`.
5. Save.

### If you are using an existing Traefik

Nothing to create — but three values in `docker-compose.traefik.yml` are
placeholders, and all three are wrong by default on most setups. The file's
own header comment says the same thing; this is how to find each one.

1. **The network name.** `traefik` in the file must be the network your
   Traefik is actually attached to:

   ```sh
   docker inspect <your-traefik-container> -f '{{json .NetworkSettings.Networks}}'
   ```

2. **The entrypoint and certificate resolver.** `websecure` and `letsencrypt`
   are conventional, and frequently something else:

   ```sh
   docker inspect <your-traefik-container> -f '{{range .Config.Cmd}}{{println .}}{{end}}'
   ```

3. **The hostname** in `traefik.http.routers.recall.rule: Host(...)`. It has
   to match what `RECALL_URL` will be on every client.

## 2. Configure secrets

```sh
cd deploy
cp .env.example .env
```

Fill in `.env`:
- `RECALL_TOKEN` — generate with `openssl rand -hex 32`. Required for both
  ingresses.
- `CLOUDFLARE_TUNNEL_TOKEN` — the token copied in step 1.3. **Cloudflare
  only**; leave it empty with Traefik.

`.env` is gitignored — never commit it.

## 3. Run it

Cloudflare Tunnel:

```sh
cd deploy
docker compose up -d --build
docker compose logs -f   # confirm both containers report healthy/connected
```

An existing Traefik:

```sh
cd deploy
docker compose -f docker-compose.traefik.yml up -d --build
docker compose -f docker-compose.traefik.yml logs -f
```

The first build takes a minute or two, most of it installing the `claude`
CLI. Pass `-f` on **every** later `docker compose` command too, or Compose
will read the default file and act on the wrong stack.

## 4. Verify from outside

```sh
curl -H "Authorization: Bearer <your RECALL_TOKEN>" \
  "https://recall.yourdomain.com/sync?project_key=smoke-test"
# expect: {"project_key":"smoke-test","files":[]}
```

`GET /health` needs no token, useful for uptime checks or a quick "is it
alive" from anywhere:

```sh
curl "https://recall.yourdomain.com/health"
# expect: {"status":"ok","started_at":"...","last_sync_at":null or ISO timestamp}
```

If that works from your own machine, it'll work from a fresh cloud
session too — it's just an HTTPS request either way.

## 5. Point real environments at it

Set on every environment that should push and pull — see [`../docs/reference/token-setup.md`](../docs/reference/token-setup.md):

| Variable | Value |
|---|---|
| `RECALL_URL` | `https://recall.yourdomain.com` |
| `RECALL_TOKEN` | the same token from `.env` |
| `CLAUDE_CODE_REMOTE_MEMORY_DIR` | (remote/cloud environments only) that environment's `~/.claude` |

Then, in each project you want synced, run `recall init` — it wires that
project's own `.claude/settings.json` — and then `recall backfill`, which
sends the memory that project already had. See
[`../docs/reference/install.md`](../docs/reference/install.md).

## Switching ingress on a server that is already running

Moving an existing deployment from one compose file to the other — Cloudflare
Tunnel to Traefik, or back. The database is not in the container, so this does
not touch it, but "does not touch it" is worth proving rather than assuming.

**The data lives in a named Docker volume.** Both compose files declare
`name: recall` and a `recall-data` volume, which Docker names
`recall_recall-data`. Same project name, same volume, so switching files keeps
the database — *provided* the old stack really was started with that project
name. Check before you do anything else:

```sh
docker volume ls | grep recall      # expect: recall_recall-data
```

If it is named something else, the old stack was started from a different
directory or with `-p`, and you must pass the matching `-p <name>` to every
command below or you will start against an empty database.

### 1. Back up, outside Docker

```sh
cd deploy
docker compose exec recall-server sh -c \
  "cp /data/recall.db /backups/before-ingress-switch-$(date +%Y%m%d-%H%M%S).db"
ls -la backups/
```

The server also takes its own snapshots, but take one now anyway — it is the
difference between a mistake costing five minutes and costing everything.

### 2. Prepare the new file — in an override, not in place

Make sure `.env` has `RECALL_TOKEN`; keeping the **same token** means no
client needs re-provisioning.

Then set the three placeholders — but **not** by editing
`docker-compose.traefik.yml`. That file is tracked, so editing it leaves the
server's clone permanently dirty, and moving it to the next release's tag
fails from then on, including the checkout the deploy workflow runs. Put the
differences in an
untracked override beside it instead:

```yaml
# deploy/docker-compose.traefik.local.yml
services:
  recall-server:
    labels:
      traefik.docker.network: your-traefik-network
      traefik.http.routers.recall.rule: Host(`recall.yourdomain.com`)

networks:
  traefik:
    external: true
    name: your-traefik-network
```

`name:` under the network is what does the real work: it remaps the `traefik`
alias this file uses onto whatever your network is actually called. The
entrypoint and certificate resolver are left alone whenever `websecure` and
`letsencrypt` already match, which they often do.

Every command from here on takes both files, in order:

```sh
docker compose \
  -f docker-compose.traefik.yml \
  -f docker-compose.traefik.local.yml \
  <command>
```

### 3. Switch

```sh
docker compose \
  -f docker-compose.traefik.yml \
  -f docker-compose.traefik.local.yml \
  up -d --build --remove-orphans
```

`--remove-orphans` is what retires the old ingress container: `cloudflared` is
not in the new file, so without it the tunnel keeps running and the old URL
keeps working, which sounds harmless and is actually the confusing outcome —
two live paths to one server, and no signal when you get one of them wrong.

**Never `docker compose down -v`.** The `-v` deletes the volume, which is the
database. Plain `down` is fine.

**If the deployment is also moving directory**, copy `deploy/backups/` across
with it. That bind mount is relative to the compose file, so a new location is
a new, empty backup directory — the old snapshots stay on disk, unmounted, and
nothing says so until you go looking for one.

### 4. Verify, in this order

```sh
# the row counts should match what you saw before the switch
docker compose -f docker-compose.traefik.yml exec recall-server \
  sh -c "ls -la /data"

# from anywhere: the new URL answers, with the version you meant to run
curl -sf https://recall.yourdomain.com/.well-known/recall
```

Then a real round trip from a client, which is the only check that covers the
whole path:

```sh
recall status        # in a project that was already syncing
```

### 5. Only then, retire the old ingress for good

Delete the Cloudflare tunnel in the Zero Trust dashboard (or the Traefik
router, going the other way) once the new path has carried real traffic for a
while. Until then it costs nothing to leave the DNS record in place, and it is
the fastest rollback you have: put the old compose file back, and the old
hostname works again.

If the hostname changed, every client's `RECALL_URL` has to change with it —
laptops (`recall connect <new-url>`, or the shell profile where one exports
`RECALL_URL`) and every claude.ai cloud environment
([`../docs/reference/token-setup.md`](../docs/reference/token-setup.md)) — and
any `.claude/settings.json` or `.claude/settings.local.json` that declares it,
which outranks the shell profile you just updated. A client left pointing at
the old hostname does not error loudly; `recall pull` warns on stderr and exits
0, by design, so it just quietly stops syncing. `recall status` names the file
behind the URL it is actually using.

## Updating

A server runs releases, not `main`. To move to another one, check out its tag
and rebuild:

```sh
git fetch --tags origin
git checkout v0.4.1
cd deploy
docker compose up -d --build
curl -sf https://recall.yourdomain.com/.well-known/recall   # server.version
```

`GET /.well-known/recall` reports the version and, for a release, the
channel `release`. `GET /health` reports the commit the binary was built
from. Rolling back is the same, with an older tag.

This can run automatically instead: every release deploys itself, and a
chosen version can be deployed from the Actions tab (phone included). See
[`docs/reference/github-actions-deploy.md`](../docs/reference/github-actions-deploy.md)
for wiring `.github/workflows/deploy.yml` to this machine.

To try an unreleased branch on a server, build it from the checkout:
`RECALL_SOURCE=source GIT_COMMIT=$(git rev-parse --short HEAD) docker compose
up -d --build`. The discovery document then says `dev`, which is the point.

The SQLite file lives in the named `recall-data` volume, so it survives
rebuilds/restarts. `docker compose down -v` would delete it — don't run
that unless you mean to wipe stored memory.

## Backups

The server takes its own consistent snapshots automatically (every 24h
by default, keeping the last 7) via SQLite's `VACUUM INTO` — safe to run
against a live database. They land in `deploy/backups/` on the host,
outside Docker entirely, so any off-box backup can be pointed straight at
that folder. `GET /health`'s `last_backup_at` confirms it's actually running,
and `last_offbox_at` says the same for the copy that leaves this machine.

**These snapshots are still on the same disk as the database they protect.**
They survive a bad write, a bad merge, and a container that will not start.
They do not survive losing the machine, which for memory that only ever
existed in an ephemeral cloud session is the case that ends in permanent
loss. `backup-offbox.sh` below is the other half.

Tune with env vars in `.env` if the defaults don't fit:
`RECALL_BACKUP_INTERVAL_HOURS`, `RECALL_BACKUP_KEEP`. This matters more
than it might seem: cloud Claude Code sessions are ephemeral by design,
so for memory content that only ever existed on one of those, this
server is the only copy left once the session ends — losing the
database here isn't just downtime, it's permanent data loss for that
content.

The backup each `recall-server admin` change takes first goes to
`deploy/backups/admin/` instead, where this rotation never reaches it; see
[Renaming, removing or restoring a project](#renaming-removing-or-restoring-a-project).

**To restore:** stop the server, copy a `deploy/backups/recall-*.db`
file over the live one in the `recall-data` volume, restart. That replaces
everything; to put back one project, `recall-server admin restore` copies a
single key's rows out of a backup while the server keeps running.

```sh
docker compose stop recall-server
docker run --rm -v recall_recall-data:/data -v "$(pwd)/backups":/backups:ro \
  alpine cp /backups/recall-<timestamp>.db /data/recall.db
docker compose start recall-server
```

## Off-box: `backup-offbox.sh`

```sh
RECALL_BACKUP_REMOTE=recall-crypt: ./deploy/backup-offbox.sh --dry-run
RECALL_BACKUP_REMOTE=recall-crypt: ./deploy/backup-offbox.sh
```

It copies every `recall-*.db` in `deploy/backups/` to an [rclone][] remote
and then verifies the copy. Run it from the deployment directory on the VPS,
as a user that can read `deploy/backups/` — and pick that user once, because
the rclone config and the crontab below both belong to whoever you choose.
Note that the snapshots are written from inside the container, so they are
owned by its uid rather than by the user running the compose stack; readable
is what matters here, not owned.

[rclone]: https://rclone.org/

### Set up the remote, encrypted

The database holds memory written about you and your work, so it does not
leave this machine in the clear. `rclone config` twice: once for the bucket,
once for a `crypt` remote that wraps it.

```sh
rclone config    # 1. new remote, type s3 / b2, name it e.g. recall-bucket
rclone config    # 2. new remote, type crypt, remote = recall-bucket:recall
                 #    — set a password, and keep it somewhere you will still
                 #      have it when this machine is gone
```

Then `RECALL_BACKUP_REMOTE=recall-crypt:`. Filenames are encrypted too, so
the bucket shows neither the contents nor the timestamps of your snapshots.

**The crypt password is now part of your backup.** A copy you cannot decrypt
is not a copy. Store it where it survives the loss of this VPS and of the
laptop that configured it.

### On a schedule, via cron

```sh
crontab -e
```

```
17 */6 * * * cd /path/to/recall/deploy && RECALL_BACKUP_REMOTE=recall-crypt: /usr/bin/flock -n /tmp/recall-backup.lock ./backup-offbox.sh
```

Install it as the user that owns the deployment — both halves of this are
per-user, and splitting them is the failure that hides longest. `rclone`
reads `~/.config/rclone/rclone.conf`, so a remote configured as one user does
not exist for another; the job then fails every night with "didn't find
section in config file", into a mailbox nobody reads, and you learn about it
when you go looking for a restore.

**Run it more often than the server snapshots — not once a day.** The obvious
reading of `RECALL_BACKUP_INTERVAL_HOURS=24` is one snapshot per day, which a
daily copy would catch every time. But the backup loop takes its first
snapshot *before* its first sleep, so every restart writes one as well, and a
day spent deploying can produce six or seven of them in an evening.
`RECALL_BACKUP_KEEP` counts snapshots, not days, so those restarts push the
older ones out within hours rather than over a week.

A daily copy then loses whole generations — created and pruned between two
runs — and nothing reports it, because copying seven files that happen to be
seven *different* files looks exactly like copying the seven you expected.
Every six hours is a sensible floor. The snapshots are kilobytes and the
script only ever adds, so running it too often costs nothing, while running
it too rarely costs a gap you cannot see from either end.

`flock -n` stops a slow upload from overlapping the next run; without it two
copies of the script can work the same files at once. Cron mails you anything
the job prints, and this one is quiet when it works — the output you get is
the output worth reading.

### It copies. It never deletes.

`rclone sync` would be the obvious choice and is the wrong one. Sync mirrors,
including emptiness: a bind mount that did not come up, or a path edited by
one character, and the remote is emptied to match — in one run, reporting
success, because an accurate mirror of an empty directory is an empty remote.

So this copies only, and the remote keeps snapshots this box has already
rotated away. Expiring them, if it ever matters, belongs to a lifecycle rule
on the bucket — with the thing that holds the copies, not the thing that makes
them. `scripts/backup-offbox-check.sh` asserts this against a stub rclone,
because a run that wrongly deleted the remote would look exactly like a run
that did not.

It also refuses to treat an empty source as a clean run. Nothing to copy is
not the same as nothing to do, and the difference is how you find out the
directory moved before you need a restore rather than after.

## Enabling real merge (Phase 2)

Without this step, Recall still works exactly as before — conflicting
writes just fall back to last-write-wins. Semantic merge (the server
shelling out to `claude -p` per `ARCHITECTURE.md`) needs the CLI, already
installed in the image, to actually be logged in **inside the container**.
That's a one-time interactive step only the owner can do (it's your
Claude subscription):

```sh
docker compose exec -it -u node recall-server claude setup-token
```

Follow the prompt (open a URL, paste back what it gives you). The token
lands under `/data/claude-config` — the same persistent volume as the
database, so this survives rebuilds and restarts; you don't need to redo
it after `docker compose up -d --build`, only if you tear down the
`recall-data` volume itself.

Verify it worked:

```sh
curl "https://recall.yourdomain.com/health" | jq .merge
# expect: "claude_cli": {"available": true, "logged_in": true, "error": null}
```

If `logged_in` is `false`, merge silently degrades to last-write-wins —
sync keeps working either way, this section is the only thing gating
merge quality specifically. Tune with env vars in `.env` if needed:
`RECALL_MERGE_ENABLED` (set `false` to skip even attempting it),
`RECALL_MERGE_TIMEOUT_MS` (default 45s per merge call).

## Renaming, removing or restoring a project

A project stored under a key you did not want stays there until you move it.
It happens: a repository with no git remote syncs under a `local:<path>` key
derived from its checkout path, so the same project on a second machine lands
under a second key. Setting `RECALL_PROJECT_KEY` fixes it going forward, but
the rows already written keep the old key.

Nothing breaks if you leave them. They are a few kilobytes of prose that
nothing reads, so clean up only if you want to.

Recall has **no HTTP route that deletes or moves a project**, deliberately:
`GET /admin/stats` is read-only, sqlite-web mounts the volume read-only, and a
leaked token cannot destroy your history through anything the server exposes.
The write path is `recall-server admin`, a subcommand you run inside the
server's container. It opens no listener of any kind, so no token is involved
and the only way to reach it is a shell on this machine.

Run it as `node`, the user the server runs as. `docker exec` runs as root
unless told otherwise, and the commands refuse to change the database as
anyone but its owner: a root-owned journal left behind by a crash is a file
the server cannot open. Both compose files name the container
`recall-server`, so this works from any directory, whichever file you
deployed with:

```sh
alias recall-admin='docker exec -it -u node recall-server recall-server admin'
recall-admin help
```

### Look first

```sh
recall-admin list                          # every key: files, tombstones, last update
recall-admin list local:-Users-me-thing    # one key's files
```

### Then change it

```sh
recall-admin rename local:-Users-me-thing me/thing --dry-run   # exactly what would move
recall-admin rename local:-Users-me-thing me/thing             # type both keys to confirm
recall-admin remove local:-Users-me-thing
```

Every change does what this section used to ask you to remember:

- **Keys are named exactly.** A prefix, a pattern or a different case is
  refused, and the refusal names the keys it resembles. You confirm by typing
  each key back, a rename's new key as well as its old one, since a typo
  there strands the project where no machine asks for it; `--yes` skips the
  typing for a script and prints the keys it confirmed instead. A key holding
  a character that prints as nothing (a zero-width space, a bidi mark) is
  printed escaped, and refused as a rename's new key.
- **It shows exactly what will change**, row by row, before asking.
  `--dry-run` stops there: the database is opened read-only and no backup is
  taken. `remove` also warns how many of the key's files hold content that no
  other key has, which after the remove exists only in the backup.
- **It takes a backup first** and prints its path,
  `backups/admin/recall-<timestamp>.db`, after reading it back to check it
  holds the rows it showed you. The server's rotation never prunes that
  directory, and `backup-offbox.sh` copies it with everything else; delete
  them yourself once you are sure. With nowhere to write one
  (`RECALL_BACKUP_DIR` unset, or the write fails), nothing is changed, and a
  backup cut short is deleted rather than left looking like a good one.
- **It runs in one transaction.** It reads the rows again inside it and stops
  if anything differs from what it showed you (a push landed meanwhile), and
  it commits only if SQLite's `changes()` is exactly the number of rows it
  named. Anything else rolls back. A change stopped this way before it began
  deletes the backup it took, so retrying does not pile them up.
- **It runs with the server up, and checks afterwards.** Like the server, it
  waits up to 5 seconds for a write in flight, and if the lock never comes,
  it rolls back and says so. What a lock cannot stop is a push that was
  already being merged: the server reads the file, merges for up to
  `RECALL_MERGE_TIMEOUT_MS` holding no lock, and writes after, so a push that
  read before the change committed lands after it, putting a row back under
  the old key or a merge of the old version over a restored one. So after
  committing, the command waits out that window (the timeout plus a second:
  46 seconds with the default) and checks. If something came back, it names
  the paths and what to run, and exits with status 3: the change was made,
  but needs a look. Interrupting the wait is safe; it only skips the check.
  Stopping the server first avoids the question altogether.

**A rename only moves rows onto a key that holds none**, even if no path
overlaps. The primary key is `(project_key, file_path)`, so a rename onto an
occupied key would collide, and folding two projects' notes together has no
single right answer for the files both have. See
[Folding one key into another](#folding-one-key-into-another).

Whichever you do, a machine still configured for the old key writes to it
again on its next push. Change that machine first.

**To undo a rename, rename it back**: `recall-admin rename me/thing
local:-Users-me-thing`. `restore` cannot do it, because it puts a key's rows
back under that same key; it has no way to map one key onto another. Rename
back before any machine pushes to the new key, or those rows move back too;
and if something has written to the old key since, the rename back is
refused, since that key is occupied again.

### Folding one key into another

Say `local:-Users-me-thing` should have been `me/thing`, and `me/thing`
already holds notes. **Do not** point the old machine at `me/thing` and let
it push, then remove the old key. That loses notes: the first pull of the
machine's next session writes `me/thing`'s version of every file both keys
have over the machine's own copy before anything is pushed, and backfill
skips a file whose content differs, so the old versions never reach the
server. Removing the old key then deletes the only other copy.

Instead, keep everything and reconcile by hand:

1. On the machine that writes the old key, with no Claude Code session open
   in that project, copy its memory directory aside. `recall status` prints
   where it is.

   ```sh
   cp -a <memory directory> ~/recall-fold-copy
   ```

2. Set `RECALL_PROJECT_KEY=me/thing` on that machine.
3. Rename the old key to an archive key rather than removing it, so the
   server keeps it under a name no machine uses:

   ```sh
   recall-admin rename local:-Users-me-thing local:-Users-me-thing.archived-2026-09-23
   ```

4. Start a session there. Its pull brings `me/thing`'s files. Compare them
   with the copy from step 1, and bring across by hand whatever the copy has
   that they lack; your edits push as usual.
5. Once you are sure, `recall-admin remove` the archive key, which takes a
   backup of it first, or keep it. It costs a few kilobytes.

The rename refusal and `remove`'s warning both point here.

### Restoring one project from a backup

"To restore" under [Backups](#backups) replaces the whole database. To put
back one key, from a server snapshot or from a backup an admin command took:

```sh
recall-admin list --backup /backups/recall-<timestamp>.db             # what it holds
recall-admin list --backup /backups/recall-<timestamp>.db me/thing    # one key's files
recall-admin restore /backups/recall-<timestamp>.db me/thing --dry-run
recall-admin restore /backups/recall-<timestamp>.db me/thing
```

Paths are the container's, where `deploy/backups/` is `/backups`. A restore
copies the backup's rows for that key exactly: content, source, timestamp and
tombstone flag. It adds what the live database lacks and leaves alone what
the backup does not have; it never removes a row. A live row that differs
from the backup's is **not** overwritten without `--overwrite`: the command
lists each one with what would change (content size, timestamp, tombstone,
source) and refuses.

One kind of overwrite needs more than that. Where the backup has a file as
deleted (a tombstone) and it is live now, restoring the tombstone deletes the
file, and not only on the server: every machine removes it at its next pull.
The file may well have been written again on purpose since the backup, so
`--overwrite` skips these and lists them as `skipped`; add
`--restore-deletions` as well to delete them. A machine that edits a
restored file before it next pulls pushes its own version over the restored
one.

### Last resort: by hand, with `sqlite3`

For when the commands cannot run at all: an image older than they are, or a
server that will not start. Every step here is one they take for you, so read
this as the checklist they follow as much as a procedure.

```sh
# 1. Stop the server, so nothing writes while you edit by hand.
docker compose stop recall-server

# 2. A backup you can actually restore from. Do not skip this. With the
#    server stopped, a plain copy of the file is consistent.
docker run --rm -v recall_recall-data:/data -v "$(pwd)/backups":/backups alpine \
  cp /data/recall.db "/backups/before-cleanup-$(date +%Y%m%d-%H%M%S).db"

# 3. Open the database. sqlite3 is not in the server image.
docker run --rm -it -v recall_recall-data:/data alpine sh -c \
  "apk add --no-cache sqlite >/dev/null && sqlite3 /data/recall.db"
```

Look before you change anything, and name the key exactly as this prints it:

```sql
SELECT project_key, count(*), sum(deleted) FROM memory_files GROUP BY 1 ORDER BY 1;
```

Then change it inside a transaction, and check `changes()` before committing.
To remove:

```sql
BEGIN;
DELETE FROM memory_files WHERE project_key = 'local:-Users-me-thing';
SELECT changes();  -- must be the count above; if it is not, ROLLBACK;
COMMIT;
```

To rename, mind the primary key, `(project_key, file_path)`: a rename onto a
key that already holds the same paths collides, so check that the new key
holds nothing first.

```sql
BEGIN;
SELECT count(*) FROM memory_files WHERE project_key = 'me/thing';  -- must be 0
UPDATE memory_files SET project_key = 'me/thing'
WHERE project_key = 'local:-Users-me-thing';
SELECT changes();  -- must be the count above; if it is not, ROLLBACK;
COMMIT;
```

Then `docker compose start recall-server`, which also hands the files back to
the user it runs as, and confirm it is healthy with
`curl -sf https://your-host/health`.

## Monitoring / inspecting the database

Two read-oriented views come up with `docker compose up -d` alongside the
server, both for the owner's own use:

- **sqlite-web** (`coleifer/sqlite-web`) mounts the `recall-data` volume
  read-only and browses the live `recall.db` at
  `http://localhost:8081` — but **only on the machine running Docker**,
  since its port is bound to `127.0.0.1` on purpose, never exposed
  through the Cloudflare tunnel. From another machine, tunnel over SSH
  first: `ssh -L 8081:localhost:8081 <user>@<host>`, then open
  `http://localhost:8081` locally.
- **`GET /admin`**, built into `recall-server` itself, is reachable at
  the regular public URL (`https://recall.yourdomain.com/admin`) and
  needs the same `RECALL_TOKEN` as the hooks to load data.
