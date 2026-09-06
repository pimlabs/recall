#!/usr/bin/env bash
#
# Cuts a release across all four install channels.
#
#   ./scripts/release.sh v0.1.0 --dry-run    # every check, no pushing
#   ./scripts/release.sh v0.1.0              # the real thing
#
# Everything up to the tag is automated and safe to re-run. The three steps
# that need your credentials — the tag push, `npm publish`, `cargo publish` —
# each stop and ask first, because none of them can be undone: a tag can be
# force-moved but people may already have pinned it, an npm version can be
# deprecated but not removed, and a crates.io version can be yanked but never
# deleted.
#
# See docs/releasing.md for what each step does and why the order matters.
set -uo pipefail

TAG="${1:-}"
DRY_RUN=false
[ "${2:-}" = "--dry-run" ] && DRY_RUN=true

REPO="pimlabs/recall"
CRATES=(recall-wire recall-paths recall-hooks recall-server recall-sync)
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
step "1/8  Working tree"
# --------------------------------------------------------------------------
[ -z "$(git status --porcelain)" ] || die "uncommitted changes — commit or stash first"
ok "clean"

branch="$(git rev-parse --abbrev-ref HEAD)"
[ "$branch" = "main" ] || die "on '$branch'; releases are cut from main"
ok "on main"

git fetch origin main --quiet || die "could not reach origin"
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
  || die "main is not in sync with origin/main — pull or push first"
ok "in sync with origin/main"

if git rev-parse "$TAG" >/dev/null 2>&1; then
  die "$TAG already exists locally. A released tag must not be moved."
fi
if git ls-remote --tags origin "refs/tags/$TAG" | grep -q .; then
  die "$TAG already exists on origin. Releases are immutable; bump the version."
fi
ok "$TAG is unused"

# --------------------------------------------------------------------------
step "2/8  Versions agree"
# --------------------------------------------------------------------------
# These three are not linked to each other. npm's postinstall looks for a
# release named after its *own* version, so a drift here ships a package that
# installs nothing.
cargo_v=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
npm_v=$(grep -m1 '"version"' npm/package.json | cut -d'"' -f4)
formula_v=$(grep -m1 'refs/tags' Formula/recall.rb | sed 's#.*refs/tags/v\([^.]*\.[^.]*\.[^.]*\)\.tar\.gz.*#\1#')

printf '    Cargo.toml %s | npm %s | Formula %s | tag %s\n' \
  "$cargo_v" "$npm_v" "$formula_v" "$VERSION"
[ "$cargo_v" = "$VERSION" ] || die "Cargo.toml says $cargo_v, you asked for $VERSION"
[ "$npm_v" = "$VERSION" ]   || die "npm/package.json says $npm_v, you asked for $VERSION"
[ "$formula_v" = "$VERSION" ] || warn "Formula still points at v$formula_v — step 6 fixes this"
ok "versions line up"

# --------------------------------------------------------------------------
step "3/8  The suite"
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
run "compat-check.sh (11 checks)"      ./scripts/compat-check.sh target/release/recall
run "api-doc-check.sh (27 checks)"     ./scripts/api-doc-check.sh target/release/recall
run "trusted-ip-check.sh (9 checks)"   ./scripts/trusted-ip-check.sh target/release/recall

built=$(./target/release/recall version)
printf '    built: %s\n' "$built"
case "$built" in
  "recall $VERSION"*) ok "binary reports $VERSION" ;;
  *) die "binary reports '$built', expected recall $VERSION" ;;
esac

# --------------------------------------------------------------------------
step "4/8  crates.io names"
# --------------------------------------------------------------------------
# Only meaningful on a first publish, but cheap, and the reason recall-cli
# had to become recall-sync.
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
[ "$taken" -eq 0 ] && ok "all five names are free" \
  || warn "$taken already on crates.io — fine if that is you republishing, fatal if not"

# --------------------------------------------------------------------------
step "5/8  Tag and push"
# --------------------------------------------------------------------------
if confirm "create and push $TAG (this publishes a GitHub Release)"; then
  git tag -a "$TAG" -m "recall $VERSION" || die "could not create the tag"
  git push origin "$TAG" || { git tag -d "$TAG"; die "could not push the tag (local tag removed)"; }
  ok "pushed $TAG — the release workflow is now building four targets"
  echo "    https://github.com/$REPO/actions"
else
  warn "skipped — nothing after this point can run"
  exit 0
fi

# --------------------------------------------------------------------------
step "6/8  Wait for the release, then fix the formula"
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
  warn "timed out. Check https://github.com/$REPO/actions, then re-run steps 6-8 by hand"
  warn "(docs/releasing.md has each command)"
  exit 1
fi

for asset in recall_darwin_amd64 recall_darwin_arm64 recall_linux_amd64 recall_linux_arm64; do
  if curl -sfI "$release_url/$asset.tar.gz" >/dev/null 2>&1; then
    ok "$asset.tar.gz"
  else
    die "$asset.tar.gz is missing from the release"
  fi
done

if command -v shasum >/dev/null; then sum() { shasum -a 256; }
elif command -v sha256sum >/dev/null; then sum() { sha256sum; }
else die "need shasum or sha256sum to hash the source tarball"; fi
sha=$(curl -fsSL "https://github.com/$REPO/archive/refs/tags/$TAG.tar.gz" | sum | cut -d' ' -f1)
printf '    source tarball sha256: %s\n' "$sha"
if [ -n "$sha" ]; then
  python3 - "$sha" "$TAG" <<'PY'
import re, sys
sha, tag = sys.argv[1], sys.argv[2]
p = "Formula/recall.rb"
s = open(p).read()
s = re.sub(r'refs/tags/v[0-9.]+\.tar\.gz', f'refs/tags/{tag}.tar.gz', s)
s = re.sub(r'sha256 "[a-f0-9]*"', f'sha256 "{sha}"', s)
open(p, "w").write(s)
PY
  ok "Formula/recall.rb updated — commit it on a branch and open a PR"
  git --no-pager diff --stat Formula/recall.rb
fi

# --------------------------------------------------------------------------
step "7/8  npm"
# --------------------------------------------------------------------------
# Scoped packages default to private, hence --access public. postinstall never
# runs during publish, so the download path is only exercised on install.
if confirm "npm publish @pimlabs/recall@$VERSION (a version cannot be unpublished after 72h)"; then
  (cd npm && npm publish --access public) || die "npm publish failed"
  ok "published — verify with: npm install -g @pimlabs/recall && recall version"
else
  warn "skipped npm"
fi

# --------------------------------------------------------------------------
step "8/8  crates.io"
# --------------------------------------------------------------------------
# Bottom-up, because each crate must be on the index before anything that
# depends on it can even be packaged.
if confirm "publish ${#CRATES[@]} crates to crates.io (a version can be yanked, never deleted)"; then
  i=0
  for n in "${CRATES[@]}"; do
    printf '    publishing %s ... ' "$n"
    if cargo publish -p "$n" >/tmp/release-publish.log 2>&1; then
      printf '%sok%s\n' "$green" "$off"
    else
      printf '%sFAILED%s\n' "$red" "$off"
      tail -20 /tmp/release-publish.log
      die "stopped at $n. Fix it, then resume from here — the ones before it are already published."
    fi
    # The index needs a moment; publishing the next crate too early fails
    # with "no matching package named ...". Indexed rather than ${CRATES[-1]},
    # which needs bash 4.3 — macOS still ships 3.2.
    i=$((i + 1))
    [ "$i" -eq "${#CRATES[@]}" ] || sleep 20
  done
  ok "published — verify with: cargo install recall-sync && recall version"
else
  warn "skipped crates.io"
fi

step "Done"
cat <<EOF
    Verify each channel from a clean machine:

      curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | bash
      npm install -g @pimlabs/recall
      cargo install recall-sync
      brew tap pimlabs/recall https://github.com/$REPO && brew install pimlabs/recall/recall

    Then commit the Formula/recall.rb change on a branch and open a PR.
    Cutting the production server over is separate — see deploy/README.md.
EOF
