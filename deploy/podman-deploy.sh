#!/usr/bin/env bash
#
# Deploys a release of recall-server under rootless Podman, from the Quadlet
# units in deploy/quadlet/, for the user who runs it. Run it from a checkout
# of the release's tag: .github/workflows/deploy.yml does, over SSH, when
# DEPLOY_RUNTIME is podman, and by hand it is the same command.
#
#   deploy/podman-deploy.sh 0.4.17 sha256:<digest>   # those bytes, as deploy.yml pulls them
#   deploy/podman-deploy.sh 0.4.17                   # the tag, trusted
#
# Everything it checks, it checks before it changes anything, so a deploy
# that stops early leaves the running server as it was. Then it pulls the
# image, installs the units, restarts recall-server, and waits for /health.
# Once the new version answers, it removes older version tags of the image,
# keeping the one before it for a rollback, as deploy.yml does under compose.
#
# The units come from this checkout, the image from its version, and both
# are the release's own: deploying 0.4.17 runs the way 0.4.17 says.
set -euo pipefail

REPO=ghcr.io/pimlabs/recall-server
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
UNITS="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd"
BACKUPS="$HERE/backups"

die() { echo "podman-deploy: $*" >&2; exit 1; }

[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: $0 <version> [sha256:<digest>]"
VERSION=$1
DIGEST=${2:-}
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || die "'$VERSION' is not a release version like 0.4.17 (no v, no suffix)."
[[ -z $DIGEST || $DIGEST =~ ^sha256:[0-9a-f]{64}$ ]] \
  || die "'$DIGEST' is not a digest like sha256:<64 hex digits>."

# An SSH login has XDG_RUNTIME_DIR; su, sudo -u and cron do not, and
# systemctl --user cannot find the user's manager without it.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
me=$(id -un)

# ---------------------------------------------------------------- checks

command -v podman >/dev/null || die "podman is not installed."
have=$(podman version --format '{{.Client.Version}}')
[ "$(printf '%s\n' 5.0.0 "$have" | sort -V | head -n 1)" = 5.0.0 ] \
  || die "Podman is $have; the units need 5.0 or newer (drop-in directories)."
systemctl --user show-environment >/dev/null 2>&1 \
  || die "cannot reach $me's systemd user manager. Log in as $me over SSH rather than with su or sudo."
[ "$(loginctl show-user "$me" --property=Linger --value 2>/dev/null)" = yes ] \
  || die "linger is off for $me, so the server would stop when $me logs out. Turn it on once: sudo loginctl enable-linger $me"
podman secret exists recall-token \
  || die "there is no Podman secret recall-token, which the server reads its RECALL_TOKEN from. Create it first: see \"Running it under Podman\" in deploy/README.md."
mkdir -p "$BACKUPS"
[ -w "$BACKUPS" ] \
  || die "$BACKUPS is not writable by $me. The server writes its snapshots there as $me; give it to $me: sudo chown -R $me: $BACKUPS"

# Quadlet itself, run on the units this deploy would install, with this
# host's own drop-ins beside them, before any of them replaces what runs.
# A unit it cannot convert would otherwise vanish from systemd on the
# reload below, and the running server with it.
generator=
for g in /usr/libexec/podman/quadlet /usr/lib/podman/quadlet; do
  if [ -x "$g" ]; then generator=$g; break; fi
done
[ -n "$generator" ] || die "cannot find Podman's quadlet generator, in /usr/libexec/podman or /usr/lib/podman."

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$HERE"/quadlet/*.container "$HERE"/quadlet/*.volume "$stage"/
for d in "$UNITS"/recall-*.d; do
  if [ -d "$d" ]; then cp -R "$d" "$stage"/; fi
done
mkdir -p "$stage/recall-server.container.d"
cat > "$stage/recall-server.container.d/50-deploy.conf" <<EOF
# Written by deploy/podman-deploy.sh on every deploy; an edit here is lost
# at the next one. This host's own settings go in local.conf beside it.
[Container]
Image=$REPO:$VERSION
Volume=$BACKUPS:/backups
EOF
if ! converted=$(QUADLET_UNIT_DIRS="$stage" "$generator" -user -dryrun 2>&1); then
  printf '%s\n' "$converted" >&2
  die "Quadlet cannot convert the units at $VERSION (its error is above), so nothing was changed."
fi

# ---------------------------------------------------------------- image

# By digest, the bytes deploy.yml verified, whatever the tag names by now.
# By hand, the tag, and the image's own label has to agree with it.
if [ -n "$DIGEST" ]; then
  ref="$REPO@$DIGEST"
else
  ref="$REPO:$VERSION"
  echo "No digest given: pulling $ref and trusting the tag."
fi
podman pull "$ref" || die "could not pull $ref, so nothing was changed."
label=$(podman image inspect --format '{{index .Labels "org.opencontainers.image.version"}}' "$ref")
[ "$label" = "$VERSION" ] \
  || die "$ref says it is version '$label', not $VERSION, so nothing was changed."
podman tag "$ref" "$REPO:$VERSION"

# ---------------------------------------------------------------- units

mkdir -p "$UNITS/recall-server.container.d"
install -m 0644 "$stage"/*.container "$stage"/*.volume "$UNITS"/
install -m 0644 "$stage/recall-server.container.d/50-deploy.conf" "$UNITS/recall-server.container.d/"
systemctl --user daemon-reload

healthy=
if systemctl --user restart recall-server.service; then
  for _ in $(seq 1 60); do
    if health=$(podman exec recall-server wget -qO- http://127.0.0.1:8787/health 2>/dev/null); then
      healthy=1
      break
    fi
    sleep 2
  done
fi
if [ -z "$healthy" ]; then
  systemctl --user status recall-server.service --no-pager >&2 || true
  journalctl --user -u recall-server.service -n 40 --no-pager >&2 || true
  die "$VERSION did not start, or did not answer /health within two minutes (its log is above). Roll back by deploying the version before it."
fi
running=$(podman inspect --format '{{.ImageName}}' recall-server)
[ "$running" = "$REPO:$VERSION" ] \
  || die "recall-server answers, but runs $running rather than $REPO:$VERSION. A drop-in in $UNITS/recall-server.container.d/ that sets Image= would do that."
echo "recall-server $VERSION is up: $health"

# ---------------------------------------------------------------- tidy

# Every deploy leaves the version it replaced tagged, so nothing reclaims
# it by itself. Keep the version just deployed, the newest one older than
# it (the rollback target, which a rollback then finds without pulling),
# and anything newer, which only a rollback leaves. Remove older version
# tags. Nothing is forced: an image a container still uses is kept.
older() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n 1)" = "$1" ]
}
tags=$(podman image ls --format '{{.Tag}}' "$REPO" | grep -Ex '[0-9]+\.[0-9]+\.[0-9]+' | sort -u -V || true)
previous=
for tag in $tags; do
  if older "$tag" "$VERSION"; then previous=$tag; fi
done
kept=
for tag in $tags; do
  if ! older "$tag" "$VERSION" || [ "$tag" = "$previous" ]; then
    kept="$kept $tag"
  elif podman image rm "$REPO:$tag" >/dev/null; then
    echo "Removed $REPO:$tag"
  else
    kept="$kept $tag"
    echo "Kept $REPO:$tag: podman would not remove it (its reason is above)."
  fi
done
podman image prune --force >/dev/null
echo "$REPO keeps the versions:${kept:- none}"
