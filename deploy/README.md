# Deploying Recall's server via OrbStack + Cloudflare Tunnel

For running the server on a Mac with [OrbStack](https://orbstack.dev)
instead of renting a VPS. OrbStack ships Docker + Compose, so this is
plain `docker compose`. Cloudflare Tunnel gives the server a stable public
HTTPS URL without opening any port on your router, and without requiring
anything special installed on the client side (laptops, cloud sessions) —
they just `curl` a normal URL, keeping Recall's "zero prior setup on a
fresh environment" property intact.

Trade-off to know going in: the server only runs while your Mac + OrbStack
+ the tunnel are up. Fine for proving Phase 1 works and for regular use if
your Mac is usually on; if you want it reachable while your Mac is asleep
or off, that's what a small always-on VPS is for later — nothing here
would need to change except where it's deployed.

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

- OrbStack installed and running.
- A domain added to your Cloudflare account (free plan is enough). You
  need this for a **stable** hostname — Cloudflare's zero-config "Quick
  Tunnels" give you a random `*.trycloudflare.com` URL that changes every
  time you start it, which means updating `RECALL_URL` everywhere each
  restart. Not worth it beyond a five-minute smoke test.

## 1. Create the tunnel in Cloudflare

1. Open the [Zero Trust dashboard](https://one.dash.cloudflare.com/) →
   **Networks → Tunnels → Create a tunnel**.
2. Choose the **Cloudflared** connector type, name it (e.g. `recall`).
3. On the install-command step, copy just the **token** value (the long
   string after `--token`) — you don't need to run anything on this Mac
   directly, `docker compose` will run the connector in a container.
4. Still in the wizard, add a **Public Hostname**: pick a subdomain (e.g.
   `recall.yourdomain.com`), type **HTTP**, and service URL
   `recall-server:8787` — that's the other container's name and port on
   the Compose network, not `localhost`.
5. Save.

## 2. Configure secrets

```sh
cd deploy
cp .env.example .env
```

Fill in `.env`:
- `RECALL_TOKEN` — generate with `openssl rand -hex 32`.
- `CLOUDFLARE_TUNNEL_TOKEN` — the token copied in step 1.3.

`.env` is gitignored — never commit it.

## 3. Run it

```sh
cd deploy
docker compose up -d
docker compose logs -f   # confirm both containers report healthy/connected
```

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

Set on every environment that should push and pull — see [`../docs/token-setup.md`](../docs/token-setup.md):

| Variable | Value |
|---|---|
| `RECALL_URL` | `https://recall.yourdomain.com` |
| `RECALL_TOKEN` | the same token from `.env` |
| `CLAUDE_CODE_REMOTE_MEMORY_DIR` | (remote/cloud environments only) that environment's `~/.claude` |

Then, in each project you want synced, run `recall init` — it wires that
project's own `.claude/settings.json`. See [`../docs/install.md`](../docs/install.md).

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
hand — see `docs/github-actions-deploy.md` for wiring up
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
