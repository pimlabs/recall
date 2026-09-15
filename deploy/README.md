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
- **A clone of this repository on that machine.** Neither compose file pulls
  a published image — both build from source (`build.context` is the repo
  root), so the source has to be there:

  ```sh
  git clone https://github.com/pimlabs/recall && cd recall
  ```

- **Enough memory to compile.** The build stage is Rust plus SQLite's C
  amalgamation. It is the step most likely to fail on a small instance, and
  it fails by being killed rather than by saying why:

  ```
  error: could not compile `recall-server` (signal: 9, SIGKILL: kill)
  ```

  That message means the kernel killed the compiler, not that anything is
  wrong with the build. The fix is more memory: add swap on a small instance,
  or build the image on a bigger machine and move it over with
  `docker save` / `docker load`.

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

The first build takes a few minutes — that is SQLite compiling from C. Pass
`-f` on **every** later `docker compose` command too, or Compose will read
the default file and act on the wrong stack.

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
project's own `.claude/settings.json`. See [`../docs/reference/install.md`](../docs/reference/install.md).

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
server's clone permanently dirty and `git pull --ff-only` fails from then on,
including the pull the auto-deploy workflow runs. Put the differences in an
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

# from anywhere: the new URL answers, and the commit is the one you built
curl -sf https://recall.yourdomain.com/health
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
laptops (shell profile) and every claude.ai cloud environment
([`../docs/reference/token-setup.md`](../docs/reference/token-setup.md)). A client left pointing at
the old hostname does not error loudly; `recall pull` warns on stderr and exits
0, by design, so it just quietly stops syncing.

## Updating

```sh
cd deploy
GIT_COMMIT=$(git rev-parse --short HEAD) docker compose up -d --build
```

`GIT_COMMIT` gets baked into the image and shows up in `GET /health` —
useful for confirming what's actually running matches what's on `main`,
since `main` having a fix and the running container having it are two
different things until this is run. Plain `docker compose up -d --build`
without it still works, `/health` just reports `"unknown"` for
`git_commit`.

This can also run automatically on every push to `main` instead of by
hand — see `docs/reference/github-actions-deploy.md` for wiring up
`.github/workflows/ci-deploy.yml` against this VPS.

The SQLite file lives in the named `recall-data` volume, so it survives
rebuilds/restarts. `docker compose down -v` would delete it — don't run
that unless you mean to wipe stored memory.

## Backups

The server takes its own consistent snapshots automatically (every 24h
by default, keeping the last 7) via SQLite's `VACUUM INTO` — safe to run
against a live database. They land in `deploy/backups/` on the host,
outside Docker entirely, so you can point any off-box backup (Time
Machine, an external drive, cloud storage) straight at that folder.
`GET /health`'s `last_backup_at` confirms it's actually running.

Tune with env vars in `.env` if the defaults don't fit:
`RECALL_BACKUP_INTERVAL_HOURS`, `RECALL_BACKUP_KEEP`. This matters more
than it might seem: cloud Claude Code sessions are ephemeral by design,
so for memory content that only ever existed on one of those, this
server is the only copy left once the session ends — losing the
database here isn't just downtime, it's permanent data loss for that
content.

**To restore:** stop the server, copy a `deploy/backups/recall-*.db`
file over the live one in the `recall-data` volume, restart.

```sh
docker compose stop recall-server
docker run --rm -v recall_recall-data:/data -v "$(pwd)/backups":/backups:ro \
  alpine cp /backups/recall-<timestamp>.db /data/recall.db
docker compose start recall-server
```

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

## Removing a project that was stored under the wrong key

Recall has **no admin write surface**, deliberately: `GET /admin/stats` is
read-only, sqlite-web mounts the volume read-only, and nothing in the HTTP API
can delete a project. A leaked token cannot be used to quietly destroy your
history through any route the server exposes.

The cost of that choice is that a project stored under a key you did not want
stays there. It happens: a repository with no git remote syncs under a
`local:<path>` key derived from its checkout path, so the same project on a
second machine lands under a second key. Setting `RECALL_PROJECT_KEY` fixes it
going forward, but the rows already written keep the old key.

Nothing breaks if you leave them — they are a few kilobytes of prose that
nothing reads. Clean up only if you want to.

### Look before you delete

```sh
docker compose exec recall-server sh -c \
  "sqlite3 /data/recall.db \
   'SELECT project_key, count(*), sum(deleted) FROM memory_files GROUP BY 1 ORDER BY 1;'"
```

`sqlite3` is not in the image; if that fails, do it from the host against the
volume, or read it through sqlite-web, which is exactly what it is for.

### Then delete, from a backup you just took

```sh
# 1. A backup you can actually restore from. Do not skip this.
docker compose exec recall-server sh -c \
  "cp /data/recall.db /backups/before-cleanup-$(date +%Y%m%d-%H%M%S).db"

# 2. Stop the server. SQLite tolerates concurrent writers; you should not
#    rely on that while hand-editing the only copy of your memory.
docker compose stop recall-server

# 3. Delete, naming the key exactly as the query above printed it.
docker compose run --rm -v recall_recall-data:/data alpine sh -c \
  "apk add --no-cache sqlite >/dev/null && \
   sqlite3 /data/recall.db \"DELETE FROM memory_files WHERE project_key = 'local:-Users-me-thing';\""

docker compose start recall-server
```

Check the result with the same `GROUP BY` query, and confirm the server is
healthy again with `curl -sf https://your-host/health`.

### Renaming rather than deleting

If the point is to move a project's history to a new key rather than discard
it, `UPDATE` instead — and mind the primary key, which is
`(project_key, file_path)`, so a rename onto a key that already holds the same
paths will collide:

```sql
UPDATE memory_files SET project_key = 'me/thing'
WHERE project_key = 'local:-Users-me-thing';
```

Run it inside a transaction, and check `SELECT changes();` before committing.

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
