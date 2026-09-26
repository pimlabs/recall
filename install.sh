#!/usr/bin/env bash
# Recall installer, for machines without npm or Homebrew:
#
#   curl -fsSL https://recall.pimlabs.id/install | bash
#
# That URL is a Cloudflare Worker that fetches this file from main on every
# request — install-worker.js in this repository. It proxies rather than
# copies, so this file stays the only version of the installer and nothing
# can serve a stale one, which matters because the checksum verification
# below is exactly what a stale copy would be missing.
#
# Downloads the prebuilt binary for this platform from the latest GitHub
# release, checks it against that release's checksums.txt, and installs it
# to ~/.local/bin/recall; override with RECALL_BIN_DIR, or pin a version
# with RECALL_VERSION=v0.1.0.
#
# RECALL_TEST_RELEASES_URL is for this repository's own CI and nothing else:
# it replaces https://github.com/pimlabs/recall/releases, so
# scripts/installer-test.sh can serve a release from loopback. The checksums
# come from the same place as the archive, so pointing it anywhere else
# verifies nothing. Leave it unset.
set -euo pipefail

REPO="pimlabs/recall"
RELEASES="${RECALL_TEST_RELEASES_URL:-https://github.com/$REPO/releases}"
BIN_DIR="${RECALL_BIN_DIR:-$HOME/.local/bin}"
VERSION="${RECALL_VERSION:-latest}"

# v0.4.5 is the last release whose archives are named recall_<os>_<arch>
# and hold one binary renamed the same way. Every release after it names
# them recall-<rust target>, holding a recall-<rust target>/ directory with
# `recall` in it. Tags never move, so both stay true for good.
LAST_OLD_STYLE_RELEASE=0.4.5

die() {
  echo "install: $*" >&2
  exit 1
}

# True when $1 (v0.4.5, 0.4.5, v0.4.6-rc.1, ...) was released with the old
# archive names. A pre-release counts as the version it leads up to.
old_style_archives() {
  local v a b c x y z
  v="${1#v}"
  v="${v%%[-+]*}"
  IFS=. read -r a b c <<<"$v"
  IFS=. read -r x y z <<<"$LAST_OLD_STYLE_RELEASE"
  ((10#$a < 10#$x || (10#$a == 10#$x && (10#$b < 10#$y || (10#$b == 10#$y && 10#$c <= 10#$z)))))
}

for dep in curl tar uname awk; do
  command -v "$dep" >/dev/null || die "$dep is required but not installed"
done

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$os" in
  darwin | linux) ;;
  # Git Bash / MSYS2's `uname -s` says one of these, never darwin or linux.
  # A Windows binary needs a Windows archive this script does not fetch, so
  # send whoever ran this under Git Bash to the installer that does, rather
  # than failing with an OS name that names nothing to do about it.
  mingw* | msys* | cygwin*)
    die "this is install.ps1's job on Windows, not install.sh's:
    irm https://recall.pimlabs.id/install.ps1 | iex
  Run that from PowerShell, not Git Bash." ;;
  *) die "unsupported OS: $os (macOS and Linux only)" ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64 | amd64) arch="amd64" ;;
  arm64 | aarch64) arch="arm64" ;;
  *) die "unsupported architecture: $arch" ;;
esac

case "$os-$arch" in
  darwin-amd64) target="x86_64-apple-darwin" ;;
  darwin-arm64) target="aarch64-apple-darwin" ;;
  linux-amd64) target="x86_64-unknown-linux-gnu" ;;
  linux-arm64) target="aarch64-unknown-linux-gnu" ;;
esac

build_hint="If no release exists yet, build from source instead:
      git clone https://github.com/$REPO && cd recall
      cargo build --release -p recall   # binary at target/release/recall
    Or, without a Rust toolchain of your own:
      brew install --HEAD pimlabs/tap/recall"

# "latest" is resolved to a tag first, because the archive's name depends
# on which release it is. GitHub answers /releases/latest with a redirect to
# /releases/tag/<tag>, and that last path segment is the version: one HEAD
# request, and no API rate limit to run into.
if [[ "$VERSION" == "latest" ]]; then
  resolved="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES/latest")" \
    || die "could not look up the latest release at $RELEASES/latest
    $build_hint"
  resolved="${resolved%/}"
  VERSION="${resolved##*/}"
  echo "install: the latest release is $VERSION"
fi
VERSION="v${VERSION#v}"
[[ "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]] \
  || die "not a release version: $VERSION (expected something like v0.4.6)"

if old_style_archives "$VERSION"; then
  asset="recall_${os}_${arch}.tar.gz"
  inner="recall_${os}_${arch}"
else
  asset="recall-$target.tar.gz"
  inner="recall-$target/recall"
fi
base="$RELEASES/download/$VERSION"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "install: downloading $asset ($VERSION)..."
if ! curl -fsSL "$base/$asset" -o "$tmp/$asset"; then
  die "could not download $base/$asset
    $build_hint"
fi
curl -fsSL "$base/checksums.txt" -o "$tmp/checksums.txt" \
  || die "could not download $base/checksums.txt"

# checksums.txt lists every archive in the release; only the line naming
# this one counts. Nothing is unpacked, let alone made executable, before it
# matches.
expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/checksums.txt")"
[ -n "$expected" ] || die "checksums.txt has no line for $asset; the release may be incomplete"
if command -v sha256sum >/dev/null; then
  actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
elif command -v shasum >/dev/null; then
  actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
else
  die "need sha256sum or shasum to check the download, and found neither"
fi
[ "$actual" = "$expected" ] \
  || die "checksum mismatch for $asset: got $actual, checksums.txt says $expected. The download may be corrupt or tampered with; nothing was installed."

tar -xzf "$tmp/$asset" -C "$tmp"
[ -f "$tmp/$inner" ] \
  || die "$asset did not contain $inner; please report it at https://github.com/$REPO/issues"
mkdir -p "$BIN_DIR"
install -m 0755 "$tmp/$inner" "$BIN_DIR/recall"

echo "install: installed $BIN_DIR/recall"
"$BIN_DIR/recall" version

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "  ! $BIN_DIR isn't on your PATH. Add to your shell profile:"
    echo "      export PATH=\"$BIN_DIR:\$PATH\""
    ;;
esac

cat <<'EOF'

Next: set RECALL_URL and RECALL_TOKEN (see docs/reference/token-setup.md), then run
'recall init' inside a project you want synced.
EOF
