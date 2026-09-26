#!/bin/sh
# Downloads a released recall-server (or recall-worker), checks it against
# the release's own checksums, and installs it at $3.
#
#   fetch-release.sh <version> <arch> <dest> [releases-url] [binary]
#
# Run by deploy/Dockerfile, so the image a server runs holds the exact
# binary the release published rather than one rebuilt from a checkout.
# <arch> is Docker's name for it (amd64, arm64). [releases-url] is where
# the v<version>/ directories live; GitHub's by default, and a local
# directory served over HTTP in CI, which has no release to point at.
# [binary] is recall-server unless it says recall-worker, which releases
# carry from the version that added the merge queue.
set -eu

version="$1"
arch="$2"
dest="$3"
base="${4:-https://github.com/pimlabs/recall/releases/download}"
binary="${5:-recall-server}"

# v0.4.5 is the last release whose server archives are named
# <binary>_linux_<arch>.tar.gz and hold one file renamed the same way. Every
# release after it names them <binary>-<rust target>.tar.gz, holding a
# <binary>-<rust target>/ directory with <binary> in it. Tags never move, so
# a rollback to 0.4.5 or older still finds the old names.
last_old_style=0.4.5

die() {
  echo "fetch-release: $*" >&2
  exit 1
}

# 0.4.5 -> 4005, so two versions compare as numbers. A pre-release counts
# as the version it leads up to (0.0.0-ci is 0.0.0).
version_number() {
  v="${1%%[-+]*}"
  case "$v" in
    *[!0-9.]* | "" | .* | *. | *..*) die "$1 is not a release version" ;;
  esac
  major="${v%%.*}"
  rest="${v#*.}"
  minor="${rest%%.*}"
  patch="${rest#*.}"
  case "$patch" in *.*) die "$1 is not a release version" ;; esac
  echo $((major * 1000000 + minor * 1000 + patch))
}

case "$binary" in
  recall-server | recall-worker) ;;
  *) die "$binary is not a binary a release publishes" ;;
esac
case "$arch" in
  amd64) target=x86_64-unknown-linux-musl ;;
  arm64) target=aarch64-unknown-linux-musl ;;
  *) die "no $binary release is built for $arch" ;;
esac

# Assigned first, so a version that is not one stops the script here under
# `set -e` rather than inside an `if`, where it would not.
wanted=$(version_number "$version")
cutoff=$(version_number "$last_old_style")
if [ "$wanted" -le "$cutoff" ]; then
  asset="${binary}_linux_${arch}"
  inner="$asset"
else
  asset="$binary-$target"
  inner="$asset/$binary"
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "fetch-release: $binary $version for linux/$arch from $base ($asset.tar.gz)"
curl -fsSL -o "$work/$asset.tar.gz" "$base/v$version/$asset.tar.gz"
curl -fsSL -o "$work/checksums.txt" "$base/v$version/checksums.txt"

# The line for this archive and no other. A release without one, or an
# archive that does not match it, is refused rather than installed.
line=$(grep " $asset.tar.gz\$" "$work/checksums.txt" || true)
if [ -z "$line" ]; then
  die "checksums.txt has no entry for $asset.tar.gz"
fi
(cd "$work" && echo "$line" | sha256sum -c -)

tar -xzf "$work/$asset.tar.gz" -C "$work"
[ -f "$work/$inner" ] || die "$asset.tar.gz did not contain $inner"
mkdir -p "$(dirname "$dest")"
mv "$work/$inner" "$dest"
chmod 755 "$dest"
"$dest" version
