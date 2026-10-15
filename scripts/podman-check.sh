#!/usr/bin/env bash
#
# Runs deploy/podman-deploy.sh and the Quadlet units in deploy/quadlet/ for
# real, under rootless Podman and the user's own systemd, then runs
# deploy/README.md's Podman blocks as the README has them, and checks what
# each leaves behind.
#
#   scripts/podman-check.sh
#
# Made for CI's ubuntu-26.04 runner, which has the Podman a deployment runs
# (5.7). It turns on linger, deploys the three newest published releases in
# a row from GHCR, creates and removes Podman secrets, volumes and units,
# and uses Docker for the move from Compose, so it refuses to start where a
# recall-server unit, a recall-data volume or a recall-token secret already
# exists. Needs curl, jq, openssl, docker with buildx, and gh with GH_TOKEN:
# the newer two releases are deployed the way deploy.yml deploys them, by
# the digest scripts/verify-release-image.sh verified, and the oldest the way
# the README's update by hand does, by its tag.
#
# What it guards is a running server and the only copy of its database. A
# unit that publishes on 0.0.0.0, a secret that reaches the container with a
# newline on the end, a restore that leaves the old WAL beside the new file,
# a move from Compose that starts on an empty volume: each reads fine in a
# review, and each makes this fail.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
README="$ROOT/deploy/README.md"
DEPLOY="$ROOT/deploy/podman-deploy.sh"
BACKUPS="$ROOT/deploy/backups"
UNITS="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd"
REPO=ghcr.io/pimlabs/recall-server
URL=http://127.0.0.1:18787
KEY="podman/check"
W=$(mktemp -d)
pass=0; fail=0

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
chk() { [ "$2" = "$3" ] && ok "$1" || { bad "$1"; printf '        got  %s\n        want %s\n' "$2" "$3"; }; }
section() { printf '\n%s\n' "$1"; }

finish() {
  if [ "$fail" != 0 ]; then
    section "recall-server's journal, for the failures above"
    journalctl --user -u recall-server.service -n 80 --no-pager 2>/dev/null
  fi
  rm -rf "$W"
  printf '\n%d passed, %d failed\n' "$pass" "$fail"
  [ "$fail" = 0 ]
}

if [ -e "$UNITS/recall-server.container" ] \
  || podman volume exists recall-data 2>/dev/null \
  || podman secret exists recall-token 2>/dev/null; then
  echo "This machine already has a recall-server unit, a recall-data volume or a recall-token secret. This check deploys over all three, so it runs only where nothing of Recall's does." >&2
  exit 1
fi

# ---------------------------------------------------------------- the user

me=$(id -un)
sudo loginctl enable-linger "$me"
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
for _ in $(seq 1 100); do
  systemctl --user show-environment >/dev/null 2>&1 && break
  sleep 0.1
done
systemctl --user show-environment >/dev/null || { echo "$me's systemd user manager did not start" >&2; exit 1; }
if ! grep -q "^$me:" /etc/subuid || ! grep -q "^$me:" /etc/subgid; then
  sudo usermod --add-subuids 200000-265535 --add-subgids 200000-265535 "$me"
  podman system migrate
fi

# ---------------------------------------------------------------- helpers

# The fenced sh block of README that contains $1.
block() {
  python3 - "$README" "$1" <<'EOF'
import re, sys
text, marker = open(sys.argv[1]).read(), sys.argv[2]
found = [b for b in re.findall(r'```sh\n(.*?)```', text, re.S) if marker in b]
if len(found) != 1:
    sys.exit('%d sh blocks in deploy/README.md contain %r' % (len(found), marker))
sys.stdout.write(found[0])
EOF
}
# Pastes a block into a fresh bash at the checkout's root, as an owner
# would, and prints what it printed.
paste_block() { (cd "$ROOT" && bash -c "$1") 2>&1; }

status() { curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${2:-$TOKEN}" "${1:-$URL}/sync?project_key=$KEY"; }
push() {
  curl -s -o /dev/null -w '%{http_code}' -X POST "${3:-$URL}/sync" \
    -H "Authorization: Bearer ${4:-$TOKEN}" -H 'Content-Type: application/json' \
    -d "{\"project_key\":\"$KEY\",\"file_path\":\"$1\",\"content\":\"$2\",\"source_env\":\"podman-check\"}"
}
# What the server holds for one file, or nothing.
content() {
  curl -fsS -H "Authorization: Bearer ${2:-$TOKEN}" "${3:-$URL}/sync?project_key=$KEY" \
    | jq -r --arg p "$1" '.files[] | select(.file_path == $p and (.deleted | not)) | .content'
}
# Until $1 answers 2xx, for at most $2 seconds.
answers() {
  for _ in $(seq 1 "$2"); do
    curl -fsS -o /dev/null "$1" 2>/dev/null && return 0
    sleep 1
  done
  return 1
}
healthy() { answers "${1:-$URL}/health" "${2:-60}"; }
running_image() { podman inspect --format '{{.ImageName}}' recall-server 2>/dev/null; }
listening() { ss -Htln "sport = :$1" | awk '{print $4}' | sort -u | tr '\n' ' '; }
volume() { podman volume inspect recall-data --format '{{.Mountpoint}}'; }
tags() { podman image ls --format '{{.Tag}}' "$REPO" | grep -Ex '[0-9]+\.[0-9]+\.[0-9]+' | sort -u -V | tr '\n' ' '; }

# ---------------------------------------------------------------- releases

section "The three newest releases with published images"
picked=()
for v in $(git -C "$ROOT" ls-remote --tags --refs origin 'v*' \
  | sed -n 's|.*refs/tags/v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$|\1|p' | sort -Vr); do
  [ "$(printf '%s\n' "$v" 0.4.6 | sort -V | tail -n 1)" = 0.4.6 ] && break
  if "$ROOT/scripts/image-pullable.sh" "$REPO:$v" >/dev/null 2>&1; then picked+=("$v"); fi
  [ "${#picked[@]}" = 3 ] && break
done
if [ "${#picked[@]}" != 3 ]; then
  echo "Fewer than three releases have published images." >&2
  exit 1
fi
NEW=${picked[0]}; OLD=${picked[1]}; OLDEST=${picked[2]}
echo "$OLDEST, then $OLD, then $NEW"
verified() {
  GITHUB_OUTPUT="$W/verified-$1" "$ROOT/scripts/verify-release-image.sh" "$1" >&2 \
    && sed -n 's/^server_digest=//p' "$W/verified-$1"
}
OLD_DIGEST=$(verified "$OLD") && NEW_DIGEST=$(verified "$NEW") \
  || { echo "verify-release-image.sh did not verify $OLD and $NEW" >&2; exit 1; }

# ---------------------------------------------------------------- refusals

section "Refusals, before anything exists"
out=$("$DEPLOY" "$NEW" "$NEW_DIGEST" 2>&1); code=$?
[ "$code" != 0 ] && ok "with no recall-token secret the deploy refuses" || bad "the deploy ran with no recall-token secret"
grep -q 'recall-token' <<<"$out" && ok "  and names the secret" || { bad "  and names the secret"; echo "$out"; }
[ ! -e "$UNITS/recall-server.container" ] && ok "  having installed no unit" || bad "  having installed no unit"
podman image exists "$REPO:$NEW" && bad "  having pulled no image" || ok "  having pulled no image"
"$DEPLOY" "v$NEW" >/dev/null 2>&1 && bad "v$NEW is accepted as a version" || ok "a version with a v is refused"
"$DEPLOY" "$NEW" sha256:abc >/dev/null 2>&1 && bad "sha256:abc is accepted as a digest" || ok "a digest that is not 64 hex digits is refused"

section "README: the token, at first start"
out=$(paste_block "$(block 'RECALL_TOKEN for your clients')"); code=$?
chk "the block succeeds" "$code" "0"
TOKEN=$(sed -n 's/^RECALL_TOKEN for your clients: //p' <<<"$out")
[[ $TOKEN =~ ^[0-9a-f]{64}$ ]] && ok "  and prints a 64-digit token" || bad "  and prints a 64-digit token, not '$TOKEN'"

# ---------------------------------------------------------------- first deploy

section "Deploy $OLDEST by its tag, as by hand"
out=$("$DEPLOY" "$OLDEST" 2>&1); code=$?
chk "the deploy succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
grep -q 'trusting the tag' <<<"$out" && ok "  and says it trusted the tag" || bad "  and says it trusted the tag"
chk "recall-server runs $OLDEST" "$(running_image)" "$REPO:$OLDEST"
chk "/health answers through the published port" "$(curl -fsS "$URL/health" | jq -r .status)" "ok"
chk "18787 is published on loopback only" "$(listening 18787)" "127.0.0.1:18787 "
ip=$(ip -4 -o route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')
chk "  and does not answer on $ip" "$(curl -s -m 3 -o /dev/null -w '%{http_code}' "http://$ip:18787/health")" "000"
chk "the token from the secret is accepted" "$(status)" "200"
chk "  and another is not" "$(status "$URL" "x$TOKEN")" "401"
podman inspect recall-server | grep -qF "$TOKEN" && bad "podman inspect shows the token" || ok "podman inspect does not show the token"
systemctl --user cat recall-server.service | grep -qF "$TOKEN" && bad "the unit holds the token" || ok "the unit does not hold the token"
chk "a push is stored" "$(push A.md 'note A')" "200"
chk "the server runs as node" "$(podman exec recall-server stat -c %U /proc/1)" "node"
chk "recall.db belongs to $me on the host" "$(stat -c %U "$(volume)/recall.db")" "$me"
podman exec -u node recall-server touch /backups/.podman-check
chk "what node writes to /backups belongs to $me" "$(stat -c %U "$BACKUPS/.podman-check" 2>/dev/null)" "$me"
touch "$BACKUPS/.last-offbox" && ok "$me can write backup-offbox.sh's marker" || bad "$me cannot write backup-offbox.sh's marker"
chk "the container has 60 seconds to stop" "$(podman inspect --format '{{.Config.StopTimeout}}' recall-server)" "60"
podman healthcheck run recall-server >/dev/null 2>&1 && ok "its health check passes" || bad "its health check fails"
chk "the claude CLI is there for merges" "$(curl -fsS "$URL/health" | jq -r .merge.claude_cli.available)" "true"
deps=$(systemctl --user list-dependencies --plain --no-pager default.target)
grep -q 'recall-server.service' <<<"$deps" && ok "recall-server starts at boot" || bad "recall-server does not start at boot"
grep -q 'recall-sqlite-web.service' <<<"$deps" && bad "the database browser starts at boot" || ok "the database browser does not start at boot"
podman kill recall-server >/dev/null
sleep 2
healthy && ok "killed, it is started again" || bad "killed, it stays down"
chk "  with its data" "$(content A.md)" "note A"

# ---------------------------------------------------------------- upgrades

section "Deploy $OLD by its verified digest"
out=$("$DEPLOY" "$OLD" "$OLD_DIGEST" 2>&1); code=$?
chk "the deploy succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
chk "recall-server runs $OLD" "$(running_image)" "$REPO:$OLD"
chk "the data is still there" "$(content A.md)" "note A"
chk "both versions are kept" "$(tags)" "$OLDEST $OLD "

section "Deploy $NEW by its verified digest"
out=$("$DEPLOY" "$NEW" "$NEW_DIGEST" 2>&1); code=$?
chk "the deploy succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
chk "recall-server runs $NEW" "$(running_image)" "$REPO:$NEW"
chk "the data is still there" "$(content A.md)" "note A"
chk "$OLDEST is removed, $OLD kept for a rollback" "$(tags)" "$OLD $NEW "

section "This host's local.conf, and a deploy of the same version again"
mkdir -p "$UNITS/recall-server.container.d"
printf '[Container]\nEnvironment=RECALL_PUBLIC_URL=https://recall.example.test\nEnvironment=RECALL_BACKUP_KEEP=3\n' \
  > "$UNITS/recall-server.container.d/local.conf"
before=$(sha256sum < "$UNITS/recall-server.container.d/local.conf")
out=$("$DEPLOY" "$NEW" "$NEW_DIGEST" 2>&1); code=$?
chk "the deploy succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
chk "local.conf is as it was" "$(sha256sum < "$UNITS/recall-server.container.d/local.conf")" "$before"
chk "its RECALL_PUBLIC_URL reaches the server" "$(podman exec recall-server printenv RECALL_PUBLIC_URL)" "https://recall.example.test"
chk "its RECALL_BACKUP_KEEP overrides the unit's" "$(podman exec recall-server printenv RECALL_BACKUP_KEEP)" "3"
chk "$OLD is still kept" "$(tags)" "$OLD $NEW "

section "A unit Quadlet cannot convert"
cp -R "$ROOT/deploy" "$W/broken"
sed -i 's/^\[Container\]$/[Container]\nNoSuchKey=1/' "$W/broken/quadlet/recall-server.container"
out=$("$W/broken/podman-deploy.sh" "$NEW" "$NEW_DIGEST" 2>&1); code=$?
[ "$code" != 0 ] && ok "the deploy refuses it" || bad "the deploy installed a unit Quadlet cannot convert"
grep -q 'cannot convert' <<<"$out" && ok "  and says why" || { bad "  and says why"; echo "$out"; }
chk "  leaving the installed unit as it was" "$(sha256sum < "$UNITS/recall-server.container")" "$(sha256sum < "$ROOT/deploy/quadlet/recall-server.container")"
chk "  and the server running" "$(systemctl --user is-active recall-server.service)" "active"

section "A file in deploy/backups whose group the user namespace does not map"
cp -R "$ROOT/deploy" "$W/foreign"
touch "$W/foreign/backups/recall-foreign.db"
sudo chgrp root "$W/foreign/backups/recall-foreign.db"
conf="$UNITS/recall-server.container.d/50-deploy.conf"
before=$(sha256sum < "$conf")
started=$(podman inspect --format '{{.State.StartedAt}}' recall-server)
out=$("$W/foreign/podman-deploy.sh" "$NEW" "$NEW_DIGEST" 2>&1); code=$?
[ "$code" != 0 ] && ok "the deploy refuses it" || bad "the deploy ran with a file of group root in deploy/backups"
grep -qF "$W/foreign/backups/recall-foreign.db" <<<"$out" && ok "  and names the file" || { bad "  and names the file"; echo "$out"; }
grep -qF "sudo chown -R $me:$(id -gn) $W/foreign/backups" <<<"$out" && ok "  and gives the chown that fixes it" || bad "  and gives the chown that fixes it"
chk "  leaving the installed drop-in as it was" "$(sha256sum < "$conf")" "$before"
chk "  and the server running, never restarted" "$(podman inspect --format '{{.State.StartedAt}}' recall-server)" "$started"

# ---------------------------------------------------------------- browser

section "The database browser"
systemctl --user start recall-sqlite-web.service
answers http://127.0.0.1:8081/-/versions.json 120 && ok "it starts" || bad "it does not start"
tables=$(curl -fsS http://127.0.0.1:8081/recall.json | jq -r '.tables[].name' 2>/dev/null)
grep -qx memory_files <<<"$tables" && ok "it reads recall.db" || { bad "it reads recall.db"; journalctl --user -u recall-sqlite-web.service -n 30 --no-pager; }
chk "8081 is published on loopback only" "$(listening 8081)" "127.0.0.1:8081 "
curl -s -o /dev/null "http://127.0.0.1:8081/recall.json?sql=delete+from+memory_files"
chk "a write through it changes nothing" "$(content A.md)" "note A"
systemctl --user stop recall-sqlite-web.service
chk "stopped, it is gone" "$(listening 8081)" ""

# ---------------------------------------------------------------- README

section "README: rotating the token"
out=$(paste_block "$(block 'podman secret create --replace recall-token')"); code=$?
chk "the block succeeds" "$code" "0"
NEW_TOKEN=$(sed -n 's/^New RECALL_TOKEN: //p' <<<"$out")
healthy && ok "the server is back" || bad "the server is not back"
chk "the new token is accepted" "$(status "$URL" "$NEW_TOKEN")" "200"
chk "  and the old one is not" "$(status)" "401"
TOKEN=$NEW_TOKEN

section "README: restoring a snapshot"
SNAP=recall-podman-check.db
podman run --rm -i -v recall-data:/data -v "$BACKUPS":/backups docker.io/library/alpine:3 \
  sh -c 'apk add --no-cache sqlite >/dev/null && sqlite3 /data/recall.db' <<EOF
VACUUM INTO '/backups/$SNAP';
EOF
[ -f "$BACKUPS/$SNAP" ] && ok "a snapshot is taken with A in it" || bad "no snapshot was taken"
chk "B is pushed after it" "$(push B.md 'note B')" "200"
RESTORE=$(block 'podman run --rm -e SNAP')
aside_before=$(find "$(volume)" -maxdepth 1 -name 'replaced-*' | wc -l)
out=$(paste_block "${RESTORE//recall-<timestamp>.db/recall-2000-01-01T00-00-00-000Z.db}"); code=$?
[ "$code" != 0 ] && ok "a snapshot that is not there stops the block" || bad "the block succeeded with no snapshot"
chk "  with the server still running" "$(systemctl --user is-active recall-server.service)" "active"
chk "  and nothing moved aside" "$(find "$(volume)" -maxdepth 1 -name 'replaced-*' | wc -l)" "$aside_before"
out=$(paste_block "${RESTORE//recall-<timestamp>.db/$SNAP}"); code=$?
chk "the block succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
healthy && ok "the server is back" || bad "the server is not back"
chk "A is there" "$(content A.md)" "note A"
chk "B, newer than the snapshot, is not" "$(content B.md)" ""
aside=$(find "$(volume)" -maxdepth 1 -name 'replaced-*' | head -n 1)
[ -f "$aside/recall.db" ] && ok "the replaced database is kept aside" || bad "the replaced database is not kept aside"
chk "the restored file belongs to $me" "$(stat -c %U "$(volume)/recall.db")" "$me"

section "README: moving a Compose deployment to Podman"
systemctl --user stop recall-server.service
podman secret rm recall-token >/dev/null
podman volume rm recall-data >/dev/null
DTOKEN=$(openssl rand -hex 32)
docker volume create recall_recall-data >/dev/null
docker run -d --name recall-server --stop-timeout 60 -e RECALL_TOKEN="$DTOKEN" \
  -v recall_recall-data:/data -p 127.0.0.1:18799:8787 "$REPO:$NEW" >/dev/null
healthy http://127.0.0.1:18799 && ok "a server under Docker runs" || bad "the server under Docker did not start"
chk "  and stores M" "$(push M.md 'note M' http://127.0.0.1:18799 "$DTOKEN")" "200"
docker exec -u node recall-server sh -c 'mkdir -p /data/claude-config && echo "{}" > /data/claude-config/.credentials.json'
out=$(paste_block "$(block 'docker inspect --format')"); code=$?
chk "the token block succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
out=$(paste_block "$(block 'podman volume import recall-data')"); code=$?
chk "the volume block succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
chk "  having stopped the server under Docker" "$(docker inspect --format '{{.State.Running}}' recall-server)" "false"
out=$(paste_block "$(block 'podman volume import recall-data')"); code=$?
[ "$code" != 0 ] && ok "run again, it refuses the volume it made" || bad "run again, it copied over the volume it made"
out=$("$DEPLOY" "$NEW" "$NEW_DIGEST" 2>&1); code=$?
chk "the deploy succeeds" "$code" "0"
[ "$code" != 0 ] && echo "$out"
chk "M moved, under Docker's token" "$(content M.md "$DTOKEN")" "note M"
[ -f "$(volume)/claude-config/.credentials.json" ] && ok "the claude CLI's login moved" || bad "the claude CLI's login did not move"
chk "the moved files belong to $me" "$(stat -c %U "$(volume)/recall.db") $(stat -c %U "$(volume)/claude-config/.credentials.json")" "$me $me"
docker volume inspect recall_recall-data >/dev/null 2>&1 && ok "Docker's volume is still there" || bad "Docker's volume is gone"

finish
