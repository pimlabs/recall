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

case "$binary" in
  recall-server | recall-worker) ;;
  *) echo "fetch-release: $binary is not a binary a release publishes" >&2; exit 1 ;;
esac
case "$arch" in
  amd64 | arm64) ;;
  *) echo "fetch-release: no $binary release is built for $arch" >&2; exit 1 ;;
esac

asset="${binary}_linux_${arch}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "fetch-release: $binary $version for linux/$arch from $base"
curl -fsSL -o "$work/$asset.tar.gz" "$base/v$version/$asset.tar.gz"
curl -fsSL -o "$work/checksums.txt" "$base/v$version/checksums.txt"

# The line for this archive and no other. A release without one, or an
# archive that does not match it, is refused rather than installed.
line=$(grep " $asset.tar.gz\$" "$work/checksums.txt" || true)
if [ -z "$line" ]; then
  echo "fetch-release: checksums.txt has no entry for $asset.tar.gz" >&2
  exit 1
fi
(cd "$work" && echo "$line" | sha256sum -c -)

tar -xzf "$work/$asset.tar.gz" -C "$work"
mkdir -p "$(dirname "$dest")"
mv "$work/$asset" "$dest"
chmod 755 "$dest"
"$dest" version
