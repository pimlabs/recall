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

Three compose files, same server, same image, same volume:

| File | Ingress | Use it when |
|---|---|---|
| `docker-compose.yml` | Cloudflare Tunnel | The machine runs nothing else public. Brings its own ingress and needs no open ports. |
| `docker-compose.traefik.yml` | An existing Traefik | The machine already routes other services through Traefik. One ingress you understand beats a second one you have to remember. |
| `docker-compose.direct.yml` | None — the server terminates TLS itself | The machine has no ingress at all and you would rather not add one just for this. |

The first two put something else in front of the container and have two
properties that have to hold together, neither optional:

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

**`docker-compose.direct.yml` is a different model, not a variant of the
above.** There is no ingress, so the container's port *is* published, and
`RECALL_TRUSTED_IP_HEADER` is deliberately left unset: with nothing in front
of the server, the only address that isn't the client's own choosing is the
one the TCP connection itself arrived from, so the server reads that
instead. Setting the variable anyway is refused at startup rather than
silently ignored — see `crates/recall-server/src/config.rs` and
`scripts/tls-trusted-ip-check.sh`, which proves the same property this
section proves for the other two, on a real TLS socket.

Running Recall directly on the host instead of in a container is a worse
trade than it looks: the semantic merge shells out to the `claude` CLI, which
is a Node package, so "just one Rust binary" is not what gets installed. The
container also already solves the CLI's login living on the data volume,
running as non-root over a pre-existing root-owned volume, and rollback.


## Prerequisites

All three need:

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

The direct-TLS option needs port 443 free on that machine (nothing else
already bound to it) and, for its ACME sub-mode, the same DNS-first rule as
Traefik above — `recall-server` cannot get a certificate for a hostname that
does not resolve to it yet. Its `RECALL_TLS_CERT`/`RECALL_TLS_KEY` sub-mode
needs no DNS at all, since it never talks to a certificate authority itself.

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

### If you are using direct TLS

Nothing to create up front, but the trade-offs are worth reading before you
pick this over the other two:

- **Certificate renewal lives inside `recall-server` itself**, not in a
  separate tool you have to remember to keep running. Two ways to give it a
  certificate, both off unless configured, and configuring both — or half of
  either — is a startup error:
  - **`RECALL_TLS_CERT` + `RECALL_TLS_KEY`** point at a PEM certificate and
    key already on disk (a bind mount into the container), kept renewed by
    something else — certbot, a script, a certificate copied out of another
    Traefik. The server never writes to these files. It reads them at
    startup, again on `SIGHUP`, and every 12 hours regardless, swapping in
    the new pair only if it changed and loads cleanly (a half-written or
    mismatched pair is logged and the old certificate keeps serving). See
    [Certificates from certbot](#certificates-from-certbot) for mounting
    them.
  - **`RECALL_TLS_ACME_DOMAINS` + `RECALL_TLS_ACME_EMAIL`** ask the server to
    get and renew its own certificate from Let's Encrypt, over TLS-ALPN-01 —
    the challenge is answered on the same port the server already listens
    on, so nothing else needs port 80 or a separate ACME client. The DNS for
    every domain listed must already point at this machine before the first
    run, and the issued certificate is cached under `RECALL_TLS_ACME_DIR`
    (default `/data/acme`, inside the same named volume as the database, so
    it survives rebuilds; the server keeps that directory `0700` and its
    files `0600`, since it holds the ACME account key and the certificate's
    private key). `RECALL_TLS_ACME_STAGING=true` (or `1`/`yes`, any case)
    switches to Let's Encrypt's staging directory, for testing without
    burning the real rate limit — its certificate is not one any real client
    will trust; any value that is not a yes or a no refuses to start rather
    than silently meaning production. Wildcard domains are refused at
    startup (TLS-ALPN-01 cannot validate them). A failed order is logged
    with its attempt number and when the next retry is (one second,
    doubling up to about 18 hours); a certificate already issued keeps
    serving until it expires, so watch the log for `acme: certificate order
    failed`.
- **It fails closed.** `docker-compose.direct.yml` sets
  `RECALL_TLS_REQUIRED=true`, so if its TLS variables ever arrive missing or
  empty the server refuses to start instead of serving the bearer token over
  plain HTTP on a published port. It also sets `RECALL_TRUSTED_IP_HEADER`
  explicitly empty ("trust no header"), which is what TLS forces anyway;
  naming a header there while TLS is on refuses to start.
- **Port 443 is exposed directly to whatever can reach this machine.** The
  other two files never publish a port at all; this one has to, since there
  is no ingress to publish it for. `docker-compose.direct.yml` runs
  `recall-server` on an unprivileged internal port and maps only `443` on the
  host to it, so the process itself never needs root or a Linux capability
  to bind a privileged port.
- **The published port has to preserve each client's address.** The rate
  limiter keys on it, and with no ingress there is nothing else to key on.
  Docker's normal publish (rootful Docker, iptables DNAT) preserves it.
  Two setups do not, and in both every client lands in one shared
  rate-limit bucket, so one abusive client locks out everyone, the owner
  included:
  - **Rootless Docker.** Its port forwarding makes every connection appear
    to come from inside the container's network. Use rootful Docker for
    this file.
  - **IPv6 without IPv6 on the Docker network.** Docker then forwards IPv6
    connections through its userland proxy, and every IPv6 client appears
    as the bridge gateway. That is why `docker-compose.direct.yml` publishes
    on `0.0.0.0` only; the cost is that IPv6-only clients cannot connect.
    To serve IPv6 too, enable IPv6 on the compose network (and ip6tables
    in the daemon) and add a `[::]:443:8443` publish.

#### What an ingress did that this mode now does itself, and what it doesn't

Behind Cloudflare Tunnel or Traefik, the ingress is what faces the internet:
it holds the idle and half-open connections, times out slow clients, and
only ever hands `recall-server` complete requests. With direct TLS,
`recall-server` does that itself (`crates/recall-server/src/server/tls.rs`):

- **A connection cap**, counting connections still in their TLS handshake:
  `RECALL_TLS_MAX_CONNECTIONS`, default 512. Past it, a new connection is
  closed as soon as it is accepted, and the refusals are logged at most once
  a minute. The compose file sets `nofile` to 8192 so the cap, not the
  process's file-descriptor limit, is what a flood runs into.
- **A 10-second TLS handshake deadline**, in both modes.
- **A 15-second deadline for an HTTP/1 request's headers**, which also
  closes a keep-alive connection left idle that long (slowloris).
- **A 30-second idle deadline** for a connection with no request in flight:
  one that finishes the handshake and then sends nothing, or an HTTP/2
  connection with no stream open. HTTP/2 connections are also pinged every
  20 seconds and dropped if the ping goes unanswered for 10.
- **A ceiling on a single request**: the merge timeout plus a minute, from
  its headers to the last byte of its response, after which the idle
  deadline applies again, so a client that stops reading its response
  cannot hold the connection forever.

What it still does not do:

- **No DDoS absorption.** A flood big enough to fill the connection cap, or
  the machine's bandwidth, takes the server off the air for its duration,
  the owner's own clients included; an edge network like Cloudflare's
  absorbs that before it reaches the machine. The cap keeps the process
  alive and responsive to what it does accept, nothing more.
- **No per-address connection limit.** One client address can use every
  slot under the cap; the per-address rate limit applies to requests, not
  to connections.
- **No request filtering, bot detection or geo-blocking**, and no hiding the
  machine's address: its IP is in DNS for anyone to find.

If any of that matters for your machine, put an ingress in front and use one
of the other two files instead.

#### Certificates from certbot

`RECALL_TLS_CERT`/`RECALL_TLS_KEY` need both files readable by the
container's `node` user (uid 1000), and neither of the obvious mounts gives
that:

- `/etc/letsencrypt/live/<domain>` mounted on its own: the files there are
  symlinks into `../../archive/`, which does not exist inside the container.
- `/etc/letsencrypt` mounted whole: the links resolve, but certbot keeps
  every `privkey.pem` readable by root only, so the server cannot read it.

Copy them out instead, with a deploy hook that runs after every renewal.
Save this as `/etc/letsencrypt/renewal-hooks/deploy/recall.sh` and make it
executable:

```sh
#!/bin/sh
# Copies the renewed certificate somewhere the recall-server container can
# read it, with the key private to that container's user, then tells the
# server to reload it.
set -e
install -d -m 0700 -o 1000 -g 1000 /srv/recall-certs
install -m 0644 -o 1000 -g 1000 "$RENEWED_LINEAGE/fullchain.pem" /srv/recall-certs/fullchain.pem
install -m 0600 -o 1000 -g 1000 "$RENEWED_LINEAGE/privkey.pem" /srv/recall-certs/privkey.pem
docker kill -s HUP recall-server
```

Run it once by hand for the first copy (`sudo RENEWED_LINEAGE=/etc/letsencrypt/live/<domain>
sh /etc/letsencrypt/renewal-hooks/deploy/recall.sh`; the `docker kill` fails
harmlessly if the container is not up yet), then mount
`/srv/recall-certs:/certs:ro` and set `RECALL_TLS_CERT: /certs/fullchain.pem`
and `RECALL_TLS_KEY: /certs/privkey.pem`, as the comment in
`docker-compose.direct.yml` shows. The server logs `tls: reloaded
certificate` when the signal lands; without the signal it still picks the
new files up within 12 hours. It warns at startup if the key is readable by
anyone but its owner: keep it `0600`, owned by uid 1000.

## 2. Configure secrets

```sh
cd deploy
cp .env.example .env
```

Fill in `.env`:
- `RECALL_TOKEN` — generate with `openssl rand -hex 32`. Required for all
  three.
- `CLOUDFLARE_TUNNEL_TOKEN` — the token copied in step 1.3. **Cloudflare
  only**; leave it empty otherwise.
- `RECALL_TLS_ACME_DOMAINS`, `RECALL_TLS_ACME_EMAIL`, `RECALL_TLS_ACME_STAGING`
  — **direct TLS only**, and only for the ACME sub-mode described above; leave
  them empty otherwise. Switching to the `RECALL_TLS_CERT`/`RECALL_TLS_KEY`
  sub-mode instead needs its own volume mount, which isn't in `.env` — see the
  comments in `docker-compose.direct.yml`.

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

Direct TLS:

```sh
cd deploy
docker compose -f docker-compose.direct.yml up -d --build
docker compose -f docker-compose.direct.yml logs -f
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

**To restore:** stop the server, copy a `deploy/backups/recall-*.db`
file over the live one in the `recall-data` volume, restart.

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
as a user that can read `deploy/backups/`. Note that the snapshots are
written from inside the container, so they are owned by its uid rather than
by the user running the compose stack; readable is what matters here, not
owned.

[rclone]: https://rclone.org/

### Set it up: `backup-offbox.sh init`

```sh
./deploy/backup-offbox.sh init
```

Run as the user that should own this deployment's backups. `init` walks the
whole one-time setup in one go: it asks for a provider and its credentials,
creates the raw remote and a `crypt` remote wrapping it, generates the crypt
password(s) and shows them to you once, runs a write-read-delete test through
the encrypted remote, does the first real copy, and installs the cron line,
all for the user actually running it.

Supported providers:

| `--provider` | What it is | Also needs |
| --- | --- | --- |
| `s3` | Any S3-compatible bucket | `--bucket`, `--endpoint`, `--access-key-id`, `--secret-access-key` |
| `r2` | Cloudflare R2 | `--account-id`, `--bucket`, `--access-key-id`, `--secret-access-key` |
| `b2` | Backblaze B2, through its S3-compatible endpoint | `--region` (e.g. `us-west-002`, from the bucket's S3 endpoint), `--bucket`, `--access-key-id`, `--secret-access-key` |
| `existing` | A remote you already configured with `rclone config`, for anything not listed above | `--raw-remote` (its name) |

Every value can also come from an environment variable of the same shape
(`--access-key-id` is `RECALL_BACKUP_INIT_ACCESS_KEY_ID`, and so on), which
is what makes this scriptable:

```sh
RECALL_BACKUP_INIT_PROVIDER=r2 \
RECALL_BACKUP_INIT_ACCOUNT_ID=... \
RECALL_BACKUP_INIT_BUCKET=recall-backups \
RECALL_BACKUP_INIT_ACCESS_KEY_ID=... \
RECALL_BACKUP_INIT_SECRET_ACCESS_KEY=... \
RECALL_BACKUP_INIT_CONFIRM=yes \
./deploy/backup-offbox.sh init
```

On a real terminal, missing values are prompted for, including a final
"type yes" once the crypt password(s) are shown, since there is no other way
to confirm you actually stored them. Without a terminal, `init` never
prompts: anything missing, including that confirmation, stops it before any
remote is created, and names exactly what to pass. **The crypt password(s)
are shown once.** A copy you cannot decrypt is not a copy: store them
somewhere that survives losing this VPS and the laptop that configured it,
before you do anything else.

Running `init` again is safe. If the crypt remote it would create already
exists, it leaves the remotes and password alone and just re-runs the
round-trip test, the first copy, and the cron install, which is why the cron
line never gets duplicated by a re-run.

**`rclone config` and `crontab` must belong to the same user.** `init`
refuses outright, with an explanation, if it detects they would not, for
example under `sudo`. `rclone` reads `~/.config/rclone/rclone.conf` for
whoever's `$HOME` is in effect, while `crontab` acts on whoever the effective
user is; those can quietly stop matching under `sudo`, and the result is a
cron job that fails every night with "didn't find section in config file",
into a mailbox nobody reads. Log in as the user this should run as and run
`init` directly, without `sudo`.

### On a schedule, via cron

`init` installs this for you. By hand, or to change it:

```sh
crontab -e
```

```
17 */6 * * * cd /path/to/recall/deploy && RECALL_BACKUP_REMOTE=recall-crypt: /usr/bin/flock -n /tmp/recall-backup.lock ./backup-offbox.sh
```

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

The one exception is `init`'s own round-trip test: it writes a single object
through the crypt remote, reads it back, and deletes only that object, to
prove the remote is reachable and decryptable before anything real is copied
to it. `scripts/backup-offbox-check.sh` checks this too: that the delete it
issues names only its own test object, never anything else.

### Setting it up by hand

`init` covers S3-compatible storage, Cloudflare R2, Backblaze B2, and
reusing a remote you already configured yourself. For anything else, or to
see exactly what `init` automates, the underlying steps:

```sh
rclone config    # 1. new remote, type s3 / b2 / whatever your provider is
rclone config    # 2. new remote, type crypt, remote = recall-bucket:recall
                 #    — set a password, and keep it somewhere you will still
                 #      have it when this machine is gone
```

Then `RECALL_BACKUP_REMOTE=recall-crypt:`. Filenames are encrypted too, so
the bucket shows neither the contents nor the timestamps of your snapshots.
**The crypt password is part of your backup.** A copy you cannot decrypt is
not a copy. Store it where it survives the loss of this VPS and of the
laptop that configured it.

Install the cron line yourself with `crontab -e` (see above), as the same
user that ran `rclone config`. That pairing is per-user, and splitting it is
the failure that hides longest: a remote configured as one user does not
exist for another, so the job fails every night with "didn't find section in
config file", into a mailbox nobody reads, and you learn about it when you go
looking for a restore. `init` refuses to let this happen; doing it by hand,
nothing stops you from getting it wrong.

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

Recall has **no admin write surface for memory**, deliberately: `GET /admin/stats` is
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
  read-only (with direct TLS, just the `recall.db` file out of it, since the
  volume also holds the ACME cache's private keys) and browses the live
  `recall.db` at
  `http://localhost:8081` — but **only on the machine running Docker**,
  since its port is bound to `127.0.0.1` on purpose, never exposed
  through the Cloudflare tunnel. From another machine, tunnel over SSH
  first: `ssh -L 8081:localhost:8081 <user>@<host>`, then open
  `http://localhost:8081` locally.
- **`GET /admin`**, built into `recall-server` itself, is reachable at
  the regular public URL (`https://recall.yourdomain.com/admin`) and
  needs the same `RECALL_TOKEN` as the hooks to load data.
