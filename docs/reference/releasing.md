# Cutting a release

Four channels ship the same binary. Tagging publishes the GitHub Release;
npm, crates.io and the Homebrew tap are then published by the same workflow,
once the owner approves it.

Order matters, because three of the four depend on the release existing.

## Versioning

[Semver](https://semver.org), with the rule it gives below 1.0: **the minor
number is where breaking changes live, and everything else is a patch.** New
commands and new behaviour included — below 1.0 a patch may add, it may not
break. Nothing about how large or interesting a change is decides the number;
only whether something that worked stops working.

A change is **breaking** — bump the minor, `0.3.x` → `0.4.0` — if any of these
is true:

- A CLI command or flag is removed or renamed, or an existing flag now means
  something else.
- A field is removed or renamed in `--json` output (`recall status --json`,
  `recall doctor --json`), or its type or meaning changes. Adding a field is
  not breaking.
- The HTTP API stops working between an old client and a new server, or a new
  client and an old server: a request or response field removed or renamed, a
  status code changed, a previously accepted request refused. Adding an
  optional field is not breaking — neither side rejects fields it does not
  know.
- An environment variable is removed, or starts meaning something else.
- An on-disk format Recall owns — the baseline, the credentials or config
  files, the SQLite schema — changes without Recall migrating the old one
  itself.
- The hook commands in a committed `.claude/settings.json` stop working, so a
  project wired by an older `recall init` has to be re-wired.

Everything else is a **patch**: fixes, new commands, new optional fields, new
variables, migrations Recall performs itself, performance, and every change to
text output meant for people. Human-readable output is explicitly not a
contract; `--json` is.

Two consequences worth stating, because both have already been got wrong:

- **0.3.0 should have been 0.2.1.** It added `recall connect`, `recall
  doctor` and the machine scope, and broke nothing — every setup that worked
  on 0.2.0 kept working. It went to 0.3.0 because it felt large, which is
  not the rule.
- **Every pull request says which it is**, in its description, with the item
  above that makes it breaking if it is. The release number is then counted
  from the merged PRs rather than argued about when the tag is cut.

## The short version

Merge a PR that bumps the version in `Cargo.toml` and `npm/package.json`.
Then either, from anywhere including the GitHub mobile app:

> **Actions → Cut a release → Run workflow** (on `main`)

or, from a machine with a checkout:

```sh
git tag -a v0.3.1 -m "recall 0.3.1" && git push origin v0.3.1
```

Both end in the same place. *Cut a release*
(`.github/workflows/cut-release.yml`) reads the version from `main`, refuses
if CI did not pass on that commit or if the tag already exists somewhere
else, creates the tag, and starts the Release workflow on it. It dispatches
Release explicitly because a tag pushed with the workflow's own token starts
no workflow by itself, and it dispatches it *on the tag* because the
`release` environment only admits tag refs. Run it twice and the second run
either refuses or resumes; it never moves a tag.

`.github/workflows/release.yml` then does the rest, in this order:

1. **Versions agree** — the tag, `Cargo.toml` and `npm/package.json`, before
   any runner time is spent.
2. **Build** the client's targets on native runners, then **create the
   GitHub Release** with `checksums.txt`. `install.sh` (and, on Windows,
   `install.ps1`) work from this moment.
3. **Stop and wait for you.** The `npm`, `crates`, `homebrew` and `winget`
   jobs run in the `release` environment, whose required reviewer is the
   owner. GitHub notifies you; *Review deployments → Approve* releases all
   four.
4. **npm** and **crates.io** publish through *trusted publishing*: each
   registry trusts this workflow's OIDC identity and issues a credential that
   lasts for the job. No npm or crates.io token is stored anywhere — not in
   the repository, not in a cloud environment, not on a laptop.
5. **Homebrew**: the formula is rewritten from the release's own checksums
   and pushed to `pimlabs/homebrew-tap`. The rewritten file is attached to the
   run; it lands in this repository through a PR like every other change.
6. **winget**: [`vedantmgoyal9/winget-releaser`](https://github.com/vedantmgoyal9/winget-releaser)
   opens a pull request against `microsoft/winget-pkgs` with the new
   version's manifest. Needs a one-time first submission by hand before it
   does anything — see [winget](#5-winget-first-submission-only) below.

The `npm`, `crates` and `homebrew` jobs are independent — one failing does
not stop the others — and each skips a version its registry already has, so
**re-running a failed job is always safe**. `winget` is independent of the
other three the same way, but is not its own idempotency check the way they
are: re-running it after it already opened a pull request for this version
asks winget-pkgs to open another one, which is a mess to untangle rather than
a safe no-op. If it fails partway, check whether a pull request exists at
[microsoft/winget-pkgs](https://github.com/microsoft/winget-pkgs/pulls) before
re-running the job.

**Why an approval, not a check.** Nothing a registry accepts can be taken
back: npm refuses to unpublish after 72 hours, and crates.io can yank a
version but never delete it. Whoever cuts the tag — you, or an agent
working for you — can build and draft everything; only a person can make it
public. From a phone that is the whole release: *Run workflow*, then, about
ten minutes later, *Approve* on the notification.

### One-time setup

Done once per repository; nothing here needs repeating per release.

1. **GitHub environment** — *Settings → Environments → New environment*,
   named exactly `release`:
   - **Required reviewers**: yourself.
   - **Deployment branches and tags**: *Selected*, add the tag rule `v*`.
   - **Create it before the first tag.** A job naming an environment that
     does not exist makes GitHub create it — unprotected — and the publish
     would run with no approval at all.
2. **npm** — on npmjs.com, `@pimlabs/recall` → *Settings → Trusted
   publishing* → GitHub Actions: organization `pimlabs`, repository `recall`,
   workflow `release.yml`, environment `release`.
3. **crates.io** — for **each** of `recall-wire`, `recall-hooks`,
   `recall-server` and `recall`: the crate's *Settings → Trusted Publishing →
   Add*, with repository owner `pimlabs`, repository `recall`, workflow
   `release.yml`, environment `release`.
4. **Homebrew tap** — a fine-grained personal access token with *Contents:
   read and write* on `pimlabs/homebrew-tap` **only**, saved as the secret
   `HOMEBREW_TAP_TOKEN` on the `release` environment (not the repository),
   so only an approved job can read it. Without it the `homebrew` job warns
   and skips; everything else still publishes.
5. **winget** — a *classic* personal access token with the `public_repo` and
   `workflow` scopes (the second so a workflow-file change upstream in
   `microsoft/winget-pkgs` doesn't fail the job intermittently — a fine-grained
   token is not accepted here), saved as the secret `WINGET_TOKEN` on the
   `release` environment. Without it the `winget` job warns and skips;
   everything else still publishes. This alone is not enough to make the
   `winget` job do anything, though — it also needs the fork and the first
   submission below, done once, by hand.

Once the first CI release has published cleanly, the old npm and crates.io
tokens on any laptop can be revoked.

## From a laptop instead

```sh
./scripts/release.sh v0.3.0 --dry-run    # every check, nothing published
./scripts/release.sh v0.3.0
```

Still works, and is the fallback when CI cannot publish. It runs the
preflight, checks the three version fields agree, runs the full suite plus
all three real-server checkers, checks the crate names are still free, then
walks the irreversible steps — tag, `npm publish`, `cargo publish`, the tap —
**asking before each one**. Answering anything but `y` skips that step;
nothing is published by accident. It needs npm and crates.io credentials on
the machine running it.

Pushing the tag from the script also starts the workflow above. Its publish
jobs will wait for approval and then find every version already published,
so approving or rejecting them makes no difference.

**Run it again if it stops part-way.** It reads each registry before
publishing anything, so a channel that already has this version is skipped
rather than retried, and a tag already on origin is a resume rather than a
collision. A failure in one channel no longer ends the run either: npm and
crates.io have nothing to do with each other, so the one that can still work
does. Whatever did not publish is named at the end and the script exits
non-zero.

If main has moved on since the tag — the ordinary case, because the fix for
whatever broke the release gets merged — the script prints what differs from
the tag and asks before publishing from `HEAD` instead. A crate's package
contains only its own directory, so commits that touch nothing under
`crates/` change nothing that reaches the registry; the diff it prints is how
you tell.

The formula rewrite and the tap push are `scripts/update-formula.py` and
`scripts/push-tap.sh`, shared by the script and the workflow so the two
cannot write different formulas. CI checks the rewrite on every PR with
`scripts/update-formula-check.sh`.

The rest of this page is what each step does and why, for when a step fails
and you need to finish by hand.

---

## 0. Before the tag

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

cargo build --release
./scripts/compat-check.sh target/release/recall-server     # 19 checks
./scripts/api-doc-check.sh target/release/recall-server    # 90 checks
./scripts/trusted-ip-check.sh target/release/recall-server # 9 checks
```

Confirm the version is the same in all three places — they are not linked, so
a mismatch ships a broken npm package (its `postinstall` looks for a release
named after *its own* version):

```sh
grep '^version' Cargo.toml           # workspace.package.version
grep '"version"' npm/package.json
grep 'refs/tags' Formula/recall.rb
```

## 1. Tag

```sh
git tag v0.1.0
git push origin v0.1.0
```

`.github/workflows/release.yml` fires on `v*`. It builds the client for six
targets, each on a **native runner** — `rusqlite` compiles SQLite from C, so
cross-compiling would need a C toolchain per target — then writes
`checksums.txt` and creates the GitHub Release:

| Asset | Runner |
|---|---|
| `recall_darwin_amd64.tar.gz` | `macos-15-intel` |
| `recall_darwin_arm64.tar.gz` | `macos-15` |
| `recall_linux_amd64.tar.gz` | `ubuntu-latest` |
| `recall_linux_arm64.tar.gz` | `ubuntu-24.04-arm` |
| `recall_windows_amd64.zip` | `windows-latest` |
| `recall_windows_arm64.zip` | `windows-11-arm` |

Those names are a contract: `install.sh`/`install.ps1` and `npm/install.js`
all construct them from `uname` / `PROCESSOR_ARCHITECTURE` / `process.platform`.
Don't rename them without changing all three. The Windows pair is `.zip`, not
`.tar.gz` — the same `checksums.txt` covers both shapes, and everything that
verifies against it filters by exact filename, not extension.

### When a build job never starts

A retired runner label does not fail — it is simply never served. The job
sits `queued` with no runner assigned and no error, and because `publish`
waits for every leg of the matrix, no release is created at all. This
happened on the first real tag: `macos-13` had been retired, the job queued
indefinitely, and `release.sh` timed out at step 6 with the other builds
green and nothing to show for them.

The recovery does **not** involve moving the tag. `workflow_dispatch` takes
the workflow file from `main` but checks out the ref you name, so fixing the
matrix on `main` and re-dispatching builds the tagged code with the corrected
runners:

1. Cancel the stuck run.
2. Fix the label in `.github/workflows/release.yml`, land it on `main`.
3. Actions → Release → Run workflow, with `tag` set to the tag in question.

Check the label against [`actions/runner-images`](https://github.com/actions/runner-images)
before assuming anything else is wrong.

**The moment this finishes, `install.sh` works.** Verify it against the real
thing rather than assuming:

```sh
curl -fsSL https://recall.pimlabs.id/install | bash
recall version
```

The version it prints must be the one you just tagged, and the commit beside
it must be the commit that tag points at.

### Where that short URL comes from

A **Cloudflare Worker** on `recall.pimlabs.id/*`, from
[`install-worker.js`](../../install-worker.js) and
[`wrangler.toml`](../../wrangler.toml) at the root of this repository.

It is **deployed by Cloudflare's Git integration** — the repository is
connected under Workers & Pages, and a push to `main` redeploys it. Nothing
is pasted into a dashboard, and that is not convenience: the Worker proxies
`install.sh`, so the two have to move together, and a pasted copy would drift
the moment either changed. `wrangler.toml` carries the route, so even the
hostname it answers for is reviewed rather than clicked.

Connecting it is a one-time step: **Workers & Pages → Create → Connect to
Git**, pick this repository, leave the build command empty (there are no
dependencies), deploy command `npx wrangler deploy`.

| Path | Answer |
|---|---|
| `/install`, `/install.sh` | 200, `text/x-sh`, the contents of `install.sh` on `main` |
| `/sync`, `/health`, `/admin`, `/admin/stats` | 410, naming `recall-server.pimlabs.id` |
| anything else | 404, listing the installer, the API and the repository |

It **proxies rather than copies**: every request fetches `install.sh` from
`main`, so no second version of that script exists anywhere. That is not
tidiness — `install.sh` is where the downloaded binary's SHA-256 is checked
against the release's `checksums.txt`, so a stale copy would be an installer
that verifies nothing, and nobody would notice.

The 410s are the reason this is a Worker and not a Redirect Rule, which
cannot answer for paths it does not match. `recall.pimlabs.id` was the API's
address until 2026-09-15. A client still pointed at it would otherwise
receive a bare 404 from whatever sits at the origin — and Recall's hooks exit
0 on an unreachable server by design, so that machine would stop syncing in
complete silence. **410 rather than a redirect** is deliberate too: sending a
`POST /sync` onwards would hand a client's memory to a host it never
authenticated against.

`scripts/install-worker-test.js` drives all of it against a stubbed
upstream, including the case where GitHub is down — which must fail loudly
rather than pipe a truncated script into someone's shell.
`scripts/wrangler-check.py` asserts the config itself: that the route covers
the whole host, that no binding or secret has crept in, and that
`workers_dev` is a top-level key. That last one is not hypothetical — written
one line lower it parses as a *route* key instead, and the `*.workers.dev`
subdomain stays quietly enabled, publishing a second address for the
installer that nothing documents. CI runs both.

The API now lives at `recall-server.pimlabs.id`, on a **DNS-only** record.
That record must stay DNS-only: proxying it would put Cloudflare's edge
between the client and Traefik, and every client would share one rate-limit
bucket. The zone's `*.pimlabs.id` wildcard **is** proxied, so deleting the
specific record would not break the hostname — it would quietly demote it,
which is worse.

## 2. Homebrew

> Ordered last in `release.sh`, after both registries, and not for tidiness:
> rewriting `Formula/recall.rb` leaves the working tree dirty, and `cargo
> publish` refuses to package from a repository with uncommitted changes
> anywhere in it. With the formula written first — as it was through v0.2.0 —
> crates.io could not run at all. The formula still needs a pull request of
> its own afterwards; the script does not commit it, and says so when it
> finishes.

The formula installs the release archives rather than building them, so it
carries **four** checksums — one per platform — and they come from the
release's own `checksums.txt`. `scripts/release.sh` does this: it downloads
that file, rewrites `Formula/recall.rb` in place, and pairs each hash with
the archive named on the `url` line above it. Pairing matters: hashes written
positionally would hand a platform another platform's digest, and Homebrew
would report a corrupt download rather than a mistake in this repository.

Then it pushes the result to **`pimlabs/homebrew-tap`**, which is where
`brew install pimlabs/tap/recall` looks. That push is the step a release
forgets when it is done by hand — and the failure arrives later, on someone
else's machine, as a checksum error that says nothing about why.

`Formula/recall.rb` here is the source; the copy in the tap is output and
carries a generated-file header saying so. Commit the updated source in this
repository too, so the two do not drift.

By hand, if you are finishing an interrupted run:

```sh
curl -fsSL https://github.com/pimlabs/recall/releases/download/v0.1.0/checksums.txt
# put each hash on the sha256 line under its own archive's url, then:
git clone https://github.com/pimlabs/homebrew-tap /tmp/tap
cp Formula/recall.rb /tmp/tap/Formula/recall.rb
# commit and push /tmp/tap
```

`brew install --HEAD pimlabs/tap/recall` builds from `main` and needs none of
this — it is the one path that still wants a Rust toolchain.

## 3. npm

```sh
cd npm
npm publish --access public
```

Scoped packages default to private, hence `--access public`. Verify from a
clean machine — the interesting part is the `postinstall` download, which
never runs during `npm publish`:

```sh
npm install -g @pimlabs/recall && recall version
bun install -g @pimlabs/recall  && recall version
```

If `npm/package.json`'s version doesn't match a released tag, the install
fails with a clear message pointing at cargo and Homebrew — by design, since
the alternative is a half-installed package.

## 4. crates.io

Five crates, published **bottom-up**. Each one has to be on the index before
anything that depends on it can be packaged, so the order is not optional:

On a **first** publish, check the names are still free before you start —
crate names are global and first-come, and a half-published set is awkward to
back out of:

```sh
for n in recall-wire recall-hooks recall-server recall; do
  a=$(echo "$n" | cut -c1-2); b=$(echo "$n" | cut -c3-4)
  printf '%-16s %s\n' "$n" \
    "$(curl -s -o /dev/null -w '%{http_code}' "https://index.crates.io/$a/$b/$n")"
done
# 404 = available, 200 = taken
```

This is not hypothetical: the binary crate was called `recall-cli` until that
check found the name already belonged to an unrelated project — a TUI session
browser for AI coding assistants, in almost exactly this space. `recall` was
taken as well, so v0.1.0 shipped as `recall-sync`.

`recall` has since been transferred to us, and the crate took it at **0.2.0**.
That is the part worth carrying forward: a transferred name arrives with its
old versions still on the index, and a version number is never reusable. The
2019 crate's `0.1.0` sits under that name for good, so 0.1.x was never
available and the first publishable number was 0.2.0. A `200` from the check
above no longer means "pick another name" when the name is already ours — it
means look at which versions are there before choosing the next one.

Then, bottom-up:

```sh
cargo publish -p recall-wire
cargo publish -p recall-hooks
cargo publish -p recall-server
cargo publish -p recall
```

Wait for each to land before the next — the index takes a few seconds, and
`cargo publish` will fail with "no matching package named …" if you get ahead
of it. Then:

```sh
cargo install recall
```

**crates.io is permanent.** A published version can be yanked but never
deleted, and the crate names are claimed for good. That is the reason this
step is deliberate rather than automated.

## 5. winget (first submission only)

Package identifier `PimLabs.Recall`, publisher `pimlabs`, license `MIT`,
moniker `recall`. The `winget` job in `.github/workflows/release.yml`
(`vedantmgoyal9/winget-releaser`, which drives
[komac](https://github.com/russellbanks/Komac) under the hood) keeps this up
to date automatically from then on — **but it updates an existing package,
it does not create one.** At least one version has to already be in
`microsoft/winget-pkgs` before that job does anything, so the first version
after this feature was added has to go in by hand. Once it has, every later
release needs nothing further here.

This is a few one-time steps, done once total, not once per release:

1. **Classic PAT.** [github.com/settings/tokens](https://github.com/settings/tokens)
   → *Generate new token (classic)* → scopes `public_repo` and `workflow` (see
   the One-time setup step above for why both). Copy it somewhere safe; GitHub
   shows it only once. **Browser-only, works from any machine.**
2. **Fork `microsoft/winget-pkgs`** under the `pimlabs` account — the same
   account that owns this repository, which is what lets the action push to
   it without a `fork-user` input. **Browser-only, works from any machine.**
3. **Add the token as `WINGET_TOKEN`** on this repository's `release`
   environment (*Settings → Environments → release → Environment secrets*),
   not the repository's own secrets — same reasoning as
   `HOMEBREW_TAP_TOKEN` above: only an approved job can read it.
4. **The first submission itself.** This is the one step that touches a real
   manifest, and it is where a mistake is most likely — the nested-zip shape
   below is easy to get wrong by hand and easy to verify with
   `packaging/winget/generate_manifest.py` first:

   ```sh
   python3 packaging/winget/generate_manifest.py 0.4.1 /path/to/checksums.txt
   cat packaging/winget/out/0.4.1/manifests/p/PimLabs/Recall/0.4.1/*.yaml
   ```

   `checksums.txt` is the target release's own — download it from that
   release's GitHub page, or run
   `curl -fsSL https://github.com/pimlabs/recall/releases/download/v0.4.1/checksums.txt`.
   This prints what a correct manifest looks like: `InstallerType: zip`,
   `NestedInstallerType: portable`, one `Installers` entry per architecture,
   and `NestedInstallerFiles` naming `recall.exe` with
   `PortableCommandAlias: recall` — that alias is what makes `recall` the
   command `winget install PimLabs.Recall` leaves on `PATH`, and it is the
   one field an interactive wizard is most likely to skip past without
   asking. This script is not itself what submits — neither komac nor
   wingetcreate take a pre-written manifest folder as input for a *new*
   package — it exists so you have the correct shape open next to the
   wizard's prompts rather than guessing at them.

   **Recommended: [komac](https://github.com/russellbanks/Komac).** It is
   the tool the automated `winget` job itself uses, it runs on macOS and
   Linux as well as Windows, and it is the only one of the two that does —
   so the whole submission can be done from the same non-Windows machine
   `scripts/release.sh` already runs on, with nothing installed beyond komac
   itself:

   ```sh
   brew install komac                # macOS; or: cargo install --locked komac
   komac token add                   # pastes the PAT from step 1, or:
   komac token add --token=$(gh auth token)   # if you use the GitHub CLI
   komac new PimLabs.Recall
   ```

   `komac new` asks for the package version, then an installer URL at a
   time — give it both archive URLs from the release
   (`https://github.com/pimlabs/recall/releases/download/v0.4.1/recall_windows_amd64.zip`
   and the `arm64` one), and check what it infers against the generated
   manifest above before accepting: installer type `zip`, nested installer
   type `portable`, and a command alias of `recall`. Fill in the metadata
   fields (publisher, license, moniker, description — the generated
   `PimLabs.Recall.locale.en-US.yaml` has the exact values) the same way,
   and choose to submit at the end; komac opens the pull request against
   your fork from step 2.

   **Alternative, Windows-only:
   [wingetcreate](https://github.com/microsoft/winget-create).** Needs a
   Windows machine (or a Windows VM) — there is no macOS or Linux build:

   ```powershell
   winget install wingetcreate
   wingetcreate new
   ```

   Same interactive shape as `komac new`: paste in both archive URLs, check
   the same fields against the generated manifest, and submit at the end
   (`--token` or an interactive GitHub login if you'd rather not pass the
   PAT on the command line).

   Either way, the PR goes through `microsoft/winget-pkgs`' own moderation —
   automated checks first, then a human review, commonly a few days. Once it
   merges, `winget install PimLabs.Recall` works, and every release after
   this one updates it without anyone touching a manifest again. Update
   `docs/reference/install.md`'s winget row once it is confirmed working.

## 6. Afterwards

- `recall version` on a freshly installed binary should print the new version
  and the commit it was built from — and that commit should be the one the
  tag points at. The workflow reads it back from the checkout for exactly
  this reason, so a mismatch means the wrong ref was built, not a cosmetic
  slip.
- If the release changed a request or response shape, capture it as golden
  fixtures: `./scripts/capture-wire-fixtures.sh <version>`, then commit the
  new directory under `crates/recall-wire/fixtures/wire/`. Its README has
  the rules.
- Cutting over the production server is separate — see `deploy/README.md`, and
  run `./scripts/compat-check.sh` before and after.
