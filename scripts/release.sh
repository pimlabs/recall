#!/usr/bin/env bash
#
# Cuts a release across all four install channels.
#
#   ./scripts/release.sh v0.1.0 --dry-run    # every check, no pushing
#   ./scripts/release.sh v0.1.0              # the real thing
#
# Everything up to the tag is automated and safe to re-run, and so is the
# whole script: it reads each registry before publishing, so a run that
# failed part-way is finished by running the same command again. The three steps
# that need your credentials — the tag push, `npm publish`, `cargo publish` —
# each stop and ask first, because none of them can be undone: a tag can be
# force-moved but people may already have pinned it, an npm version can be
# deprecated but not removed, and a crates.io version can be yanked but never
# deleted.
#
# The ordinary path is now the release workflow, which publishes through
# trusted publishing once the owner approves it; this script is the fallback
# for when that cannot run, and needs npm and crates.io credentials locally.
# See docs/reference/releasing.md for both, and for what each step does and
# why the order matters.
set -uo pipefail

TAG="${1:-}"
DRY_RUN=false
[ "${2:-}" = "--dry-run" ] && DRY_RUN=true

REPO="pimlabs/recall"
# The shared Homebrew tap. Being named homebrew-* is what lets Homebrew
# resolve `pimlabs/tap/recall` with no URL and no separate `brew tap` step.
TAP="pimlabs/homebrew-tap"
# Bottom-up: recall-worker before recall-server, which uses its merge code.
CRATES=(recall-wire recall-hooks recall-worker recall-server recall)
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bold=$(tput bold 2>/dev/null || true)
red=$(tput setaf 1 2>/dev/null || true)
green=$(tput setaf 2 2>/dev/null || true)
yellow=$(tput setaf 3 2>/dev/null || true)
off=$(tput sgr0 2>/dev/null || true)

step() { printf '\n%s==> %s%s\n' "$bold" "$1" "$off"; }
ok()   { printf '    %sok%s   %s\n' "$green" "$off" "$1"; }
warn() { printf '    %swarn%s %s\n' "$yellow" "$off" "$1"; }
die()  { printf '\n%serror%s %s\n' "$red" "$off" "$1" >&2; exit 1; }

# A skipped confirmation is a no, not a yes.
confirm() {
  if $DRY_RUN; then
    warn "dry run: would $1"
    return 1
  fi
  printf '\n    %s%s?%s [y/N] ' "$bold" "$1" "$off"
  read -r reply </dev/tty || return 1
  [ "$reply" = "y" ] || [ "$reply" = "Y" ]
}

case "$TAG" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  "") die "usage: $0 vX.Y.Z [--dry-run]" ;;
  *)  die "tag must look like v1.2.3, got '$TAG'" ;;
esac
VERSION="${TAG#v}"

$DRY_RUN && printf '%s(dry run — nothing will be pushed or published)%s\n' "$yellow" "$off"

# --------------------------------------------------------------------------
step "1/9  Working tree"
# --------------------------------------------------------------------------
# Formula/recall.rb is allowed to be dirty and nothing else is: the last
# step of this script rewrites it, and it lands via a pull request afterwards
# like every other change, so a resumed run finds it modified. Treating that
# as "uncommitted changes" would make the script unable to finish what it
# started.
dirty="$(git status --porcelain)"
# Anchored on the whole porcelain line — two status characters, a space,
# then exactly that path. Matching the path unanchored would also excuse a
# vendor/Formula/recall.rb, which is a different file with the same ending.
others="$(printf '%s\n' "$dirty" | grep -v '^.. Formula/recall\.rb$' | grep -v '^$' || true)"
if [ -n "$others" ]; then
  printf '%s\n' "$others"
  die "uncommitted changes — commit or stash first"
fi
if [ -n "$dirty" ]; then
  warn "Formula/recall.rb is modified — this script's own output from an earlier run"
else
  ok "clean"
fi

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || die "on '$branch'; releases are cut from main"
ok "on main"

git fetch origin main --quiet || die "could not reach origin"
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
  || die "main is not in sync with origin/main — pull or push first"
ok "in sync with origin/main"

# An existing tag used to end the run, which meant a release that failed
# anywhere after step 5 could never be finished — the tag it had just pushed
# locked the door behind it. A tag pointing at this exact commit is not a
# collision, it is the previous attempt, so the run resumes and every step
# below skips what is already done.
RESUMING=false
remote_tag="$(git ls-remote --tags origin "refs/tags/$TAG^{}" | cut -f1)"
[ -n "$remote_tag" ] || remote_tag="$(git ls-remote --tags origin "refs/tags/$TAG" | cut -f1)"
if [ -n "$remote_tag" ]; then
  if [ "$remote_tag" = "$(git rev-parse HEAD)" ]; then
    RESUMING=true
    warn "$TAG is already on origin, pointing here — resuming an earlier run"
  elif git merge-base --is-ancestor "$remote_tag" HEAD 2>/dev/null; then
    # The ordinary way a resume arrives: the release failed part-way, the fix
    # for whatever broke it was merged, and main has moved on. What gets
    # published would then be this tree and not the tagged one, so the
    # difference is printed rather than described — a crate's package only
    # contains its own directory, so commits touching nothing under crates/
    # change nothing that reaches the registry, and that is visible here.
    RESUMING=true
    warn "$TAG is on origin at ${remote_tag:0:8}, and main has moved on since:"
    git --no-pager log --oneline "$remote_tag..HEAD" | sed 's/^/      /'
    echo "    what differs from the tag:"
    git --no-pager diff --stat "$remote_tag..HEAD" | sed 's/^/      /'
    # Asked, not assumed — but never in a dry run, where `confirm` always
    # answers no and would make the one mode meant for looking around the
    # only one that cannot reach the steps below.
    if $DRY_RUN; then
      warn "dry run: would ask whether to publish from HEAD rather than from $TAG"
    elif ! confirm "publish from HEAD rather than from $TAG"; then
      die "stopped. Either cut a new version, or check out $TAG and run from there."
    fi
  else
    die "$TAG is on origin at ${remote_tag:0:8}, which is not an ancestor of HEAD. Releases are immutable; bump the version."
  fi
elif git rev-parse "$TAG" >/dev/null 2>&1; then
  die "$TAG exists locally but not on origin. Delete it or push it; do not move it."
else
  ok "$TAG is unused"
fi

# Channels that were asked to publish and did not. Kept as a string rather
# than an array: under `set -u`, bash 3.2 — which macOS still ships — errors
# on ${arr[@]} when the array is empty.
failed=""

# --------------------------------------------------------------------------
step "2/9  Versions agree"
# --------------------------------------------------------------------------
# These three are not linked to each other. npm's postinstall looks for a
# release named after its *own* version, so a drift here ships a package that
# installs nothing.
cargo_v=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
npm_v=$(grep -m1 '"version"' npm/package.json | cut -d'"' -f4)
# Read from the same line step 6 rewrites, deliberately. This used to parse a
# `refs/tags/vX.Y.Z` URL, which the formula stopped containing when it moved
# to prebuilt archives — so it matched nothing, `formula_v` was empty, and the
# warning below printed "still points at v" and could never be right. A check
# and the write it guards have to agree on where the value lives.
formula_v=$(grep -m1 '^  version "' Formula/recall.rb | cut -d'"' -f2)

printf '    Cargo.toml %s | npm %s | Formula %s | tag %s\n' \
  "$cargo_v" "$npm_v" "$formula_v" "$VERSION"
[ "$cargo_v" = "$VERSION" ] || die "Cargo.toml says $cargo_v, you asked for $VERSION"
[ "$npm_v" = "$VERSION" ]   || die "npm/package.json says $npm_v, you asked for $VERSION"
# An empty read is not a stale formula, it is a broken parser, and the two
# have to be told apart out loud. Reading nothing is exactly how the previous
# pattern rotted unnoticed: it printed "still points at v" — a version with no
# number in it — for every release after the formula changed shape.
[ -n "$formula_v" ] \
  || die "could not read a version from Formula/recall.rb — the grep above has stopped matching, so this check is not checking anything"
[ "$formula_v" = "$VERSION" ] || warn "Formula still points at v$formula_v — step 6 fixes this"
ok "versions line up"

# --------------------------------------------------------------------------
step "3/9  The suite"
# --------------------------------------------------------------------------
run() {
  local label="$1"; shift
  printf '    %-46s' "$label"
  if "$@" >/tmp/release-step.log 2>&1; then
    printf '%sok%s\n' "$green" "$off"
  else
    printf '%sFAILED%s\n' "$red" "$off"
    tail -25 /tmp/release-step.log
    die "$label failed"
  fi
}

run "cargo fmt --all --check"          cargo fmt --all -- --check
run "cargo clippy -D warnings"         cargo clippy --workspace --all-targets --locked -- -D warnings
run "cargo test --workspace"           cargo test --workspace --locked
(export RUSTDOCFLAGS="-D warnings"; run "cargo doc" cargo doc --workspace --no-deps --locked) || exit 1
run "cargo build --release"            cargo build --release --locked

# The two that talk to a real server rather than a stand-in. Both have caught
# bugs every unit test in the repo missed.
run "compat-check.sh (19 checks)"      ./scripts/compat-check.sh target/release/recall-server
run "api-doc-check.sh (115 checks)"    ./scripts/api-doc-check.sh target/release/recall-server
run "trusted-ip-check.sh (9 checks)"   ./scripts/trusted-ip-check.sh target/release/recall-server

built=$(./target/release/recall version)
printf '    built: %s\n' "$built"
case "$built" in
  "recall $VERSION"*) ok "binary reports $VERSION" ;;
  *) die "binary reports '$built', expected recall $VERSION" ;;
esac
built=$(./target/release/recall-server version)
printf '    built: %s\n' "$built"
case "$built" in
  "recall-server $VERSION"*) ok "server binary reports $VERSION" ;;
  *) die "server binary reports '$built', expected recall-server $VERSION" ;;
esac
if printf '%s\n' "$built" | grep -Eq '^features:( [a-z-]+)* passkeys( |$)'; then
  ok "server binary has passkey sign-in"
else
  die "server binary was built without the passkeys feature"
fi
built=$(./target/release/recall-worker version)
printf '    built: %s\n' "$built"
case "$built" in
  "recall-worker $VERSION"*) ok "worker binary reports $VERSION" ;;
  *) die "worker binary reports '$built', expected recall-worker $VERSION" ;;
esac

# --------------------------------------------------------------------------
step "4/9  crates.io names"
# --------------------------------------------------------------------------
# Only meaningful on a first publish, but cheap, and the reason recall-cli
# had to become recall-sync for v0.1.0.
#
# `recall` now reports 200 for a third reason, which is neither of the two the
# warning below names: the name was transferred to us, and the unrelated 2019
# crate's versions are still on the index under it. That is expected. It is
# also why 0.1.x is unavailable to us forever — a version number is never
# reusable — and why this project went from 0.1.0 straight to 0.2.0.
taken=0
for n in "${CRATES[@]}"; do
  a=${n:0:2}; b=${n:2:2}
  code=$(curl -s -o /dev/null -w '%{http_code}' "https://index.crates.io/$a/$b/$n" || echo "000")
  case "$code" in
    404) printf '    %-16s available\n' "$n" ;;
    200) printf '    %-16s %staken%s\n' "$n" "$yellow" "$off"; taken=$((taken+1)) ;;
    *)   printf '    %-16s could not check (HTTP %s)\n' "$n" "$code" ;;
  esac
done
[ "$taken" -eq 0 ] && ok "all ${#CRATES[@]} names are free" \
  || warn "$taken already on crates.io — fine if that is you republishing, fatal if not"

# --------------------------------------------------------------------------
step "5/9  Tag and push"
# --------------------------------------------------------------------------
if $RESUMING; then
  ok "$TAG is already pushed — skipping"
elif confirm "create and push $TAG (this publishes a GitHub Release)"; then
  git tag -a "$TAG" -m "recall $VERSION" || die "could not create the tag"
  git push origin "$TAG" || { git tag -d "$TAG"; die "could not push the tag (local tag removed)"; }
  ok "pushed $TAG — the release workflow is now building the client and the server"
  echo "    https://github.com/$REPO/actions"
else
  warn "skipped — nothing after this point can run"
  exit 0
fi

# --------------------------------------------------------------------------
step "6/9  Wait for the release"
# --------------------------------------------------------------------------
printf '    waiting for the release assets (native runners, ~5-10 min)'
release_url="https://github.com/$REPO/releases/download/$TAG"
for _ in $(seq 1 90); do
  if curl -sfI "$release_url/checksums.txt" >/dev/null 2>&1; then
    printf '\n'; ok "release is up"
    break
  fi
  printf '.'; sleep 20
done

if ! curl -sfI "$release_url/checksums.txt" >/dev/null 2>&1; then
  printf '\n'
  warn "timed out. Check https://github.com/$REPO/actions — if the builds are"
  warn "still running, run this same command again once they finish; nothing"
  warn "published so far is republished. docs/reference/releasing.md has each"
  warn "command if you would rather finish by hand."
  exit 1
fi

for asset in recall_darwin_amd64 recall_darwin_arm64 recall_linux_amd64 recall_linux_arm64 \
             recall-server_linux_amd64 recall-server_linux_arm64 \
             recall-worker_linux_amd64 recall-worker_linux_arm64; do
  if curl -sfI "$release_url/$asset.tar.gz" >/dev/null 2>&1; then
    ok "$asset.tar.gz"
  else
    die "$asset.tar.gz is missing from the release"
  fi
done
for asset in recall_windows_amd64 recall_windows_arm64; do
  if curl -sfI "$release_url/$asset.zip" >/dev/null 2>&1; then
    ok "$asset.zip"
  else
    die "$asset.zip is missing from the release"
  fi
done

# --------------------------------------------------------------------------
step "7/9  npm"
# --------------------------------------------------------------------------
# Scoped packages default to private, hence --access public. postinstall never
# runs during publish, so the download path is only exercised on install.
if curl -sf "https://registry.npmjs.org/@pimlabs%2Frecall/$VERSION" >/dev/null 2>&1; then
  ok "npm already has $VERSION — skipping"
elif confirm "npm publish @pimlabs/recall@$VERSION (a version cannot be unpublished after 72h)"; then
  if (cd npm && npm publish --access public); then
    ok "published — verify with: npm install -g @pimlabs/recall && recall version"
  else
    # Not fatal. crates.io has nothing to do with npm, and ending the run here
    # is what left v0.2.0 with four crates unpublished and no way to re-run.
    warn "npm publish failed — continuing, the steps below do not depend on it"
    warn "a 404 on PUT usually means a stale token, not a missing package: npm login"
    failed="$failed npm"
  fi
else
  warn "skipped npm"
fi

# --------------------------------------------------------------------------
step "8/9  crates.io"
# --------------------------------------------------------------------------
# Bottom-up, because each crate must be on the index before anything that
# depends on it can even be packaged.
if confirm "publish ${#CRATES[@]} crates to crates.io (a version can be yanked, never deleted)"; then
  i=0
  for n in "${CRATES[@]}"; do
    i=$((i + 1))
    # Asking the index costs one request and saves publishing a version that
    # is already there, which is what makes a second run of this script safe.
    if curl -sf "https://index.crates.io/${n:0:2}/${n:2:2}/$n" 2>/dev/null \
         | grep -q "\"vers\":\"$VERSION\""; then
      printf '    %-16s already at %s\n' "$n" "$VERSION"
      continue
    fi
    printf '    publishing %s ... ' "$n"
    if cargo publish -p "$n" >/tmp/release-publish.log 2>&1; then
      printf '%sok%s\n' "$green" "$off"
    else
      printf '%sFAILED%s\n' "$red" "$off"
      tail -20 /tmp/release-publish.log
      # Stop the loop but not the script: the crates after this one depend on
      # it and would fail anyway, while the formula below does not.
      warn "stopped at $n — the ones before it are published and will be skipped next run"
      failed="$failed crates.io"
      break
    fi
    # The index needs a moment; publishing the next crate too early fails
    # with "no matching package named ...". Indexed rather than ${CRATES[-1]},
    # which needs bash 4.3 — macOS still ships 3.2.
    [ "$i" -eq "${#CRATES[@]}" ] || sleep 20
  done
  [ -n "$failed" ] || ok "published — verify with: cargo install recall && recall version"
else
  warn "skipped crates.io"
fi

# --------------------------------------------------------------------------
step "9/9  Formula, and the tap"
# --------------------------------------------------------------------------
# Last, and that is the point: this rewrites Formula/recall.rb, and cargo
# refuses to publish from a repository with uncommitted changes anywhere in
# it. Run before step 8, as it used to be, it guarantees crates.io fails.
# The formula installs the prebuilt archives, so it needs the release's own
# four checksums rather than a hash of the source tarball. They are taken
# from checksums.txt, which the workflow generated from the artifacts it had
# just built — hashing the downloads again here would only prove curl works.
checksums_file="$(mktemp)"
curl -fsSL "$release_url/checksums.txt" -o "$checksums_file" \
  || die "could not download checksums.txt"
# The rewrite itself lives in scripts/update-formula.py, shared with the
# release workflow so a release cut from CI and one cut from here cannot
# write different formulas.
python3 scripts/update-formula.py "$VERSION" "$checksums_file" \
  || die "could not update Formula/recall.rb"
rm -f "$checksums_file"
ok "Formula/recall.rb updated"
git --no-pager diff --stat Formula/recall.rb

# --------------------------------------------------------------------------
step "9b/9  Publish the formula to the tap"
# --------------------------------------------------------------------------
# The formula has to reach $TAP for `brew install pimlabs/tap/recall` to see
# it. The push itself is scripts/push-tap.sh, shared with the release
# workflow.
if confirm "push the formula to $TAP"; then
  if ./scripts/push-tap.sh "$VERSION"; then
    ok "tap updated"
  else
    warn "could not push to $TAP"
    failed="$failed homebrew"
  fi
else
  warn "skipped — brew will keep installing whatever the tap currently has"
fi

step "Done"
cat <<EOF
    Verify each channel from a clean machine:

      curl -fsSL https://recall.pimlabs.id/install | bash
      npm install -g @pimlabs/recall
      cargo install recall
      brew install pimlabs/tap/recall

    Formula/recall.rb is modified and still needs a pull request — the tap
    has the built copy, this repository keeps the source it was built from,
    and until that merges this repository still names the previous version.
    Cutting the production server over is separate — see deploy/README.md.
EOF

if [ -n "$failed" ]; then
  warn "did not publish:$failed"
  echo "    Fix the cause and run the same command again. It reads each registry"
  echo "    first, so anything already published is skipped rather than retried."
  exit 1
fi
