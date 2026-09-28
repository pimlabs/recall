#!/usr/bin/env bash
# Can a machine with no credentials pull this image, for both platforms?
#
#   scripts/image-pullable.sh ghcr.io/pimlabs/recall-server:0.4.7
#
# Asks GHCR the way `docker pull` does, anonymously: an anonymous pull
# token for the repository, then the tag's manifest. A server that deploys
# a release pulls its image with no `docker login`, so this is the question
# that decides whether a deploy can work, asked before one starts.
#
# Exits 0 when the tag is there, public, and names linux/amd64 and
# linux/arm64. Otherwise says which of these it is, and exits 1:
#
#   - the package does not exist, or is private. GHCR answers both the same
#     way, on purpose. A package the release workflow creates starts
#     private, whatever the repository's visibility, and GitHub has no API
#     to change that, so the first release after images began to be
#     published needs one visit to the package's settings;
#   - the package is public but has no such tag: that version's image was
#     never pushed;
#   - the tag is missing a platform.
#
# Run by release.yml's `publish-image` job after it pushes, and by
# deploy.yml before it deploys.
set -euo pipefail

ref="${1:-}"
case "$ref" in
  ghcr.io/*/*:*) ;;
  *)
    echo "usage: $0 ghcr.io/<owner>/<name>:<tag>" >&2
    exit 2
    ;;
esac
repo="${ref#ghcr.io/}"
tag="${repo##*:}"
repo="${repo%:*}"
owner="${repo%%/*}"
name="${repo#*/}"
settings="https://github.com/orgs/$owner/packages/container/$name/settings"

say() { echo "image-pullable: $*"; }

tokens=$(curl -sS --max-time 30 -A recall-dev \
  "https://ghcr.io/token?scope=repository:$repo:pull") || {
  say "could not reach ghcr.io to ask about $ref"
  exit 1
}
token=$(printf '%s' "$tokens" | jq -r '.token // empty' 2>/dev/null || true)
if [ -z "$token" ]; then
  say "ghcr.io/$repo cannot be pulled without credentials: the package is private, or does not exist yet."
  say "If a release has pushed it, make it public, once: $settings"
  say "(Danger Zone -> Change visibility -> Public). A new package is private whatever the repository is."
  exit 1
fi

body=$(mktemp)
trap 'rm -f "$body"' EXIT
code=$(curl -sS --max-time 30 -A recall-dev -o "$body" -w '%{http_code}' \
  -H "Authorization: Bearer $token" \
  -H 'Accept: application/vnd.oci.image.index.v1+json' \
  -H 'Accept: application/vnd.docker.distribution.manifest.list.v2+json' \
  "https://ghcr.io/v2/$repo/manifests/$tag") || {
  say "could not reach ghcr.io to ask about $ref"
  exit 1
}
case "$code" in
  200) ;;
  404)
    say "ghcr.io/$repo is public, but has no tag $tag: that version's image was never published."
    exit 1
    ;;
  *)
    say "ghcr.io answered $code for $ref"
    exit 1
    ;;
esac

# The attestation manifests beside the images are platform unknown/unknown,
# and are not what a server pulls; only the real platforms count.
if ! jq -e '.manifests | type == "array"' "$body" >/dev/null 2>&1; then
  say "$ref is a single image, not the multi-platform index a release publishes"
  exit 1
fi
platforms=$(jq -r '[.manifests[].platform | select(.os != "unknown") | "\(.os)/\(.architecture)"] | sort | unique | join(" ")' "$body")
missing=""
for want in linux/amd64 linux/arm64; do
  case " $platforms " in
    *" $want "*) ;;
    *) missing="$missing $want" ;;
  esac
done
if [ -n "$missing" ]; then
  say "$ref has platforms '${platforms:-none}', and is missing:$missing"
  exit 1
fi
say "$ref can be pulled without credentials ($platforms)"
