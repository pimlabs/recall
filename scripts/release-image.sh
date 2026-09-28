#!/usr/bin/env bash
# Builds the images a release publishes, and publishes them:
#
#   scripts/release-image.sh status   <version>
#   scripts/release-image.sh build    <version> <commit> <arch> push|check [<digest-dir>]
#   scripts/release-image.sh manifest <version> <digest-dir>
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
# checked against its checksums.txt. Nothing is compiled.
#
# Each architecture is built on a machine of that architecture, never under
# emulation: under QEMU, the arm64 half's `npm install` of the claude CLI
# died with an illegal instruction and the build then hung without end
# (CI, 2026-09-28). So `build` makes one platform, the runner's own, and
# `manifest` joins the two into one multi-platform tag, Docker's pattern for
# distributing a build across runners.
#
# release.yml, in order:
#
#   status    (`check-image`) Is each image's tag already published? Asked
#             with the job's login, so a package that is still private is
#             seen. Found, or plainly not found (buildx's "<ref>: not
#             found"); anything else, a timeout, a 5xx, a 429, a refusal, is
#             no evidence either way and stops the run, since pushing on it
#             could replace a published tag with different bytes. Writes
#             build=true when anything needs building.
#   build     (`build-image`, one leg per arch) Builds both images for
#             linux/<arch> and pushes them by digest, with no tag, writing
#             each digest to <digest-dir>/<image>-<arch>. Then runs the
#             result on this runner: the claude CLI, and recall-server or
#             recall-worker, each asked its version.
#   manifest  (`publish-image`) Asks again, the same three ways: a tag that
#             is there is left alone and its digest reported; one that is
#             plainly not there is made from the two per-arch digests, with
#             the index's annotations. Under GitHub Actions each tag's
#             digest becomes a step output (server_digest, worker_digest)
#             for the provenance attestation after it.
#
# A published tag therefore never moves, and a re-run of a published
# version builds nothing.
#
# `build ... check` is ci.yml's `check-publish-image`, one leg per arch: the
# same build, arguments and provenance, with push=false, then the same run
# of what was built. There is no release of an unmerged change, so CI names
# the newest one that exists.
set -euo pipefail

REGISTRY=ghcr.io/pimlabs
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

usage() {
  die "usage: $0 status <version> | build <version> <commit> <arch> push|check [<digest-dir>] | manifest <version> <digest-dir>"
}

# Under GitHub Actions, a step output.
output() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "$1=$2" >>"$GITHUB_OUTPUT"
  fi
}

# published <ref>: prints the tag's digest and returns 0 when it is there;
# returns 3 when the registry says plainly that it is not; exits non-zero
# otherwise (1, from die), which stops the script.
published() {
  local answer digest
  if answer=$(docker buildx imagetools inspect --format '{{json .Manifest}}' "$1" 2>&1); then
    digest=$(printf '%s' "$answer" | jq -r '.digest // empty' 2>/dev/null || true)
    [ -n "$digest" ] || die "$1 is published, but its digest could not be read from: $answer"
    printf '%s\n' "$digest"
    return 0
  fi
  case "$answer" in
    *"$1: not found"* | *MANIFEST_UNKNOWN* | *NAME_UNKNOWN*) return 3 ;;
    *) die "could not tell whether $1 is already published, so nothing was pushed: $answer" ;;
  esac
}

mode="${1:-}"
version="${2:-}"
[ -n "$mode" ] || usage
# Only a plain release version: it becomes an image tag and a URL path.
printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || die "'$version' is not a release version like 0.4.7"

cd "$(dirname "$0")/.."

case "$mode" in
  status)
    build=false
    for entry in "${IMAGES[@]}"; do
      read -r name target _ <<<"$entry"
      ref="$REGISTRY/$name:$version"
      # A command substitution in `if` would hide published()'s exit on an
      # ambiguous answer, so its status is read after it returns.
      found=true
      digest=$(published "$ref") || {
        status=$?
        [ "$status" -eq 3 ] || exit "$status"
        found=false
      }
      if [ "$found" = true ]; then
        echo "release-image: $ref is already published ($digest)"
      else
        echo "release-image: $ref is not published yet"
        build=true
      fi
    done
    output build "$build"
    ;;

  build)
    commit="${3:-}"
    arch="${4:-}"
    how="${5:-}"
    digests="${6:-}"
    [ -n "$commit" ] || usage
    case "$arch" in amd64 | arm64) ;; *) die "'$arch' is not amd64 or arm64" ;; esac
    case "$how" in
      push) [ -n "$digests" ] || die "build ... push needs a <digest-dir>" ;;
      check) ;;
      *) usage ;;
    esac
    # Only the runner's own architecture: see the top of this file.
    here=$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')
    [ "$here" = "$arch" ] || die "this is a $here machine; linux/$arch is built on a $arch one, never under emulation"
    push=false
    [ "$how" = push ] && push=true
    [ -z "$digests" ] || mkdir -p "$digests"
    out=$(mktemp -d)
    trap 'rm -rf "$out"' EXIT

    for entry in "${IMAGES[@]}"; do
      read -r name target _ <<<"$entry"
      # The build arguments are the ones the compose files pass:
      # RECALL_SOURCE, RECALL_VERSION and GIT_COMMIT (the tag's short commit,
      # which is what a deploy passed when the server built this image for
      # itself). Pushed by digest, under the image's name and no tag: the
      # tag is made once both architectures are there.
      args=(
        --file deploy/Dockerfile
        --target "$target"
        --platform "linux/$arch"
        --build-arg RECALL_SOURCE=release
        --build-arg RECALL_VERSION="$version"
        --build-arg GIT_COMMIT="$commit"
      )
      echo "::group::$REGISTRY/$name for linux/$arch (push=$push)"
      docker buildx build "${args[@]}" \
        --provenance mode=max \
        --output "type=image,name=$REGISTRY/$name,push-by-digest=true,name-canonical=true,push=$push" \
        --metadata-file "$out/$name.json" \
        .
      echo "::endgroup::"
      if [ "$how" = push ]; then
        digest=$(jq -r '."containerimage.digest" // empty' "$out/$name.json")
        [ -n "$digest" ] || die "the build of $name for linux/$arch reported no digest"
        echo "release-image: pushed $REGISTRY/$name@$digest (linux/$arch)"
        printf '%s\n' "$digest" >"$digests/$name-$arch"
      fi

      # What was just built, run here: the same build again, every layer
      # from the cache above, loaded into Docker. An image whose CLI or
      # binary cannot start on its own architecture fails here, in seconds.
      smoke="release-image-smoke/$name:$version-$arch"
      echo "::group::$name for linux/$arch, run"
      docker buildx build "${args[@]}" --provenance false --output "type=docker,name=$smoke" . \
        >"$out/$name-load.log" 2>&1 || { cat "$out/$name-load.log"; die "could not load $name for linux/$arch to run it"; }
      timeout 120 docker run --rm --entrypoint claude "$smoke" --version
      timeout 60 docker run --rm --entrypoint "$name" "$smoke" version
      docker image rm "$smoke" >/dev/null
      echo "::endgroup::"
    done
    ;;

  manifest)
    digests="${3:-}"
    [ -n "$digests" ] || usage
    for entry in "${IMAGES[@]}"; do
      read -r name target description <<<"$entry"
      ref="$REGISTRY/$name:$version"
      found=true
      digest=$(published "$ref") || {
        status=$?
        [ "$status" -eq 3 ] || exit "$status"
        found=false
      }
      if [ "$found" = true ]; then
        # Still reported, so an attestation that failed after an earlier
        # run made the tag is made on the re-run.
        echo "release-image: $ref is already published ($digest); leaving it as it is"
        output "${target}_digest" "$digest"
        continue
      fi
      sources=()
      for arch in amd64 arm64; do
        [ -s "$digests/$name-$arch" ] || die "$ref is not published, and there is no linux/$arch build of it in $digests"
        sources+=("$REGISTRY/$name@$(cat "$digests/$name-$arch")")
      done
      # The labels a registry shows are in the Dockerfile, on each
      # platform's image; a multi-platform tag is an index, and GHCR reads
      # the index's own annotations, so the same few go there too. `source`
      # is what links the package to this repository.
      docker buildx imagetools create \
        --tag "$ref" \
        --annotation "index:org.opencontainers.image.source=$SOURCE_URL" \
        --annotation "index:org.opencontainers.image.description=$description" \
        --annotation "index:org.opencontainers.image.licenses=MIT" \
        --annotation "index:org.opencontainers.image.version=$version" \
        "${sources[@]}"
      digest=$(published "$ref") || die "$ref was just made, and is not there"
      echo "release-image: published $ref ($digest), from ${sources[*]}"
      output "${target}_digest" "$digest"
    done
    ;;

  *) usage ;;
esac
