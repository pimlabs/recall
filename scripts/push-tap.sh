#!/usr/bin/env bash
# Push Formula/recall.rb to the Homebrew tap, where
# `brew install pimlabs/tap/recall` looks for it.
#
# Usage: push-tap.sh <version> [<tap clone url>]
#
# Shared by scripts/release.sh and the release workflow. The clone URL is an
# argument because the two authenticate differently: a laptop uses whatever
# git credentials it already has, CI passes a URL carrying a token scoped to
# the tap repository and nothing else.
#
# This used to end at "open a PR by hand", which is the step a release
# forgets: brew then installs the previous version's archives under the new
# version's name and fails on the checksum, saying nothing about why.
set -euo pipefail

VERSION="${1:?usage: push-tap.sh <version> [<tap clone url>]}"
URL="${2:-https://github.com/pimlabs/homebrew-tap.git}"

tapdir="$(mktemp -d)"
trap 'rm -rf "$tapdir"' EXIT

git clone --depth 1 --quiet "$URL" "$tapdir/tap"

{
  printf '# Generated from Formula/recall.rb in pimlabs/recall by a release. DO NOT EDIT.\n'
  printf '# Change it there and cut a release.\n'
  cat Formula/recall.rb
} >"$tapdir/tap/Formula/recall.rb"

if git -C "$tapdir/tap" diff --quiet -- Formula/recall.rb; then
  echo "    the tap already has this exact formula — nothing to push"
  exit 0
fi

git -C "$tapdir/tap" add Formula/recall.rb
git -C "$tapdir/tap" commit -q -m "recall $VERSION"
git -C "$tapdir/tap" push -q
echo "    pushed — brew install pimlabs/tap/recall now resolves $VERSION"
