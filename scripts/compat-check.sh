#!/usr/bin/env bash
# Proves the Rust binary is a drop-in for the Node server currently in
# production, by exercising every combination a mixed fleet actually hits
# during a migration:
#
#   1. The Rust server opens a database WRITTEN BY the Node server.
#   2. The old shell hooks push to and pull from the Rust server.
#   3. The Rust client pushes to and pulls from the Node server.
#   4. Content survives byte-for-byte in both directions.
#
# Run it before cutting production over, and again after. It needs `node`,
# `jq`, `curl` and a release or debug build of the binary.
#
#   ./scripts/compat-check.sh [path-to-recall-binary]
set -euo pipefail

BIN="${1:-target/debug/recall}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
TOKEN="compat-check-token"
NODE_PORT=8891
RUST_PORT=8892

pass=0
fail=0

cleanup() {
  for pid in ${PIDS:-}; do kill -9 "$pid" 2>/dev/null || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT
PIDS=""

ok()   { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail + 1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1"; printf '        got:  %q\n        want: %q\n' "$2" "$3"; fi; }

[ -x "$BIN" ] || { echo "no binary at $BIN — build it first (cargo build -p recall-cli)"; exit 1; }
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

start_node() {
  RECALL_TOKEN="$TOKEN" RECALL_PORT="$NODE_PORT" RECALL_DB_PATH="$1" RECALL_MERGE_ENABLED=false \
    node "$REPO_ROOT/server/index.js" >"$WORK/node.log" 2>&1 &
  PIDS="$PIDS $!"
  wait_for "$NODE_PORT"
}

start_rust() {
  RECALL_TOKEN="$TOKEN" RECALL_PORT="$RUST_PORT" RECALL_DB_PATH="$1" RECALL_MERGE_ENABLED=false \
    "$BIN" serve >"$WORK/rust.log" 2>&1 &
  PIDS="$PIDS $!"
  wait_for "$RUST_PORT"
}

wait_for() {
  for _ in $(seq 1 50); do
    curl -sf "http://localhost:$1/health" >/dev/null 2>&1 && return 0
    sleep 0.2
  done
  echo "server on :$1 never came up"; cat "$WORK"/*.log; exit 1
}

push_to() { # port, json
  curl -sS -X POST "http://localhost:$1/sync" \
    -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$2"
}

pull_from() { # port, project_key
  curl -sS -H "Authorization: Bearer $TOKEN" "http://localhost:$1/sync?project_key=$2"
}

echo
echo "1. Rust server reads a database written by the Node server"
SHARED="$WORK/shared.db"
start_node "$SHARED"
push_to "$NODE_PORT" '{"project_key":"acme/app","file_path":"MEMORY.md","content":"written by NODE\n","source_env":"node-era"}' >/dev/null
push_to "$NODE_PORT" '{"project_key":"acme/app","file_path":"gone.md","content":"secret\n","source_env":"node-era"}' >/dev/null
push_to "$NODE_PORT" '{"project_key":"acme/app","file_path":"gone.md","deleted":true,"source_env":"node-era"}' >/dev/null
kill -9 ${PIDS# } 2>/dev/null || true; PIDS=""; sleep 1

start_rust "$SHARED"
body="$(pull_from "$RUST_PORT" "acme/app")"
check "content survives the handover" "$(jq -r '.files[]|select(.file_path=="MEMORY.md")|.content' <<<"$body")" "written by NODE
"
check "source_env survives" "$(jq -r '.files[]|select(.file_path=="MEMORY.md")|.source_env' <<<"$body")" "node-era"
check "tombstone survives" "$(jq -r '.files[]|select(.file_path=="gone.md")|.deleted' <<<"$body")" "true"
check "tombstoned content still withheld" "$(jq -r '.files[]|select(.file_path=="gone.md")|.content' <<<"$body")" "null"

echo
echo "2. Old shell hooks against the Rust server"
PROJ="$WORK/bash-machine"
mkdir -p "$PROJ"
(cd "$PROJ" && git init -q && git remote add origin git@github.com:acme/app.git)
export CLAUDE_CONFIG_DIR="$PROJ/.claude-home"
SLUG="$(printf '%s' "$PROJ" | sed 's/[^a-zA-Z0-9]/-/g')"
MEM="$CLAUDE_CONFIG_DIR/projects/$SLUG/memory"
mkdir -p "$MEM"

(cd "$PROJ" && RECALL_URL="http://localhost:$RUST_PORT" RECALL_TOKEN="$TOKEN" \
  "$REPO_ROOT/hooks/recall-pull" 2>/dev/null)
check "shell pull got the file" "$(cat "$MEM/MEMORY.md" 2>/dev/null)" "written by NODE"

printf '# Memory\n- from the OLD shell client\n' > "$MEM/MEMORY.md"
(cd "$PROJ" && RECALL_URL="http://localhost:$RUST_PORT" RECALL_TOKEN="$TOKEN" RECALL_SOURCE_ENV="bash-client" \
  sh -c "jq -n --arg fp '$MEM/MEMORY.md' '{tool_input:{file_path:\$fp}}' | '$REPO_ROOT/hooks/recall-push'" 2>/dev/null)
check "shell push reached the Rust server" \
  "$(pull_from "$RUST_PORT" "acme/app" | jq -r '.files[]|select(.file_path=="MEMORY.md")|.source_env')" \
  "bash-client"

echo
echo "3. Rust client against the Node server"
NODE_ONLY="$WORK/node-only.db"
start_node "$NODE_ONLY"
PROJ2="$WORK/rust-machine"
mkdir -p "$PROJ2"
(cd "$PROJ2" && git init -q && git remote add origin "http://local_proxy@127.0.0.1:9999/git/acme/app")
SLUG2="$(printf '%s' "$PROJ2" | sed 's/[^a-zA-Z0-9]/-/g')"
MEM2="$PROJ2/.claude-home/projects/$SLUG2/memory"
mkdir -p "$MEM2"
printf '# Memory\n- from the RUST client' > "$MEM2/MEMORY.md"   # deliberately no trailing newline
(cd "$PROJ2" && CLAUDE_CONFIG_DIR="$PROJ2/.claude-home" RECALL_URL="http://localhost:$NODE_PORT" \
  RECALL_TOKEN="$TOKEN" RECALL_SOURCE_ENV="rust-client" \
  sh -c "jq -n --arg fp '$MEM2/MEMORY.md' '{tool_input:{file_path:\$fp}}' | '$BIN' push" 2>/dev/null)
check "Rust push reached the Node server" \
  "$(pull_from "$NODE_PORT" "acme/app" | jq -r '.files[]|select(.file_path=="MEMORY.md")|.source_env')" \
  "rust-client"

echo
echo "4. Byte-exact round trip through the Rust binary"
for name in none one two empty; do
  case "$name" in
    none)  printf '# Memory\n- no trailing newline' > "$MEM2/round.md" ;;
    one)   printf '# Memory\n- one\n'               > "$MEM2/round.md" ;;
    two)   printf '# Memory\n- two\n\n'             > "$MEM2/round.md" ;;
    empty) : > "$MEM2/round.md" ;;
  esac
  before="$(cksum < "$MEM2/round.md")"
  (cd "$PROJ2" && CLAUDE_CONFIG_DIR="$PROJ2/.claude-home" RECALL_URL="http://localhost:$NODE_PORT" \
    RECALL_TOKEN="$TOKEN" \
    sh -c "jq -n --arg fp '$MEM2/round.md' '{tool_input:{file_path:\$fp}}' | '$BIN' push" 2>/dev/null)
  rm -f "$MEM2/round.md"
  (cd "$PROJ2" && CLAUDE_CONFIG_DIR="$PROJ2/.claude-home" RECALL_URL="http://localhost:$NODE_PORT" \
    RECALL_TOKEN="$TOKEN" "$BIN" pull 2>/dev/null)
  check "trailing newlines: $name" "$(cksum < "$MEM2/round.md")" "$before"
done

echo
printf 'passed %d, failed %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
