# Auto-deploy via GitHub Actions

`.github/workflows/ci-deploy.yml` has two jobs:

- **`ci`** — runs on every push and PR: `cargo fmt`, `clippy -D warnings`,
  the test suite, `cargo doc` with `-D warnings`, the API-reference checker
  (`scripts/api-doc-check.sh`, which asserts `docs/api.md` against a running
  server), a syntax check of the shipped shell scripts, and a `docker build`
  of `deploy/Dockerfile` (build check only, nothing is pushed anywhere). No
  secrets needed for this job.
- **`deploy`** — runs only after `ci` passes, and only for a push that's
  actually landed on `main` (never for PRs, never for other branches). SSHes
  into the VPS and runs the same three commands from `deploy/README.md`'s
  "Updating" section.

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

## Generating a dedicated deploy key

Don't reuse your personal SSH key for this — GitHub's Actions runners hold
it for the life of every run, so if it ever leaked, a shared key would mean
rotating your access everywhere, not just here. A key scoped to this one
purpose costs nothing extra and only needs revoking in one place if it
ever needs to be.

On your own machine (not the VPS):

```sh
ssh-keygen -t ed25519 -f ./recall-deploy-key -N "" -C "github-actions-recall-deploy"
```

This makes two files: `recall-deploy-key` (private) and
`recall-deploy-key.pub` (public).

**Install the public key on the VPS**, appended to the deploy user's
`authorized_keys`:

```sh
ssh-copy-id -i recall-deploy-key.pub -p <port> <user>@<vps-host>
# or, if ssh-copy-id isn't available:
cat recall-deploy-key.pub | ssh <user>@<vps-host> "cat >> ~/.ssh/authorized_keys"
```

**Put the private key into the `DEPLOY_SSH_KEY` secret** — paste the
entire contents of `recall-deploy-key` (including the
`-----BEGIN OPENSSH PRIVATE KEY-----`/`-----END...-----` lines) as the
secret value. Then delete the local copy of the private key —
`rm recall-deploy-key` — it only needs to exist in the one GitHub secret
from here on.

## Optional hardening: restrict what the key can do

If the deploy user has broader access than "run docker compose in this one
directory," consider forcing this specific key to only run the deploy
command, via a `command=` prefix on its line in `authorized_keys`:

```
command="cd ~/recall/deploy && git -C .. pull --ff-only origin main && docker compose up -d --build",restrict ssh-ed25519 AAAA... github-actions-recall-deploy
```

With that in place, this key can't be used for an interactive shell or any
other command even if it leaked — it can only ever run that one deploy
step. Not required to get auto-deploy working, worth doing once things are
confirmed working without it.

## Verifying it works

Merge a trivial change to `main` (or re-run the workflow from the Actions
tab) and watch the `deploy` job's log — it ends with the `GET /health`
response from the VPS itself. A failure at the `git pull --ff-only` step
usually means the VPS's clone has local changes or is checked out
somewhere other than `DEPLOY_PATH`; a failure at the SSH connection step
usually means the public key didn't make it into `authorized_keys`, or
`DEPLOY_PORT`/`DEPLOY_HOST` doesn't match how you normally connect.
