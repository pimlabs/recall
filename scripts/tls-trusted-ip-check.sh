#!/usr/bin/env bash
#
# The direct-TLS sibling of trusted-ip-check.sh. With TLS on, config.rs
# forces RECALL_TRUSTED_IP_HEADER empty (or refuses to start if it was set
# explicitly): there is no ingress in front any more, so the rate limiter
# has to key on the real TCP peer address instead. This proves that on a
# real socket, over a real TLS handshake, rather than trusting the unit
# tests to have covered what axum-server actually hands the middleware.
#
#   cargo build --release
#   ./scripts/tls-trusted-ip-check.sh target/release/recall-server
#
# The certificate is a self-signed one generated right here with openssl;
# curl -k accepts it without needing a CA, and nothing about the property
# this script checks depends on the certificate being one a browser would
# trust, so ACME (which needs a real domain and network access) is not
# exercised here at all.
set -u
BIN="$1"
TOKEN="tls-hdr-token"
WORK=$(mktemp -d)
PIDS=""
pass=0; fail=0
cleanup() { [ -n "$PIDS" ] && kill -9 $PIDS 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
chk() { [ "$2" = "$3" ] && ok "$1" || { bad "$1"; printf '        got %s want %s\n' "$2" "$3"; }; }

CERT="$WORK/cert.pem"
KEY="$WORK/key.pem"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -keyout "$KEY" -out "$CERT" -subj "/CN=localhost" >/dev/null 2>&1
if [ ! -s "$CERT" ] || [ ! -s "$KEY" ]; then
  echo "openssl did not produce a certificate/key pair"; exit 1
fi

start() { # port
  RECALL_TOKEN="$TOKEN" RECALL_PORT="$1" RECALL_DB_PATH="$WORK/$1.db" \
    RECALL_MERGE_ENABLED=false RECALL_RATE_LIMIT_MAX=3 RECALL_RATE_LIMIT_WINDOW_MS=60000 \
    RECALL_TLS_CERT="$CERT" RECALL_TLS_KEY="$KEY" \
    "$BIN" >"$WORK/$1.log" 2>&1 &
  PIDS="$PIDS $!"
  for _ in $(seq 1 50); do curl -sfk "https://127.0.0.1:$1/health" >/dev/null 2>&1 && return; sleep 0.2; done
  echo "server on :$1 never came up"; cat "$WORK/$1.log"; exit 1
}

code() { # port, header args...
  local port=$1; shift
  curl -sk -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "$@" \
    "https://127.0.0.1:$port/sync?project_key=a/b"
}

echo
echo "Direct TLS: RECALL_TLS_CERT/RECALL_TLS_KEY, no RECALL_TRUSTED_IP_HEADER, limit 3/60s"
start 8964

# Every request in this script comes from the same real peer, 127.0.0.1, so
# burning the bucket once and then trying to escape it with a forged header
# is the whole test: a header ever changing the outcome below is the bug.
for _ in 1 2 3; do code 8964 -H "X-Real-IP: 10.0.0.1" >/dev/null; done
chk "a fourth request over TLS is limited" \
  "$(code 8964)" "429"

chk "a forged x-real-ip does not buy a fresh bucket over TLS" \
  "$(code 8964 -H 'X-Real-IP: 10.0.0.99')" "429"
chk "a forged cf-connecting-ip does not buy a fresh bucket over TLS" \
  "$(code 8964 -H 'CF-Connecting-IP: 9.9.9.9')" "429"
chk "a forged x-forwarded-for does not buy a fresh bucket over TLS" \
  "$(code 8964 -H 'X-Forwarded-For: 8.8.8.8, 10.0.0.1')" "429"
chk "a forged true-client-ip does not buy a fresh bucket over TLS" \
  "$(code 8964 -H 'True-Client-IP: 7.7.7.7')" "429"

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
