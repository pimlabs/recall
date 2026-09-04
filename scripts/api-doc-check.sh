#!/usr/bin/env bash
#
# Checks that docs/api.md describes what the server actually does.
#
# The API reference is a frozen compatibility surface, so it is worth more
# than prose: every claim it makes about status codes, error wording, field
# order and the null-versus-empty-string distinction is asserted here against
# a real server on a real socket. If a handler changes and the document
# doesn't, this fails.
#
#   ./scripts/api-doc-check.sh target/release/recall
#
set -u
BIN="$1"
WORK=$(mktemp -d)
PORT=8931
URL="http://127.0.0.1:$PORT"
TOKEN="doc-check-token"

RECALL_TOKEN="$TOKEN" RECALL_PORT="$PORT" RECALL_DB_PATH="$WORK/db.sqlite" \
  RECALL_MERGE_ENABLED=false "$BIN" serve >"$WORK/server.log" 2>&1 &
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

echo "Rate limiting"
RL_PORT=8932
RECALL_TOKEN="$TOKEN" RECALL_PORT="$RL_PORT" RECALL_DB_PATH="$WORK/rl.sqlite" \
  RECALL_MERGE_ENABLED=false RECALL_RATE_LIMIT_MAX=3 RECALL_RATE_LIMIT_WINDOW_MS=60000 \
  "$BIN" serve >"$WORK/rl.log" 2>&1 &
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
