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
# changes under a server that already pulled it; a registry that cannot say
# whether it is there stops the job rather than risk that. Under GitHub
# Actions each image's digest becomes a step output (server_digest,
# worker_digest), for the provenance attestation after it, whether this run
# pushed it or found it already there.
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

# Under GitHub Actions, the digest the attestation after this is made for.
say_digest() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "${1}_digest=$2" >>"$GITHUB_OUTPUT"
  fi
}

for entry in "${IMAGES[@]}"; do
  read -r name target description <<<"$entry"
  ref="$REGISTRY/$name:$version"

  # Asked with the job's own login, so a package that is still private is
  # seen. Only two answers lead anywhere: the tag is there, or the registry
  # says plainly that it is not (buildx's "<ref>: not found", its word for a
  # 404). Anything else, a timeout, a 5xx, a 429, a refusal, is no evidence
  # either way, and pushing on it could replace a published tag with
  # different bytes, since the base images under it move. So it stops here,
  # and a re-run asks again.
  if [ "$mode" = push ]; then
    if answer=$(docker buildx imagetools inspect --format '{{json .Manifest}}' "$ref" 2>&1); then
      digest=$(printf '%s' "$answer" | jq -r '.digest // empty' 2>/dev/null || true)
      [ -n "$digest" ] || die "$ref is published, but its digest could not be read from: $answer"
      echo "release-image: $ref is already published ($digest); leaving it as it is"
      # Still named, so an attestation that failed after an earlier push
      # is made on the re-run.
      say_digest "$target" "$digest"
      continue
    fi
    case "$answer" in
      *"$ref: not found"* | *MANIFEST_UNKNOWN* | *NAME_UNKNOWN*) ;;
      *) die "could not tell whether $ref is already published, so nothing was pushed: $answer" ;;
    esac
    echo "release-image: $ref is not published yet"
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
    say_digest "$target" "$digest"
  fi
done
