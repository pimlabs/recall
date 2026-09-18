# Auto-deploy via GitHub Actions

`.github/workflows/ci-deploy.yml` has two jobs:

- **`ci`** — runs on every push and PR: `cargo fmt`, `clippy -D warnings`,
  the test suite, `cargo doc` with `-D warnings`, the API-reference checker
  (`scripts/api-doc-check.sh`, which asserts `docs/reference/api.md` against a running
  server), a syntax check of the shipped shell scripts, and a `docker build`
  of `deploy/Dockerfile` (build check only, nothing is pushed anywhere). No
  secrets needed for this job.
- **`deploy`** — runs only after `ci` passes, and only for a push that's
  actually landed on `main` (never for PRs, never for other branches). SSHes
  into the VPS, fast-forwards its clone, and rebuilds the stack — the same
  thing `deploy/README.md`'s "Updating" section does by hand.

  **Without the secrets it skips, and says so, rather than failing.** That is
  deliberate: a repository with no VPS wired up is a normal state, and a
  workflow that fails on every push to `main` is one nobody reads. This job
  did exactly that for a while — `error: missing server host` — and five PRs
  were merged over a red build before anyone looked.

The `deploy` job needs secrets it doesn't have by default — set these once
under the repo's **Settings → Secrets and variables → Actions → New
repository secret**:

| Secret | Value |
|---|---|
| `DEPLOY_HOST` | The VPS's hostname or IP |
| `DEPLOY_USER` | The SSH user to connect as |
| `DEPLOY_SSH_KEY` | That user's private key (see below — use a dedicated one) |
| `DEPLOY_PORT` | Optional, defaults to `22` |
| `DEPLOY_PATH` | Optional, defaults to `~/recall` on the VPS |

And two **variables**, not secrets — one names filenames, the other a URL
that is already public:

| Variable | Value |
|---|---|
| `DEPLOY_COMPOSE_FILES` | The `-f` flags your server is built from — see below |
| `DEPLOY_HEALTH_URL` | Your server's `/health` URL, e.g. `https://recall.example.com/health` |

`DEPLOY_HEALTH_URL` is independent of everything above: set it even if you
deploy by hand and never wire up the secrets. It is what makes every push to
`main` state, on the run's summary page, which build production is actually
serving — see below.

## Generating a dedicated deploy key

Don't reuse your personal SSH key for this — GitHub's Actions runners hold
it for the life of every run, so if it ever leaked, a shared key would mean
rotating your access everywhere, not just here. A key scoped to this one
purpose costs nothing extra and only needs revoking in one place if it
ever needs to be.

On your own machine (not the VPS), and **not inside a git checkout** — `-f
./recall-deploy-key` writes to the working directory, and this document used
to be read while standing in this repository, where nothing in `.gitignore`
would have caught it. A scratch directory costs one line and cannot be
committed by accident:

```sh
d=$(mktemp -d)
ssh-keygen -t ed25519 -f "$d/recall-deploy-key" -N "" -C "github-actions-recall-deploy"
```

That makes two files in `$d`: `recall-deploy-key` (private) and
`recall-deploy-key.pub` (public).

**Keep `$d` until the end of this section.** The order is: install the public
half, *prove the key works*, paste the private half into the secret, then
`rm -rf "$d"`. Deleting before the proof is what turns a later failure into a
guess — a deploy that cannot connect looks identical whether the key was
wrong, the account cannot take a login, or the secret was pasted short a line.

Nothing on this machine reads the private key afterwards. Its one consumer is
the Actions runner, and GitHub secrets cannot be read back out, so a copy kept
here is a credential with no reader. `~/.ssh/` would work too, with
`chmod 600` — the reason to prefer a scratch directory is not permissions but
which way the default points: in `~/.ssh/` keeping it is what happens unless
you remember to delete it, and a key with no purpose tends to outlive the one
it had, following you onto the next machine.

**Install the public key on the VPS**, appended to the deploy user's
`authorized_keys`:

```sh
ssh-copy-id -i "$d/recall-deploy-key.pub" -p <port> <deploy-user>@<vps-host>
```

That needs to log in as the deploy user, which is exactly what does not work
yet if that account has never had a key. When the deploy user is a separate
account you reach through your own, go in as yourself and write the file with
`sudo` instead:

```sh
sudo -u <deploy-user> mkdir -p /home/<deploy-user>/.ssh
sudo -u <deploy-user> chmod 700 /home/<deploy-user>/.ssh
echo 'ssh-ed25519 AAAA... github-actions-recall-deploy' \
  | sudo -u <deploy-user> tee -a /home/<deploy-user>/.ssh/authorized_keys
sudo -u <deploy-user> chmod 600 /home/<deploy-user>/.ssh/authorized_keys
```

Check the account can actually take an SSH login first — `getent passwd
<deploy-user>` ending in `/usr/sbin/nologin` or `/bin/false` means it cannot,
whatever you put in `authorized_keys`.

**Prove the key works before GitHub depends on it.** From your own machine,
with the key you have not yet pasted anywhere:

```sh
ssh -i "$d/recall-deploy-key" <deploy-user>@<vps-host> \
  'cd <DEPLOY_PATH> && git log --oneline -1'
```

A commit, with no password prompt, means the key, the account and the path are
all right. Anything else is cheaper to fix now than through a merge-and-watch
cycle, and it tells you *which* of the three is wrong, which a failed deploy
does not.

**Then put the private key into the `DEPLOY_SSH_KEY` secret** — paste the
entire contents of `$d/recall-deploy-key` (including the
`-----BEGIN OPENSSH PRIVATE KEY-----`/`-----END...-----` lines) as the
secret value. A truncated paste fails the same way a wrong key does, which is
the other reason to have run the check above: it narrows what changed.

**Then `rm -rf "$d"`.** It only needs to exist in the one GitHub secret from
here on.

## Optional hardening: restrict what the key can do

If the deploy user has broader access than "run docker compose in this one
directory," consider forcing this specific key to only run the deploy
command, via a `command=` prefix on its line in `authorized_keys`:

```
command="cd /home/recall-deploy/recall-rust/deploy && git -C .. pull --ff-only origin main && docker compose -f docker-compose.traefik.yml -f docker-compose.traefik.local.yml up -d --build",restrict ssh-ed25519 AAAA... github-actions-recall-deploy
```

**The `-f` flags are not optional here**, and this example used to omit them
— which is precisely the failure the next section describes: with no `-f`,
`docker compose` acts on `docker-compose.yml`, so on a host running the
Traefik stack it quietly builds and starts the *other* ingress alongside the
real one. Whatever you set `DEPLOY_COMPOSE_FILES` to has to appear here too,
or the hardening silently deploys something different from what the workflow
deploys.

Paths and filenames above are an example; use your own. `docker compose ls`
on the VPS prints the config files the running stack was actually built
from, which is the answer rather than a guess.

With that in place, this key can't be used for an interactive shell or any
other command even if it leaked — it can only ever run that one deploy
step. Not required to get auto-deploy working, worth doing once things are
confirmed working without it.

## `DEPLOY_COMPOSE_FILES` — a variable, not a secret

This repository ships two ingresses, so the job cannot know which compose
files your server is built from. It does not guess: if `DEPLOY_HOST` is set
and this is not, the job stops and says so.

Set it under **Settings → Secrets and variables → Actions → Variables** (it
holds no secret — it is a pair of filenames):

```
-f docker-compose.traefik.yml -f docker-compose.traefik.local.yml
```

or, for the Cloudflare Tunnel stack:

```
-f docker-compose.yml
```

Include the local override if your server has one. `deploy/README.md`
recommends keeping host-specific values — the real Traefik network name, your
hostname — in an untracked `docker-compose.traefik.local.yml` beside the
tracked file, precisely so `git pull --ff-only` keeps working; the deploy has
to pass both or it will start a container without them.

This used to be hardcoded to no `-f` at all, which acts on
`docker-compose.yml`. On a host running the Traefik stack that quietly built
and started the *other* ingress alongside the real one.

## How the health check reaches a port that is not published

Neither compose file publishes 8787 to the host — both use `expose`,
deliberately, so nothing can reach the origin around the ingress. The job
therefore asks from *inside* the container:

```sh
docker compose $COMPOSE_FILES exec -T recall-server \
  wget -qO- http://127.0.0.1:8787/health
```

The previous version ran `curl -sf http://localhost:8787/health` on the VPS,
which could never have succeeded — and it fails *after* a deploy that worked,
which reads like a broken deployment when it is not.

That the image contains `wget` is asserted by the `ci` job on the same image
the deploy runs, rather than assumed. An untested assumption in this exact
place is what produced the bug above.

## Every push says what production is running

Whether or not the deploy steps ran, the last step of the job asks
`DEPLOY_HEALTH_URL` for its `git_commit` and writes the answer to the run's
summary:

> ### Production is NOT running this push
>
> | | commit |
> |---|---|
> | production | `5f35be3` — 25 commits behind |
> | this push | `30fee03` |

This exists because the alternative is silence. When `DEPLOY_HOST` is unset
the deploy steps skip and the job still reports success — correct for a fork,
and invisible for a repository that does have a server. On 2026-09-18 four
pushes landed on `main`, every run was green, and the VPS stayed on a build
from the 14th. Nothing was broken; nothing said anything either.

It never fails the job. Deploying by hand is a legitimate choice, and a
workflow that goes red over a choice is a workflow that gets ignored — which
is the same mistake, one step along. It emits a warning annotation instead.

`/health` needs no authentication, which is why this works without the deploy
secrets. If the variable is unset, the summary says that too, rather than
leaving you to wonder whether it checked.

## Verifying it works

Merge a trivial change to `main` (or re-run the workflow from the Actions
tab) and watch the `deploy` job's log. A failure at the `git pull --ff-only`
step usually means the VPS's clone has local changes or is checked out
somewhere other than `DEPLOY_PATH`; a failure at the SSH connection step
usually means the public key didn't make it into `authorized_keys`, or
`DEPLOY_PORT`/`DEPLOY_HOST` doesn't match how you normally connect.
