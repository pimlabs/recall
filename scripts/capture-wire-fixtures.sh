#!/usr/bin/env bash
#
# Captures what a released server puts on the wire, as golden fixtures for
# crates/recall-wire/tests/golden.rs.
#
#   ./scripts/capture-wire-fixtures.sh 0.4.0
#
# Downloads that version's release archive for this machine (the server's
# own, from 0.4.0), runs its server on a scratch database, and saves each
# response into crates/recall-wire/fixtures/wire/<version>/. A file that already exists is
# left alone: a shipped fixture is never rewritten, see the README there.
# Request fixtures are not captured here; they are written from the
# client's PushRequest at that tag.
set -u

VERSION="${1:?usage: capture-wire-fixtures.sh <version>}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO_ROOT/crates/recall-wire/fixtures/wire/$VERSION"
PORT=8932
URL="http://127.0.0.1:$PORT"
AUTH="Authorization: Bearer fixture-token"
WORK=$(mktemp -d)
SERVER=""

cleanup() {
  [ -n "$SERVER" ] && kill "$SERVER" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

# Before 0.4.0 the server was `recall serve`, inside the client's archive,
# built for Linux and macOS. From 0.4.0 it is `recall-server`, in its own
# archive, built for Linux only: that is where servers run.
if [ "$(printf '%s\n' "$VERSION" 0.4.0 | sort -V | head -1)" = 0.4.0 ]; then
  NAME=recall-server
  SERVE=""
else
  NAME=recall
  SERVE=serve
fi
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ASSET=${NAME}_linux_amd64 ;;
  Linux-aarch64 | Linux-arm64) ASSET=${NAME}_linux_arm64 ;;
  Darwin-x86_64) ASSET=${NAME}_darwin_amd64 ;;
  Darwin-arm64) ASSET=${NAME}_darwin_arm64 ;;
  *) echo "no release archive for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

curl -sSfL -o "$WORK/a.tar.gz" \
  "https://github.com/pimlabs/recall/releases/download/v$VERSION/$ASSET.tar.gz" || exit 1
tar -xzf "$WORK/a.tar.gz" -C "$WORK"
BIN="$WORK/$ASSET"
"$BIN" version

# $SERVE is deliberately unquoted: empty for recall-server, which takes no
# subcommand.
# shellcheck disable=SC2086
RECALL_TOKEN=fixture-token RECALL_PORT="$PORT" RECALL_DB_PATH="$WORK/db.sqlite" \
  RECALL_MERGE_ENABLED=false RECALL_GIT_COMMIT=abc1234 \
  "$BIN" $SERVE >"$WORK/server.log" 2>&1 &
SERVER=$!
for _ in $(seq 1 40); do
  curl -sf "$URL/health" >/dev/null 2>&1 && break
  sleep 0.25
done

mkdir -p "$OUT"

# save <file> <curl args...>: keeps the body of a 2xx or 4xx answer, and
# never replaces a fixture that is already there.
save() {
  local file="$OUT/$1"
  shift
  if [ -e "$file" ]; then
    echo "kept     $file"
    return
  fi
  local code
  code=$(curl -s -o "$WORK/body" -w '%{http_code}' "$@")
  case "$code" in
    2?? | 4??) mv "$WORK/body" "$file"; echo "captured $file ($code)" ;;
    *) echo "skipped  $file (HTTP $code)" ;;
  esac
}

push() {
  curl -s -o /dev/null -X POST -H "$AUTH" -H 'Content-Type: application/json' -d "$1" "$URL/sync"
}

save push_response.json -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"note.md","content":"# Note\n- a fact\n","source_env":"laptop"}' \
  "$URL/sync"
push '{"project_key":"acme/app","file_path":"gone.md","content":"x","source_env":"laptop"}'
save push_response_delete.json -X POST -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"gone.md","deleted":true,"source_env":"laptop"}' \
  "$URL/sync"
save sync_response.json -H "$AUTH" "$URL/sync?project_key=acme/app"
save health.json "$URL/health"
save admin_stats.json -H "$AUTH" "$URL/admin/stats"
save error.json "$URL/sync?project_key=acme/app"
# A server older than the discovery document answers 404, which is not a
# shape worth keeping.
if [ ! -e "$OUT/discovery.json" ]; then
  code=$(curl -s -o "$WORK/body" -w '%{http_code}' "$URL/.well-known/recall")
  if [ "$code" = 200 ]; then
    mv "$WORK/body" "$OUT/discovery.json"
    echo "captured $OUT/discovery.json (200)"
  else
    echo "skipped  $OUT/discovery.json (HTTP $code)"
  fi
fi
