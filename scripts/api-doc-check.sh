#!/usr/bin/env bash
#
# Checks that docs/reference/api.md describes what the server actually does.
#
# The API reference is a frozen compatibility surface, so it is worth more
# than prose: every claim it makes about status codes, error wording, field
# order and the null-versus-empty-string distinction is asserted here against
# a real server on a real socket. If a handler changes and the document
# doesn't, this fails.
#
#   ./scripts/api-doc-check.sh target/release/recall-server
#
set -u
BIN="$1"
WORK=$(mktemp -d)
PORT=8931
URL="http://127.0.0.1:$PORT"
TOKEN="doc-check-token"

RECALL_TOKEN="$TOKEN" RECALL_PORT="$PORT" RECALL_DB_PATH="$WORK/db.sqlite" \
  RECALL_MERGE_ENABLED=false "$BIN" >"$WORK/server.log" 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null; rm -rf "$WORK"' EXIT

for _ in $(seq 1 40); do
  curl -sf "$URL/health" >/dev/null 2>&1 && break
  sleep 0.25
done

pass=0; fail=0
check() { # name expected actual
  if [ "$2" = "$3" ]; then printf '  PASS %s\n' "$1"; pass=$((pass+1));
  else printf '  FAIL %s\n       want: %s\n       got : %s\n' "$1" "$2" "$3"; fail=$((fail+1)); fi
}

auth=(-H "Authorization: Bearer $TOKEN")

echo "Auth"
check "no header is 401" "401" \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/sync?project_key=a/b")"
check "401 body wording" '{"error":"unauthorized"}' \
  "$(curl -s "$URL/sync?project_key=a/b")"
check "wrong token is 401" "401" \
  "$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer nope' "$URL/sync?project_key=a/b")"
check "health needs no token" "200" \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/health")"
check "admin page needs no token" "200" \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/admin")"
check "admin stats needs a token" "401" \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/admin/stats")"

echo "Discovery and the protocol header"
check "discovery needs no token" "200" \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/.well-known/recall")"
check "discovery's top-level keys" 'protocol server min_client auth capabilities' \
  "$(curl -s "$URL/.well-known/recall" | python3 -c '
import json,sys; print(" ".join(json.load(sys.stdin).keys()))')"
check "discovery's capabilities" 'devices limits merge_base scopes' \
  "$(curl -s "$URL/.well-known/recall" | python3 -c '
import json,sys; print(" ".join(json.load(sys.stdin)["capabilities"].keys()))')"
check "discovery's auth methods" 'bearer device-sig-v1' \
  "$(curl -s "$URL/.well-known/recall" | python3 -c '
import json,sys; print(" ".join(json.load(sys.stdin)["auth"]["methods"]))')"
check "the devices capability" '{"enroll_path": "/v1/devices/enroll", "code_ttl_seconds": 900, "poll_interval_seconds": 5, "signature_window_seconds": 60}' \
  "$(curl -s "$URL/.well-known/recall" | python3 -c '
import json,sys; print(json.dumps(json.load(sys.stdin)["capabilities"]["devices"]))')"
check "discovery's protocol" '{"current": 1, "supported": [1]}' \
  "$(curl -s "$URL/.well-known/recall" | python3 -c '
import json,sys; print(json.dumps(json.load(sys.stdin)["protocol"]))')"
check "an unknown protocol is 400" '{"error":"this server speaks Recall protocol 1, and the request asked for 2. Upgrade whichever side is older; GET /.well-known/recall says what this server supports"}' \
  "$(curl -s "${auth[@]}" -H 'Recall-Protocol: 2' "$URL/sync?project_key=a/b")"
check "protocol 1 is served" "200" \
  "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -H 'Recall-Protocol: 1' "$URL/sync?project_key=a/b")"

echo "POST /sync"
check "no project_key is 400" "400" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" \
     -H 'Content-Type: application/json' -d '{"file_path":"a.md","content":"x"}' "$URL/sync")"
check "traversal is 400" '{"error":"file_path must be relative, no traversal"}' \
  "$(curl -s -X POST "${auth[@]}" -H 'Content-Type: application/json' \
     -d '{"project_key":"acme/app","file_path":"../escape.md","content":"x"}' "$URL/sync")"
check "..config.md is accepted" "200" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" \
     -H 'Content-Type: application/json' \
     -d '{"project_key":"acme/app","file_path":"..config.md","content":"x"}' "$URL/sync")"
check "a write with no content is 400" "400" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" \
     -H 'Content-Type: application/json' \
     -d '{"project_key":"acme/app","file_path":"a.md"}' "$URL/sync")"
check "an empty file is accepted" "200" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" \
     -H 'Content-Type: application/json' \
     -d '{"project_key":"acme/app","file_path":"empty.md","content":""}' "$URL/sync")"

curl -s -X POST "${auth[@]}" -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"MEMORY.md","content":"# Memory\n","source_env":"laptop"}' \
  "$URL/sync" >"$WORK/push.json"
check "push response field order" \
  'ok project_key file_path deleted merged updated_at' \
  "$(python3 -c 'import json,sys;print(" ".join(json.load(open(sys.argv[1])).keys()))' "$WORK/push.json")"

echo "GET /sync"
check "unknown project is 200 with no files" '{"project_key":"never/seen","files":[]}' \
  "$(curl -s "${auth[@]}" "$URL/sync?project_key=never/seen")"
check "no project_key is 400" "400" \
  "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "$URL/sync")"

curl -s -X POST "${auth[@]}" -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"gone.md","content":"bye","source_env":"laptop"}' \
  "$URL/sync" >/dev/null
curl -s -X POST "${auth[@]}" -H 'Content-Type: application/json' \
  -d '{"project_key":"acme/app","file_path":"gone.md","deleted":true,"source_env":"laptop"}' \
  "$URL/sync" >/dev/null
curl -s "${auth[@]}" "$URL/sync?project_key=acme/app" >"$WORK/pull.json"

check "a tombstone reports content null" "None True" \
  "$(python3 -c '
import json,sys
files={f["file_path"]: f for f in json.load(open(sys.argv[1]))["files"]}
g=files["gone.md"]
print(g["content"], g["deleted"])' "$WORK/pull.json")"
check "an empty file survives as an empty string" '""' \
  "$(python3 -c '
import json,sys
files={f["file_path"]: f for f in json.load(open(sys.argv[1]))["files"]}
print(json.dumps(files["empty.md"]["content"]))' "$WORK/pull.json")"
check "file field order" 'file_path content source_env updated_at deleted' \
  "$(python3 -c '
import json,sys
print(" ".join(json.load(open(sys.argv[1]))["files"][0].keys()))' "$WORK/pull.json")"

echo "GET /health"
curl -s "$URL/health" >"$WORK/health.json"
check "health top-level keys" 'status git_commit started_at last_sync_at merge' \
  "$(python3 -c '
import json,sys
print(" ".join(k for k in json.load(open(sys.argv[1])).keys()))' "$WORK/health.json")"
check "merge object keys" 'enabled claude_cli last_merge_error' \
  "$(python3 -c '
import json,sys
print(" ".join(json.load(open(sys.argv[1]))["merge"].keys()))' "$WORK/health.json")"
check "timestamp shape" 'True' \
  "$(python3 -c '
import json,re,sys
t=json.load(open(sys.argv[1]))["started_at"]
print(bool(re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z", t)) and len(t)==24)' "$WORK/health.json")"

echo "GET /admin/stats"
curl -s "${auth[@]}" "$URL/admin/stats" >"$WORK/stats.json"
check "stats top-level keys" 'projects totals git_commit' \
  "$(python3 -c '
import json,sys
print(" ".join(json.load(open(sys.argv[1])).keys()))' "$WORK/stats.json")"
check "a project row" 'project_key file_count deleted_count sources last_updated_at' \
  "$(python3 -c '
import json,sys
print(" ".join(json.load(open(sys.argv[1]))["projects"][0].keys()))' "$WORK/stats.json")"
check "admin stats is read-only" "404" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" "$URL/admin/stats")"

echo "Devices"
# RFC 9421's test-key-ed25519 (Appendix B.1.4): a valid key, and a public one.
KEY=JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs
json=(-H 'Content-Type: application/json')
enroll() { # name, [extra JSON members]
  curl -s -X POST "${json[@]}" \
    -d "{\"name\":\"$1\",\"public_key\":\"$KEY\",\"agent\":\"doc-check\"${2:-}}" "$URL/v1/devices/enroll"
}
poll() { # enrollment_id
  curl -s -X POST "${json[@]}" -d "{\"enrollment_id\":\"$1\"}" "$URL/v1/devices/enroll/poll"
}
field() { # file, key
  python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"
}
keys() { # file
  python3 -c 'import json,sys; print(" ".join(json.load(open(sys.argv[1])).keys()))' "$1"
}

enroll laptop >"$WORK/enroll.json"
check "enrolling needs no token, and answers with a code" 'enrollment_id user_code expires_in interval' \
  "$(keys "$WORK/enroll.json")"
check "the code is eight consonants with a hyphen" 'True' \
  "$(python3 -c '
import json,re,sys
print(bool(re.fullmatch(r"[BCDFGHJKLMNPQRSTVWXZ]{4}-[BCDFGHJKLMNPQRSTVWXZ]{4}", json.load(open(sys.argv[1]))["user_code"])))' "$WORK/enroll.json")"
check "fifteen minutes, polled every five seconds" '900 5' \
  "$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["expires_in"], d["interval"])' "$WORK/enroll.json")"
ENROLLMENT=$(field "$WORK/enroll.json" enrollment_id)
CODE=$(field "$WORK/enroll.json" user_code)
check "a bad public key is 400" '{"error":"public_key must be an Ed25519 public key: 32 bytes, base64url without padding"}' \
  "$(curl -s -X POST "${json[@]}" -d '{"name":"x","public_key":"nope"}' "$URL/v1/devices/enroll")"
check "polling before approval: authorization_pending" '400 {"error":"authorization_pending"}' \
  "$(curl -s -w ' %{http_code}' -X POST "${json[@]}" -d "{\"enrollment_id\":\"$ENROLLMENT\"}" \
     "$URL/v1/devices/enroll/poll" | awk '{print $2, $1}')"
check "polling again at once: slow_down" '{"error":"slow_down"}' "$(poll "$ENROLLMENT")"
check "an unknown enrollment_id: invalid_grant" '{"error":"invalid_grant"}' "$(poll enr_unknown)"
check "approving needs a token" '{"error":"unauthorized"}' \
  "$(curl -s -X POST "${json[@]}" -d "{\"user_code\":\"$CODE\"}" "$URL/v1/devices/approve")"
curl -s -X POST "${auth[@]}" "${json[@]}" -d "{\"user_code\":\"$CODE\"}" \
  "$URL/v1/devices/approve" >"$WORK/device.json"
check "approving answers with the device" \
  'id name scope ephemeral agent fingerprint public_key enroll_key_id created_at last_seen revoked_at' \
  "$(keys "$WORK/device.json")"
check "approved with sync scope unless asked" 'sync' "$(field "$WORK/device.json" scope)"
DEVICE=$(field "$WORK/device.json" id)
check "the poll after approval: the device" "{\"device_id\":\"$DEVICE\",\"scope\":\"sync\"}" "$(poll "$ENROLLMENT")"
check "a code is approved once" '409' \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" "${json[@]}" \
     -d "{\"user_code\":\"$CODE\"}" "$URL/v1/devices/approve")"

enroll phone >"$WORK/enroll2.json"
curl -s -X POST "${auth[@]}" "${json[@]}" -d "{\"user_code\":\"$(field "$WORK/enroll2.json" user_code)\"}" \
  "$URL/v1/devices/deny" >/dev/null
check "a denied enrolment: access_denied" '{"error":"access_denied"}' \
  "$(poll "$(field "$WORK/enroll2.json" enrollment_id)")"

check "listing devices needs a token" '401' \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/v1/devices")"
check "listing devices" "$DEVICE" \
  "$(curl -s "${auth[@]}" "$URL/v1/devices" | python3 -c '
import json,sys; print(" ".join(d["id"] for d in json.load(sys.stdin)["devices"]))')"

curl -s -X POST "${auth[@]}" "${json[@]}" -d '{"tag":"cloud","expires_in_days":90,"ephemeral":true}' \
  "$URL/v1/enroll-keys" >"$WORK/key.json"
check "an enrolment key is shown once, with its prefix" 'True' \
  "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["key"].startswith("recall-ek-"))' "$WORK/key.json")"
check "the list never shows it again" 'False' \
  "$(curl -s "${auth[@]}" "$URL/v1/enroll-keys" | python3 -c '
import json,sys; print("key" in json.load(sys.stdin)["enroll_keys"][0])')"
check "enrolling with the key is approved at once" 'device_id scope ephemeral' \
  "$(enroll cloud ",\"enroll_key\":\"$(field "$WORK/key.json" key)\"" | python3 -c '
import json,sys; print(" ".join(json.load(sys.stdin).keys()))')"
curl -s -X POST "${auth[@]}" "$URL/v1/enroll-keys/$(field "$WORK/key.json" id)/revoke" >/dev/null
check "a revoked key enrols nothing" '{"error":"unauthorized: this enrolment key has been revoked"}' \
  "$(enroll cloud ",\"enroll_key\":\"$(field "$WORK/key.json" key)\"")"
check "revoking a device stamps revoked_at" 'True' \
  "$(curl -s -X POST "${auth[@]}" "$URL/v1/devices/$DEVICE/revoke" | python3 -c '
import json,sys; print(json.load(sys.stdin)["revoked_at"] is not None)')"
check "a signature from an unknown device is 401" '{"error":"unauthorized: unknown device"}' \
  "$(curl -s -H 'Signature-Input: sig1=("@method");keyid="dev_nobody";created=1;nonce="n"' \
     -H 'Signature: sig1=:AAAA:' "$URL/sync?project_key=a/b")"

echo "Rate limiting"
RL_PORT=8932
RECALL_TOKEN="$TOKEN" RECALL_PORT="$RL_PORT" RECALL_DB_PATH="$WORK/rl.sqlite" \
  RECALL_MERGE_ENABLED=false RECALL_RATE_LIMIT_MAX=3 RECALL_RATE_LIMIT_WINDOW_MS=60000 \
  "$BIN" >"$WORK/rl.log" 2>&1 &
RL=$!
for _ in $(seq 1 40); do curl -sf "http://127.0.0.1:$RL_PORT/health" >/dev/null 2>&1 && break; sleep 0.25; done
for _ in 1 2 3; do curl -s -o /dev/null "${auth[@]}" "http://127.0.0.1:$RL_PORT/sync?project_key=a/b"; done
check "over the limit is 429" "429" \
  "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" "http://127.0.0.1:$RL_PORT/sync?project_key=a/b")"
check "429 body wording" '{"error":"rate limit exceeded, try again later"}' \
  "$(curl -s "${auth[@]}" "http://127.0.0.1:$RL_PORT/sync?project_key=a/b")"
check "retry-after is sent" "60" \
  "$(curl -s -D - -o /dev/null "${auth[@]}" "http://127.0.0.1:$RL_PORT/sync?project_key=a/b" \
     | tr -d '\r' | awk 'tolower($1)=="retry-after:"{print $2}')"
check "rate limiting precedes auth" "429" \
  "$(curl -s -o /dev/null -w '%{http_code}' -H 'Authorization: Bearer wrong' \
     "http://127.0.0.1:$RL_PORT/sync?project_key=a/b")"
kill $RL 2>/dev/null

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
