#!/usr/bin/env bash
# Is the image a release's tag names the one that release built?
#
#   scripts/verify-release-image.sh <version> [<tag>]
#
# For ghcr.io/pimlabs/recall-server and recall-worker, resolves the tag
# (<version> unless <tag> is given; ci.yml gives another to prove a
# mismatch fails) to the digest of the multi-platform index it names now,
# hashed from the index's own bytes, fetched anonymously as a server
# fetches them. Then two checks of that digest, which are what a tag
# cannot fake, since the digest is the hash of the bytes:
#
#   - the index must say it is <version>: release-image.sh writes
#     org.opencontainers.image.version into it, and the digest covers it,
#     so a tag pointed at an older release's image, attested too, fails;
#   - its GitHub attestation must verify, for this repository, signed by
#     release.yml running on the tag v<version> exactly, the identity
#     publish-image's actions/attest signs with.
#
# Prints each verified digest, and under GitHub Actions writes them as the
# step outputs server_digest and worker_digest. deploy.yml hands those to
# the server, which pulls them by digest and nothing else.
#
# Exits 1 when the server's image fails either check or cannot be read,
# or when the worker's is read and fails either check. A worker tag that
# cannot be read only warns, and gives no worker_digest: a server that runs
# the worker then refuses to deploy, and one that does not is unaffected.
#
# Needs docker with buildx, jq, and gh with a token that can read this
# repository's attestations (GH_TOKEN; `attestations: read` in a workflow).
#
# Run by deploy.yml before it deploys, and by ci.yml's check-release-image
# against the newest published release.
set -euo pipefail

REPOSITORY=pimlabs/recall
REGISTRY=ghcr.io/pimlabs

version="${1:-}"
tag="${2:-$version}"
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || ! printf '%s' "$tag" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "usage: $0 <version> [<tag>], each like 0.4.7" >&2
  exit 2
fi
identity="https://github.com/$REPOSITORY/.github/workflows/release.yml@refs/tags/v$version"

# Under GitHub Actions, an annotation; anywhere else, a plain line.
say() {
  local level="$1"
  shift
  if [ "${GITHUB_ACTIONS:-}" = true ]; then
    echo "::$level::$*"
  else
    echo "verify-release-image: $level: $*" >&2
  fi
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

for name in recall-server recall-worker; do
  ref="$REGISTRY/$name:$tag"
  index="$work/$name.json"
  if ! docker buildx imagetools inspect --raw "$ref" >"$index"; then
    if [ "$name" = recall-server ]; then
      say error "Could not read $ref from GHCR (the error is above), so it could not be resolved to a digest and verified."
      exit 1
    fi
    say warning "Could not read $ref from GHCR, so it has no verified digest. A server that runs the worker (COMPOSE_PROFILES=worker) will refuse to deploy until it has one."
    continue
  fi
  digest="sha256:$(sha256sum <"$index" | cut -d ' ' -f 1)"

  labelled=$(jq -r '.annotations["org.opencontainers.image.version"] // empty' "$index" 2>/dev/null || true)
  if [ "$labelled" != "$version" ]; then
    say error "$ref resolves to $digest, whose index says it is version '${labelled:-(none)}', not $version: the tag does not name $version's image."
    exit 1
  fi

  if ! gh attestation verify "oci://$REGISTRY/$name@$digest" \
    --repo "$REPOSITORY" \
    --cert-identity "$identity" \
    >/dev/null; then
    say error "$REGISTRY/$name@$digest, which $ref names, could not be verified as $version's image (gh's reason is above). Either it has no attestation from $identity, which is what this check is for, or GitHub's attestations API or Sigstore could not be reached, which re-running later fixes. If the release's attest step failed, re-run its publish-image job, which attests the image it already pushed."
    exit 1
  fi

  echo "$ref is $REGISTRY/$name@$digest, attested by $identity"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "${name#recall-}_digest=$digest" >>"$GITHUB_OUTPUT"
  fi
done
