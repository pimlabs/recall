#!/usr/bin/env bash
#
# The direct-TLS sibling of trusted-ip-check.sh. With TLS on, config.rs
# forces RECALL_TRUSTED_IP_HEADER empty (or refuses to start if it names a
# header): there is no ingress in front any more, so the rate limiter has to
# key on the real TCP peer address instead. This proves that on a real
# socket, over a real TLS handshake, rather than trusting the unit tests to
# have covered what axum-server actually hands the middleware.
#
#   cargo build --release
#   ./scripts/tls-trusted-ip-check.sh target/release/recall-server
#
# Two halves to that proof, and both matter. Forged headers must not buy a
# fresh bucket, and a genuinely different peer must get one: the second is
# what catches the peer address never reaching the limiter at all (serving
# without into_make_service_with_connect_info), which would put every client
# in one shared "unknown" bucket and still pass every forged-header check.
# That is why the second peer is a second loopback address, 127.0.0.2, and
# why it is checked over HTTP/2 as well as HTTP/1.1.
#
# Also checked here, since it needs a real process to signal: files mode
# picks up a replaced certificate on SIGHUP without restarting.
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
SERVER_PID=""
pass=0; fail=0
cleanup() { [ -n "$PIDS" ] && kill -9 $PIDS 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
chk() { [ "$2" = "$3" ] && ok "$1" || { bad "$1"; printf '        got %s want %s\n' "$2" "$3"; }; }

CERT="$WORK/cert.pem"
KEY="$WORK/key.pem"
selfsigned() { # cert, key
  openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
    -keyout "$2" -out "$1" -subj "/CN=localhost" >/dev/null 2>&1
  chmod 600 "$2"
  if [ ! -s "$1" ] || [ ! -s "$2" ]; then
    echo "openssl did not produce a certificate/key pair"; exit 1
  fi
}
selfsigned "$CERT" "$KEY"

start() { # port
  RECALL_TOKEN="$TOKEN" RECALL_PORT="$1" RECALL_DB_PATH="$WORK/$1.db" \
    RECALL_MERGE_ENABLED=false RECALL_RATE_LIMIT_MAX=3 RECALL_RATE_LIMIT_WINDOW_MS=60000 \
    RECALL_TLS_CERT="$CERT" RECALL_TLS_KEY="$KEY" \
    "$BIN" >"$WORK/$1.log" 2>&1 &
  SERVER_PID=$!
  PIDS="$PIDS $SERVER_PID"
  for _ in $(seq 1 50); do curl -sfk "https://127.0.0.1:$1/health" >/dev/null 2>&1 && return; sleep 0.2; done
  echo "server on :$1 never came up"; cat "$WORK/$1.log"; exit 1
}

code() { # port, peer address (127.0.0.x), curl args...
  local port=$1 peer=$2; shift 2
  curl -sk -o /dev/null -w '%{http_code}' --interface "$peer" \
    -H "Authorization: Bearer $TOKEN" "$@" \
    "https://$peer:$port/sync?project_key=a/b"
}

fingerprint() { # port
  echo | timeout 5 openssl s_client -connect "127.0.0.1:$1" -servername localhost 2>/dev/null \
    | openssl x509 -noout -fingerprint -sha256 2>/dev/null
}

P=8964
echo
echo "Direct TLS: RECALL_TLS_CERT/RECALL_TLS_KEY, no RECALL_TRUSTED_IP_HEADER, limit 3/60s"
start $P

# Burn 127.0.0.1's bucket, then try to escape it with a forged header: a
# header ever changing the outcome below is the bug.
for _ in 1 2 3; do code $P 127.0.0.1 -H "X-Real-IP: 10.0.0.1" >/dev/null; done
chk "a fourth request over TLS is limited" \
  "$(code $P 127.0.0.1)" "429"

chk "a forged x-real-ip does not buy a fresh bucket over TLS" \
  "$(code $P 127.0.0.1 -H 'X-Real-IP: 10.0.0.99')" "429"
chk "a forged cf-connecting-ip does not buy a fresh bucket over TLS" \
  "$(code $P 127.0.0.1 -H 'CF-Connecting-IP: 9.9.9.9')" "429"
chk "a forged x-forwarded-for does not buy a fresh bucket over TLS" \
  "$(code $P 127.0.0.1 -H 'X-Forwarded-For: 8.8.8.8, 10.0.0.1')" "429"
chk "a forged true-client-ip does not buy a fresh bucket over TLS" \
  "$(code $P 127.0.0.1 -H 'True-Client-IP: 7.7.7.7')" "429"

# The other half: a real second peer is not caught in 127.0.0.1's bucket.
# If the socket's peer address never reached the limiter, every request
# would share one bucket and this would be 429 too.
chk "a different peer address has its own bucket over TLS" \
  "$(code $P 127.0.0.2)" "200"

echo
echo "The same over HTTP/2"
chk "HTTP/2 is negotiated" \
  "$(curl -sk --http2 -o /dev/null -w '%{http_version}' "https://127.0.0.1:$P/health")" "2"
for _ in 1 2 3; do code $P 127.0.0.3 --http2 -H "X-Real-IP: 10.0.0.1" >/dev/null; done
chk "a fourth request over HTTP/2 is limited" \
  "$(code $P 127.0.0.3 --http2)" "429"
chk "a forged x-real-ip does not buy a fresh bucket over HTTP/2" \
  "$(code $P 127.0.0.3 --http2 -H 'X-Real-IP: 10.0.0.98')" "429"
chk "a forged cf-connecting-ip does not buy a fresh bucket over HTTP/2" \
  "$(code $P 127.0.0.3 --http2 -H 'CF-Connecting-IP: 9.9.9.8')" "429"
chk "a different peer address has its own bucket over HTTP/2" \
  "$(code $P 127.0.0.4 --http2)" "200"

echo
echo "Files mode reloads a replaced certificate on SIGHUP"
before=$(fingerprint $P)
selfsigned "$WORK/cert2.pem" "$WORK/key2.pem"
want=$(openssl x509 -noout -fingerprint -sha256 -in "$WORK/cert2.pem")
# Replaced in place, the way a certbot deploy hook's copy would.
cat "$WORK/cert2.pem" >"$CERT"
cat "$WORK/key2.pem" >"$KEY"
kill -HUP "$SERVER_PID"
after=""
for _ in $(seq 1 25); do
  after=$(fingerprint $P)
  [ "$after" = "$want" ] && break
  sleep 0.2
done
if [ -n "$before" ] && [ "$before" != "$want" ] && [ "$after" = "$want" ]; then
  ok "the new certificate is served after SIGHUP"
else
  bad "the new certificate is served after SIGHUP"
  printf '        before %s\n        after  %s\n        want   %s\n' "$before" "$after" "$want"
fi
chk "the server is still running after SIGHUP" \
  "$(kill -0 "$SERVER_PID" 2>/dev/null && echo running || echo gone)" "running"
chk "the reload is logged" \
  "$(grep -c 'reloaded certificate' "$WORK/$P.log")" "1"

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
