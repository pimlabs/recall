#!/usr/bin/env bash
# Builds the images a release publishes, and with `push` publishes them:
#
#   scripts/release-image.sh push  <version> <commit>
#   scripts/release-image.sh check <version> [<commit>]
#
# Two images, one per service in the compose files that is built from this
# repository: ghcr.io/pimlabs/recall-server:<version> (the Dockerfile's
# `server` stage) and ghcr.io/pimlabs/recall-worker:<version> (`worker`).
# sqlite-web and cloudflared are other people's images and are not ours to
# publish.
#
# Each is built exactly the way a server built it for itself before this
# existed (`docker compose up -d --build` in a checkout of the tag): from
# deploy/Dockerfile's `release` source, so it holds the recall-server and
# recall-worker that release published, fetched from the GitHub Release and
# checked against its checksums.txt. Nothing is compiled. Both platforms a
# release builds binaries for, linux/amd64 and linux/arm64, in one
# multi-platform image each; a builder that is not arm64 runs the arm64
# half's RUN steps under QEMU.
#
# `push` is release.yml's `publish-image` job. It needs a buildx builder that
# can build for both platforms, and a `docker login ghcr.io` that may push.
# A version whose tag is already in the registry is left alone rather than
# rebuilt, so re-running a failed job is safe and a published tag never
# changes under a server that already pulled it. Under GitHub Actions each
# pushed image's digest becomes a step output (server_digest,
# worker_digest), for the provenance attestation after it; an image that was
# already there has none, since this run did not build it.
#
# `check` is ci.yml's `check-publish-image` job: the same build, the same
# arguments, the same annotations and provenance, with push=false, so a
# change that would break the release's image fails a pull request instead.
# There is no release of an unmerged change, so CI names the newest one
# that exists.
set -euo pipefail

REGISTRY=ghcr.io/pimlabs
PLATFORMS=linux/amd64,linux/arm64
SOURCE_URL=https://github.com/pimlabs/recall
# image name  Dockerfile target  description
IMAGES=(
  "recall-server server Recall's sync server"
  "recall-worker worker Recall's merge worker"
)

die() {
  echo "release-image: $*" >&2
  exit 1
}

mode="${1:-}"
version="${2:-}"
commit="${3:-}"
case "$mode" in
  push) [ -n "$commit" ] || die "push needs <version> <commit>" ;;
  check) ;;
  *) die "usage: $0 push <version> <commit> | check <version> [<commit>]" ;;
esac
# Only a plain release version: it becomes an image tag and a URL path.
printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || die "'$version' is not a release version like 0.4.7"

cd "$(dirname "$0")/.."

out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

for entry in "${IMAGES[@]}"; do
  read -r name target description <<<"$entry"
  ref="$REGISTRY/$name:$version"

  if [ "$mode" = push ] && docker buildx imagetools inspect "$ref" >/dev/null 2>&1; then
    echo "release-image: $ref is already published; leaving it as it is"
    continue
  fi

  push=false
  [ "$mode" = push ] && push=true
  echo "::group::$ref ($PLATFORMS, push=$push)"
  # The build arguments are the ones the compose files pass: RECALL_SOURCE,
  # RECALL_VERSION and GIT_COMMIT (the tag's short commit, which is what a
  # deploy passed when the server built this image for itself).
  #
  # The labels a registry shows are in the Dockerfile, on each platform's
  # image; a multi-platform image is an index, and GHCR reads the index's
  # own annotations, so the same few go there too. `source` is what links
  # the package to this repository.
  docker buildx build \
    --file deploy/Dockerfile \
    --target "$target" \
    --platform "$PLATFORMS" \
    --build-arg RECALL_SOURCE=release \
    --build-arg RECALL_VERSION="$version" \
    --build-arg GIT_COMMIT="$commit" \
    --annotation "index:org.opencontainers.image.source=$SOURCE_URL" \
    --annotation "index:org.opencontainers.image.description=$description" \
    --annotation "index:org.opencontainers.image.licenses=MIT" \
    --annotation "index:org.opencontainers.image.version=$version" \
    --provenance mode=max \
    --output "type=image,name=$ref,push=$push" \
    --metadata-file "$out/$name.json" \
    .
  echo "::endgroup::"

  if [ "$mode" = push ]; then
    digest=$(jq -r '."containerimage.digest" // empty' "$out/$name.json")
    [ -n "$digest" ] || die "the build of $ref reported no digest"
    echo "release-image: pushed $ref@$digest"
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
      echo "${target}_digest=$digest" >>"$GITHUB_OUTPUT"
    fi
  fi
done
