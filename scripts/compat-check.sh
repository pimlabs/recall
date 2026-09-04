#!/usr/bin/env bash
#
# The checks that have to pass before the production server is cut over from
# the Node implementation to this one, and after.
#
#   cargo build --release
#   ./scripts/compat-check.sh target/release/recall
#
# The Node server itself is gone from the tree; what remains of it is
# `tests/fixtures/node-written.db`, a database it actually wrote, captured
# before it was deleted. That is the part that matters: production's database
# was written by Node, and this server has to open it, serve every row
# faithfully, and then keep writing to it.
#
# This script has found two bugs no unit test in the repo caught, both times
# because it used the real thing where the tests used a stand-in.
set -u

BIN="${1:-target/release/recall}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$REPO_ROOT/tests/fixtures/node-written.db"
TOKEN="compat-token"
PORT=8794
WORK=$(mktemp -d)
PIDS=""
pass=0
fail=0

cleanup() {
  # shellcheck disable=SC2086
  [ -n "$PIDS" ] && kill -9 $PIDS 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail + 1)); }

check() {
  if [ "$2" = "$3" ]; then ok "$1"; else
    bad "$1"
    printf '        got:  %s\n        want: %s\n' "$2" "$3"
  fi
}

# Compared base64-encoded, because command substitution strips trailing
# newlines — precisely the property most of this script exists to verify.
check_bytes() {
  if [ "$(printf '%s' "$2" | base64 -w0)" = "$(printf '%s' "$3" | base64 -w0)" ]; then
    ok "$1"
  else
    bad "$1"
    printf '        got:  %s\n        want: %s\n' \
      "$(printf '%s' "$2" | od -c | head -2)" "$(printf '%s' "$3" | od -c | head -2)"
  fi
}

[ -x "$BIN" ] || { echo "no binary at $BIN — build it first (cargo build -p recall-sync)"; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
[ -f "$FIXTURE" ] || { echo "missing $FIXTURE"; exit 1; }

start_rust() {
  RECALL_TOKEN="$TOKEN" RECALL_PORT="$PORT" RECALL_DB_PATH="$1" RECALL_MERGE_ENABLED=false \
    "$BIN" serve >"$WORK/rust.log" 2>&1 &
  PIDS="$PIDS $!"
  for _ in $(seq 1 50); do
    curl -sf "http://localhost:$PORT/health" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  echo "server never came up"; cat "$WORK"/*.log; exit 1
}

push() {
  curl -sS -X POST "http://localhost:$PORT/sync" \
    -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$1"
}

pull() {
  curl -sS -H "Authorization: Bearer $TOKEN" "http://localhost:$PORT/sync?project_key=$1"
}

# A copy, so a run never mutates the committed fixture.
DB="$WORK/node-written.db"
cp "$FIXTURE" "$DB"
start_rust "$DB"

echo
echo "1. This server serves a database the Node server wrote"
body="$(pull "acme/app")"
# jq -j, not -r: raw mode appends a newline of its own, which would mask a
# missing one in the value being checked.
field() { jq -j ".files[]|select(.file_path==\"$1\")|.$2" <<<"$body"; }

check_bytes "content survives byte for byte" \
  "$(field 'MEMORY.md' content | base64 -w0)" "$(printf 'written by NODE\n' | base64 -w0)"
check_bytes "a file with no trailing newline keeps none" \
  "$(field 'no-trailing-newline.md' content | base64 -w0)" \
  "$(printf 'no newline at the end' | base64 -w0)"
check_bytes "a file with two trailing newlines keeps both" \
  "$(field 'two-trailing-newlines.md' content | base64 -w0)" "$(printf 'two\n\n' | base64 -w0)"
check_bytes "an empty file stays empty, not null" \
  "$(field 'empty.md' content | base64 -w0)" "$(printf '' | base64 -w0)"
check_bytes "unicode survives" \
  "$(field 'unicode.md' content | base64 -w0)" "$(printf 'café — 🚀 中文\n' | base64 -w0)"
check "a nested path survives" \
  "$(jq -r '.files[]|select(.file_path=="topics/nested/deep.md")|.source_env' <<<"$body")" "node-era"
check "source_env survives" "$(field 'MEMORY.md' source_env)" "node-era"
check "an updated_at written by Node is served unchanged" \
  "$(field 'MEMORY.md' updated_at | grep -cE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z$')" "1"
check "tombstone survives" \
  "$(jq -r '.files[]|select(.file_path=="gone.md")|.deleted' <<<"$body")" "true"
check "tombstoned content is still withheld" \
  "$(jq -r '.files[]|select(.file_path=="gone.md")|.content' <<<"$body")" "null"
check "another project is not mixed in" \
  "$(jq -r '[.files[]|select(.file_path=="MEMORY.md")]|length' <<<"$body")" "1"
check "the other project is intact" \
  "$(jq -r '.files[0].content' <<<"$(pull 'other/repo')")" "a different project"

echo
echo "2. And keeps writing to it"
push '{"project_key":"acme/app","file_path":"MEMORY.md","content":"written by RUST\n","source_env":"rust-era"}' >/dev/null
push '{"project_key":"acme/app","file_path":"added.md","content":"new row\n","source_env":"rust-era"}' >/dev/null
body="$(pull "acme/app")"
check_bytes "an existing Node-era row can be updated" \
  "$(field 'MEMORY.md' content | base64 -w0)" "$(printf 'written by RUST\n' | base64 -w0)"
check "a new row can be added alongside" "$(field 'added.md' source_env)" "rust-era"
# Seven Node-era rows for this project, plus the one just added. The eighth
# fixture row belongs to other/repo and must not appear here.
check "the untouched Node-era rows are still there" \
  "$(jq -r '[.files[]]|length' <<<"$body")" "8"

echo
echo "3. Byte-exact round trip through the binary"
MEM="$WORK/memory"
mkdir -p "$MEM"
for case in "none:no trailing newline" "one:one\n" "two:two\n\n" "empty:"; do
  name="${case%%:*}"
  content="${case#*:}"
  printf "$content" >"$MEM/round.md"
  before="$(base64 -w0 <"$MEM/round.md")"
  push "$(jq -n --arg k round --arg p round.md --rawfile c "$MEM/round.md" \
    '{project_key:$k,file_path:$p,content:$c,source_env:"round"}')" >/dev/null
  jq -j '.files[0].content' <<<"$(pull round)" >"$MEM/round.md"
  check "trailing newlines: $name" "$(base64 -w0 <"$MEM/round.md")" "$before"
done

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
