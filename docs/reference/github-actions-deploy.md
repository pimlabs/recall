# Auto-deploy via GitHub Actions

A server runs **releases**, not `main`. Since 0.4.0 the image holds the
`recall-server` binary a GitHub Release published, checked against that
release's `checksums.txt` (`deploy/Dockerfile`), so what production runs is
the exact file every other channel shipped, and a version number says what
it is.

Two workflows are involved:

- **`.github/workflows/ci.yml`** runs on every push and PR: `cargo fmt`,
  `clippy -D warnings`, the test suite, `cargo doc` with `-D warnings`, the
  API-reference checker (`scripts/api-doc-check.sh`, which asserts
  `docs/reference/api.md` against a running server), a syntax check of the
  shipped shell scripts, and a `docker build` of `deploy/Dockerfile` both
  ways: from source, and from a release it assembles locally, including a
  tampered checksum that must stop the build. Nothing is pushed anywhere and
  no secrets are needed. It deploys nothing.

  The image build is the expensive part, so it runs as its own job (`image`),
  skipped on a **pull request** unless the diff touches `crates/`,
  `Cargo.toml`, `Cargo.lock` or `deploy/`. On a **push to `main`** it always
  runs, whatever changed, and `cut-release.yml` refuses to tag a commit
  whose CI run is not green in every job.
- **`.github/workflows/deploy.yml`** puts a release on the server. The
  Release workflow calls it once the GitHub Release exists, so **every
  release deploys itself**. It can also be run by hand, **Actions → Deploy
  → Run workflow** with a version such as `0.4.0`, from anywhere including
  the GitHub mobile app: that is how to roll back, or retry a deploy that
  failed. It SSHes into the server, checks out the release's tag, and
  rebuilds the stack, the same thing `deploy/README.md`'s "Updating" section
  does by hand.

  **Without the secrets it skips, and says so, rather than failing.** A
  repository with no server wired up is a normal state, and a workflow that
  fails every time is one nobody reads. The old deploy job did exactly that
  for a while (`error: missing server host`) and five PRs were merged over a
  red build before anyone looked.

Runs for a pull request supersede each other: push again and the run for the
commit you replaced is cancelled. Runs for a push to `main` never are, since
the release is cut from a commit only once its run finished green.

**Deploys never overlap.** Two deploys once rebuilt the same stack at the same
time, and the second to reach "Recreate" failed on a container-name conflict.
The deploy job has a concurrency group of its own: a second deploy waits for
the first, and one that arrives while another is waiting replaces it, so the
newest version asked for is the one production ends on.

The deploy needs secrets it doesn't have by default — set these once
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
deploy by hand and never wire up the secrets. It is what makes every deploy
state, on the run's summary page, which version production is actually
serving; see below.

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

These are stages, not a script. The public half has to reach the VPS and the
private half has to reach GitHub, both by hand, in between — so pasting the
whole section in one go runs the check before the key is installed, and the
delete before the secret is saved.

`$d` also lives only in the shell that ran `mktemp`, so stay in that terminal,
or note the path it printed: the directory outlives the variable.

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

The deploy runs a short script over SSH: fetch the release's tag, check it
out, `docker compose up -d --build`, then ask the container for its health.
The account it logs in as needs nothing beyond that: a dedicated user that
owns the clone and is in the `docker` group, with no `sudo`.

An earlier version of this page suggested a forced `command=` in
`authorized_keys` that pulled `main` and rebuilt. **Remove it if you set it
up.** A forced command replaces the script the workflow sends, so the server
would keep building whatever that line says rather than the release being
deployed, and the check at the end would then report the wrong version. The
`restrict` option on its own (no port, agent or X11 forwarding, no pty) is
still worth keeping:

```
restrict ssh-ed25519 AAAA... github-actions-recall-deploy
```

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
recommends keeping host-specific values (the real Traefik network name, your
hostname) in an untracked `docker-compose.traefik.local.yml` beside the
tracked file, precisely so moving between tags keeps working: the checkout
leaves untracked files alone, and refuses to move over local changes to
tracked ones. The deploy has to pass both or it will start a container
without them.

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

That the image contains `wget` is asserted by ci.yml's `image` job on the
same image the deploy runs, rather than assumed. An untested assumption in
this exact place is what produced the bug above.

## Every deploy says what production is running

Whether or not the deploy steps ran, the last step asks the server's
discovery document, `/.well-known/recall` beside `DEPLOY_HEALTH_URL`, which
version it is running and whether it is a release build, and writes the
answer to the run's summary. It waits up to a minute for the new container.

This exists because the alternative is silence. When `DEPLOY_HOST` is unset
the deploy steps skip and the job still succeeds, which is correct for a
fork and invisible for a repository that does have a server. On 2026-09-18
four pushes landed on `main`, every run was green, and the server stayed on a
build from the 14th.

It fails the job only when this run deployed and production reports some
other version afterwards. Where nothing was deployed (no secrets) it warns
instead: deploying by hand is a legitimate choice.

A server older than the discovery document (before 0.4.0) answers 404 there;
the summary then gives `/health`'s commit instead. Both need no
authentication, which is why this works without the deploy secrets.

## Verifying it works

Run **Actions → Deploy** with the version production should already be on,
and watch the log. A failure at the `git checkout` step usually means the
server's clone has local changes to tracked files, or is not at
`DEPLOY_PATH`; a failure at the SSH connection step usually means the public
key didn't make it into `authorized_keys`, or `DEPLOY_PORT`/`DEPLOY_HOST`
doesn't match how you normally connect. A failure at the build step with a
checksum error means the downloaded archive is not the one the release
published, and the image was rightly not built.
