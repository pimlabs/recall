#!/usr/bin/env bash
#
# Does RECALL_TRUSTED_IP_HEADER actually decide the rate-limit bucket, and are
# the headers it does not name actually ignored?
#
#   cargo build --release
#   ./scripts/trusted-ip-check.sh target/release/recall
#
# This is a security property, not a nicety. Rate limiting runs before auth so
# that a flood of invalid tokens is limited too — so a client that can pick its
# own bucket gets unlimited attempts at guessing the token. The unit tests
# cover client_ip in isolation; this drives the whole middleware stack on a
# real socket, which is where a future refactor would actually break it.
set -u
BIN="$1"
TOKEN="hdr-token"
WORK=$(mktemp -d)
PIDS=""
pass=0; fail=0
cleanup() { [ -n "$PIDS" ] && kill -9 $PIDS 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
chk() { [ "$2" = "$3" ] && ok "$1" || { bad "$1"; printf '        got %s want %s\n' "$2" "$3"; }; }

start() { # port, trusted header
  RECALL_TOKEN="$TOKEN" RECALL_PORT="$1" RECALL_DB_PATH="$WORK/$1.db" \
    RECALL_MERGE_ENABLED=false RECALL_RATE_LIMIT_MAX=3 RECALL_RATE_LIMIT_WINDOW_MS=60000 \
    RECALL_TRUSTED_IP_HEADER="$2" "$BIN" serve >"$WORK/$1.log" 2>&1 &
  PIDS="$PIDS $!"
  for _ in $(seq 1 50); do curl -sf "http://127.0.0.1:$1/health" >/dev/null 2>&1 && return; sleep 0.2; done
  echo "server on :$1 never came up"; cat "$WORK/$1.log"; exit 1
}

code() { # port, header args...
  local port=$1; shift
  curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$@" \
    "http://127.0.0.1:$port/sync?project_key=a/b"
}

echo
echo "Traefik shape: RECALL_TRUSTED_IP_HEADER=x-real-ip, limit 3/60s"
start 8961 x-real-ip
# Burn the bucket for one x-real-ip.
for _ in 1 2 3; do code 8961 -H "X-Real-IP: 10.0.0.1" >/dev/null; done
chk "a fourth request from the same x-real-ip is limited" \
  "$(code 8961 -H 'X-Real-IP: 10.0.0.1')" "429"
chk "a different x-real-ip has its own bucket" \
  "$(code 8961 -H 'X-Real-IP: 10.0.0.2')" "200"

# The attack: the bucket is spent for 10.0.0.1; try to escape it by sending
# headers the ingress does not set.
chk "rotating cf-connecting-ip does not escape the bucket" \
  "$(code 8961 -H 'X-Real-IP: 10.0.0.1' -H 'CF-Connecting-IP: 9.9.9.9')" "429"
chk "rotating x-forwarded-for does not escape the bucket" \
  "$(code 8961 -H 'X-Real-IP: 10.0.0.1' -H 'X-Forwarded-For: 8.8.8.8, 10.0.0.1')" "429"
chk "rotating true-client-ip does not escape the bucket" \
  "$(code 8961 -H 'X-Real-IP: 10.0.0.1' -H 'True-Client-IP: 7.7.7.7')" "429"

echo
echo "Cloudflare shape: RECALL_TRUSTED_IP_HEADER=cf-connecting-ip (the default)"
start 8962 cf-connecting-ip
for _ in 1 2 3; do code 8962 -H "CF-Connecting-IP: 10.0.0.1" >/dev/null; done
chk "a fourth request from the same cf-connecting-ip is limited" \
  "$(code 8962 -H 'CF-Connecting-IP: 10.0.0.1')" "429"
chk "a spoofed x-real-ip does not escape the bucket" \
  "$(code 8962 -H 'CF-Connecting-IP: 10.0.0.1' -H 'X-Real-IP: 9.9.9.9')" "429"
chk "a spoofed x-forwarded-for does not escape the bucket" \
  "$(code 8962 -H 'CF-Connecting-IP: 10.0.0.1' -H 'X-Forwarded-For: 9.9.9.9')" "429"

echo
echo "No ingress: RECALL_TRUSTED_IP_HEADER empty, every client is the socket"
start 8963 ""
for _ in 1 2 3; do code 8963 -H "X-Real-IP: 10.0.0.1" >/dev/null; done
chk "headers cannot buy a fresh bucket at all" \
  "$(code 8963 -H 'X-Real-IP: 10.0.0.99' -H 'CF-Connecting-IP: 8.8.8.8')" "429"

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
