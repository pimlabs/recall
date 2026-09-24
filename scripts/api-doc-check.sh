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

# A limit high enough that the checks below never meet it; the rate limit
# has a server of its own at the end.
RECALL_TOKEN="$TOKEN" RECALL_PORT="$PORT" RECALL_DB_PATH="$WORK/db.sqlite" \
  RECALL_MERGE_ENABLED=false RECALL_RATE_LIMIT_MAX=1000 "$BIN" >"$WORK/server.log" 2>&1 &
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
check "discovery's capabilities" 'devices limits merge_base merge_queue scopes' \
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
check "a name with a zero-width space is 400" '400' \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${json[@]}" \
     -d "{\"name\":\"lap\\u200btop\",\"public_key\":\"$KEY\"}" "$URL/v1/devices/enroll")"
check "an enrolment over 8 KiB is 413" '{"error":"request body too large"}' \
  "$(python3 -c 'import json; print(json.dumps({"name": "x", "public_key": "k", "agent": "a" * 9000}))' \
     | curl -s -X POST "${json[@]}" --data-binary @- "$URL/v1/devices/enroll")"
check "polling before approval: authorization_pending" '400 {"error":"authorization_pending"}' \
  "$(curl -s -w ' %{http_code}' -X POST "${json[@]}" -d "{\"enrollment_id\":\"$ENROLLMENT\"}" \
     "$URL/v1/devices/enroll/poll" | awk '{print $2, $1}')"
check "polling again at once: slow_down" '{"error":"slow_down"}' "$(poll "$ENROLLMENT")"
check "an unknown enrollment_id: invalid_grant" '{"error":"invalid_grant"}' "$(poll enr_unknown)"
check "looking up a code needs a token" '401' \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/v1/devices/pending/$CODE")"
curl -s -D "$WORK/looked.headers" "${auth[@]}" \
  "$URL/v1/devices/pending/$(printf '%s' "$CODE" | tr 'A-Z' 'a-z' | tr -d '-')" >"$WORK/looked.json"
check "looking up a code, typed any way, shows what it would approve" \
  "user_code name agent fingerprint expires_in $CODE laptop doc-check" \
  "$(python3 -c '
import json,sys; d=json.load(open(sys.argv[1])); print(" ".join(d.keys()), d["user_code"], d["name"], d["agent"])' "$WORK/looked.json")"
check "the lookup is never cached" 'no-store' \
  "$(tr -d '\r' <"$WORK/looked.headers" | awk 'tolower($1)=="cache-control:"{print $2}')"
check "an unknown code is 404" '{"error":"no enrolment is waiting with that code"}' \
  "$(curl -s "${auth[@]}" "$URL/v1/devices/pending/ZZZZ-ZZZZ")"
check "approving needs a token" '{"error":"unauthorized"}' \
  "$(curl -s -X POST "${json[@]}" -d "{\"user_code\":\"$CODE\"}" "$URL/v1/devices/approve")"
check "approving with a fingerprint that is not the code's key is 409" \
  '{"error":"that code'"'"'s key does not have the fingerprint given; nothing was approved"}' \
  "$(curl -s -X POST "${auth[@]}" "${json[@]}" \
     -d "{\"user_code\":\"$CODE\",\"fingerprint\":\"SHA256:not-this-one\"}" "$URL/v1/devices/approve")"
curl -s -X POST "${auth[@]}" "${json[@]}" \
  -d "{\"user_code\":\"$CODE\",\"fingerprint\":\"$(field "$WORK/looked.json" fingerprint)\"}" \
  "$URL/v1/devices/approve" >"$WORK/device.json"
check "approving answers with the device" \
  'id name scope ephemeral agent fingerprint public_key authkey_id created_at last_seen revoked_at' \
  "$(keys "$WORK/device.json")"
check "approved with sync scope unless asked" 'sync' "$(field "$WORK/device.json" scope)"
DEVICE=$(field "$WORK/device.json" id)
check "the poll after approval: the device" "{\"device_id\":\"$DEVICE\",\"scope\":\"sync\"}" "$(poll "$ENROLLMENT")"
check "a code is approved once" '409' \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" "${json[@]}" \
     -d "{\"user_code\":\"$CODE\"}" "$URL/v1/devices/approve")"
check "a decided code is no longer pending" '{"error":"that code was already approved or denied"}' \
  "$(curl -s "${auth[@]}" "$URL/v1/devices/pending/$CODE")"
approve_code() { # user_code
  curl -s -X POST "${auth[@]}" "${json[@]}" -d "{\"user_code\":\"$1\"}" "$URL/v1/devices/approve"
}
deny_code() { # user_code
  curl -s -o /dev/null -X POST "${auth[@]}" "${json[@]}" -d "{\"user_code\":\"$1\"}" "$URL/v1/devices/deny"
}
enroll Laptop >"$WORK/clash.json"
check "enrolling as a name already taken answers as any enrolment does" 'enrollment_id user_code expires_in interval' \
  "$(keys "$WORK/clash.json")"
check "approving a second device named laptop, in any case, is 409" \
  '{"error":"a device named Laptop already exists; revoke it first, or enrol with another name"}' \
  "$(approve_code "$(field "$WORK/clash.json" user_code)")"
deny_code "$(field "$WORK/clash.json" user_code)"
# The Cyrillic a (U+0430) goes as a JSON escape, so this file stays ASCII.
CYRILLIC_LAPTOP=$(printf 'l\\u%sptop' 0430)
check "laptop with a Cyrillic a, or a 1 for its l, is laptop too: 409 on approval" '409 409' \
  "$(for name in "$CYRILLIC_LAPTOP" 1aptop; do
       enroll "$name" >"$WORK/lookalike.json"
       code=$(field "$WORK/lookalike.json" user_code)
       curl -s -o /dev/null -w '%{http_code} ' -X POST "${auth[@]}" "${json[@]}" \
         -d "{\"user_code\":\"$code\"}" "$URL/v1/devices/approve"
       deny_code "$code"
     done | sed 's/ $//')"
check "who am I, with the token: not a device" \
  '404 {"error":"not a device: this request was authenticated with RECALL_TOKEN"}' \
  "$(curl -s -o "$WORK/me.json" -w '%{http_code}' "${auth[@]}" "$URL/v1/devices/me") $(cat "$WORK/me.json")"

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

curl -s -X POST "${auth[@]}" "${json[@]}" -d '{"tag":"cloud","expires_in_days":90}' \
  "$URL/v1/authkeys" >"$WORK/key.json"
check "an authkey is shown once, with its prefix, ephemeral unless asked" 'True True' \
  "$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["key"].startswith("recall-ak-"), d["ephemeral"])' "$WORK/key.json")"
check "a key made without max_devices enrols at most 25" '25' "$(field "$WORK/key.json" max_devices)"
check "the list never shows it again" 'False' \
  "$(curl -s "${auth[@]}" "$URL/v1/authkeys" | python3 -c '
import json,sys; print("key" in json.load(sys.stdin)["authkeys"][0])')"
enroll laptop ",\"authkey\":\"$(field "$WORK/key.json" key)\"" >"$WORK/approved.json"
check "enrolling with the key is approved at once, named by the server" 'device_id name scope ephemeral cloud-' \
  "$(keys "$WORK/approved.json") $(field "$WORK/approved.json" name | cut -c1-6)"
curl -s -X POST "${auth[@]}" "${json[@]}" -d '{"revoke_devices":true}' \
  "$URL/v1/authkeys/$(field "$WORK/key.json" id)/revoke" >/dev/null
check "a revoked key enrols nothing" '{"error":"unauthorized: this authkey has been revoked"}' \
  "$(enroll cloud ",\"authkey\":\"$(field "$WORK/key.json" key)\"")"
check "revoking a key with revoke_devices revokes what it enrolled" 'True' \
  "$(curl -s "${auth[@]}" "$URL/v1/devices" | python3 -c '
import json,sys
d={x["id"]: x for x in json.load(sys.stdin)["devices"]}
print(d[sys.argv[1]]["revoked_at"] is not None)' "$(field "$WORK/approved.json" device_id)")"
check "revoking a device stamps revoked_at" 'True' \
  "$(curl -s -X POST "${auth[@]}" "$URL/v1/devices/$DEVICE/revoke" | python3 -c '
import json,sys; print(json.load(sys.stdin)["revoked_at"] is not None)')"
check "a signature from an unknown device is 401" '{"error":"unauthorized: unknown device"}' \
  "$(curl -s -H 'Signature-Input: sig1=("@method");keyid="dev_nobody";created=1;nonce="n"' \
     -H 'Signature: sig1=:AAAA:' "$URL/sync?project_key=a/b")"
for n in 1 2 3 4 5; do enroll "waiting-$n" >/dev/null; done
check "a sixth enrolment waiting from one address is 429" '429' \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${json[@]}" \
     -d "{\"name\":\"waiting-6\",\"public_key\":\"$KEY\"}" "$URL/v1/devices/enroll")"
v6_enroll() { # address, name
  curl -s -o /dev/null -w '%{http_code}' -X POST "${json[@]}" -H "cf-connecting-ip: $1" \
    -d "{\"name\":\"$2\",\"public_key\":\"$KEY\"}" "$URL/v1/devices/enroll"
}
for n in 1 2 3 4 5; do v6_enroll "2001:db8:5:5::$n" "v6-$n" >/dev/null; done
check "every IPv6 address in one /64 is one address: the sixth waiting is 429" '429 200' \
  "$(v6_enroll 2001:db8:5:5:ffff:ffff:ffff:ffff v6-6) $(v6_enroll 2001:db8:5:6::1 v6-7)"

echo "Jobs, without a worker"
# Claiming and posting results need a worker's signature, which takes a
# signing client rather than curl; crates/recall-server/tests/jobs.rs covers
# those. What curl can show: who is refused, and what the owner sees.
check "claiming with no credential is 401" '{"error":"unauthorized"}' \
  "$(curl -s -X POST "${json[@]}" -d '{"kinds":["merge"]}' "$URL/v1/jobs/claim")"
check "claiming with the token is 403: only a worker claims" \
  '403 {"error":"forbidden: this needs a device with the worker scope"}' \
  "$(curl -s -o "$WORK/claim.json" -w '%{http_code}' -X POST "${auth[@]}" "${json[@]}" \
     -d '{"kinds":["merge"]}' "$URL/v1/jobs/claim") $(cat "$WORK/claim.json")"
check "posting a result with the token is 403" '403' \
  "$(curl -s -o /dev/null -w '%{http_code}' -X POST "${auth[@]}" "${json[@]}" \
     -d '{"lease_id":"lse_x","error":"x"}' "$URL/v1/jobs/job_x/result")"
check "listing jobs needs a token" '401' \
  "$(curl -s -o /dev/null -w '%{http_code}' "$URL/v1/jobs")"
check "no worker, no jobs" '{"jobs":[]}' "$(curl -s "${auth[@]}" "$URL/v1/jobs")"
check "an unknown state is 400" '{"error":"state must be queued, leased, done or failed"}' \
  "$(curl -s "${auth[@]}" "$URL/v1/jobs?state=stuck")"
check "retrying an unknown job is 404" '{"error":"no job has that id"}' \
  "$(curl -s -X POST "${auth[@]}" "$URL/v1/jobs/job_nothing/retry")"
check "a scope that does not exist is 400" '{"error":"scope must be sync, admin or worker"}' \
  "$(curl -s -X POST "${auth[@]}" "${json[@]}" -d '{"user_code":"BCDF-GHJK","scope":"root"}' \
     "$URL/v1/devices/approve")"

echo "Jobs, with a worker"
# Its own server, with merging on and a claude that does not exist: a stale
# push is only ever queued here, never merged, which is what is checked.
JQ_PORT=8933
JQ="http://127.0.0.1:$JQ_PORT"
RECALL_TOKEN="$TOKEN" RECALL_PORT="$JQ_PORT" RECALL_DB_PATH="$WORK/jq.sqlite" \
  RECALL_MERGE_ENABLED=true RECALL_CLAUDE_BIN="$WORK/no-claude-here" RECALL_RATE_LIMIT_MAX=1000 \
  "$BIN" >"$WORK/jq.log" 2>&1 &
JQS=$!
for _ in $(seq 1 40); do curl -sf "$JQ/health" >/dev/null 2>&1 && break; sleep 0.25; done
merge_keys() {
  curl -s "$JQ/health" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin)["merge"].keys()))'
}
jq_push() { # content, base
  curl -s -X POST "${auth[@]}" "${json[@]}" \
    -d "{\"project_key\":\"acme/app\",\"file_path\":\"topics/auth.md\",\"content\":\"$1\",\"source_env\":\"laptop\",\"base_sha256\":\"$2\"}" \
    "$JQ/sync"
}
OLDER=e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
check "no worker enrolled: health has no worker or queue" 'enabled claude_cli last_merge_error' "$(merge_keys)"
curl -s -X POST "${json[@]}" \
  -d "{\"name\":\"worker\",\"public_key\":\"$KEY\",\"agent\":\"recall-worker/doc-check\"}" \
  "$JQ/v1/devices/enroll" >"$WORK/wenroll.json"
curl -s -X POST "${auth[@]}" "${json[@]}" \
  -d "{\"user_code\":\"$(field "$WORK/wenroll.json" user_code)\",\"scope\":\"worker\"}" \
  "$JQ/v1/devices/approve" >"$WORK/worker.json"
check "approving with the worker scope" 'worker' "$(field "$WORK/worker.json" scope)"
check "a worker enrolled: health gains worker and queue" \
  'enabled claude_cli last_merge_error worker queue' "$(merge_keys)"
check "the worker, before its first claim" '{"last_claim_at": null, "agent": "recall-worker/doc-check"}' \
  "$(curl -s "$JQ/health" | python3 -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["merge"]["worker"]))')"
jq_push "# Auth\\n" "" >/dev/null
jq_push "# Auth, again\\n" "$OLDER" >"$WORK/queued.json"
check "a stale push is queued: merged false, and the job" \
  'ok project_key file_path deleted merged updated_at merge_job False True' \
  "$(python3 -c '
import json,sys; d=json.load(open(sys.argv[1])); print(" ".join(d.keys()), d["merged"], d["merge_job"].startswith("job_"))' "$WORK/queued.json")"
check "stored as sent" '# Auth, again' \
  "$(curl -s "${auth[@]}" "$JQ/sync?project_key=acme/app" | python3 -c '
import json,sys; print(json.load(sys.stdin)["files"][0]["content"].strip())')"
curl -s "${auth[@]}" "$JQ/v1/jobs" >"$WORK/jobs.json"
check "the job, as listed, without file content" \
  "id kind state project_key file_path attempt created_at updated_at error follow_up merge queued" \
  "$(python3 -c '
import json,sys; j=json.load(open(sys.argv[1]))["jobs"][0]; print(" ".join(j.keys()), j["kind"], j["state"])' "$WORK/jobs.json")"
check "the queue in health" '{"queued": 1, "leased": 0, "failed": 0}' \
  "$(curl -s "$JQ/health" | python3 -c '
import json,sys; q=json.load(sys.stdin)["merge"]["queue"]; print(json.dumps({k: q[k] for k in ("queued","leased","failed")}))')"
check "only a failed job is retried" '{"error":"only a failed job can be retried; this one is queued"}' \
  "$(curl -s -X POST "${auth[@]}" "$JQ/v1/jobs/$(field "$WORK/queued.json" merge_job)/retry")"
curl -s -X POST "${auth[@]}" "$JQ/v1/devices/$(field "$WORK/worker.json" id)/revoke" >/dev/null
# The revocation starts a drain in the background; this server's claude
# does not exist, so the waiting job is failed rather than merged.
jq_queue() {
  curl -s "$JQ/health" | python3 -c '
import json,sys; q=json.load(sys.stdin)["merge"].get("queue"); print(json.dumps(q and {k: q[k] for k in ("queued","leased","failed")}))'
}
for _ in $(seq 1 40); do [ "$(jq_queue)" = '{"queued": 0, "leased": 0, "failed": 1}' ] && break; sleep 0.25; done
check "the worker revoked: its waiting job is failed, and health still shows the queue" \
  '{"queued": 0, "leased": 0, "failed": 1}' "$(jq_queue)"
check "health names the failed job, and no project or file" 'True False' \
  "$(curl -s "$JQ/health" | python3 -c '
import json,sys; m=json.load(sys.stdin)["merge"]["last_merge_error"]["message"]
print("see GET /v1/jobs?state=failed" in m, "acme" in m or "topics" in m)')"
check "the worker revoked: a stale push answers as before" \
  'ok project_key file_path deleted merged updated_at' \
  "$(jq_push "# Auth, once more\\n" "$OLDER" | python3 -c 'import json,sys; print(" ".join(json.load(sys.stdin).keys()))')"
kill $JQS 2>/dev/null

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
