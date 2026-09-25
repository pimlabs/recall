#!/usr/bin/env bash
#
# Captures what a released server puts on the wire, as golden fixtures for
# crates/recall-wire/tests/golden.rs.
#
#   ./scripts/capture-wire-fixtures.sh 0.4.0
#   ./scripts/capture-wire-fixtures.sh 0.4.1 target/release/recall-server
#
# Downloads that version's release archive for this machine (the server's
# own, from 0.4.0), runs its server on a scratch database, and saves each
# response into crates/recall-wire/fixtures/wire/<version>/. A file that already exists is
# left alone: a shipped fixture is never rewritten, see the README there.
# Request fixtures are not captured here; they are written from the
# wire types at that tag.
#
# With a second argument, that local recall-server binary is run instead of
# downloading one: how a version's fixtures are captured from a development
# build before it is released.
set -u

VERSION="${1:?usage: capture-wire-fixtures.sh <version> [recall-server binary]}"
LOCAL_BIN="${2:-}"
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

if [ -n "$LOCAL_BIN" ]; then
  BIN="$LOCAL_BIN"
  SERVE=""
else
  curl -sSfL -o "$WORK/a.tar.gz" \
    "https://github.com/pimlabs/recall/releases/download/v$VERSION/$ASSET.tar.gz" || exit 1
  tar -xzf "$WORK/a.tar.gz" -C "$WORK"
  BIN="$WORK/$ASSET"
fi
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
# Captured again below, with the worker and the queue in it, on a server
# that has the merge queue; only when this run is the one that wrote it.
[ -e "$OUT/health.json" ] || HEALTH_CAPTURED_HERE=1
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

# Devices, from the version that added them (0.4.1): a server that does not
# list the capability has none of these routes. One enrolment is followed
# through, so each response is a real one from the step before. The public
# key is RFC 9421's test-key-ed25519 (Appendix B.1.4): a valid key whose
# private half is published, so nothing here is anyone's secret, and the
# ids and the authkey come from a scratch database deleted on exit.
if curl -s "$URL/.well-known/recall" | grep -q '"devices"'; then
  KEY=JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs
  JSON="Content-Type: application/json"
  AGENT="recall/$VERSION (linux-x86_64)"

  # fetch <name> <curl args...>: the body into $WORK/<name>, the status
  # into $WORK/<name>.code.
  fetch() {
    local name=$1
    shift
    curl -s -o "$WORK/$name" -w '%{http_code}' "$@" >"$WORK/$name.code"
  }
  # keep <fixture> <name>: as save above, for a response already fetched.
  keep() {
    local file="$OUT/$1" code
    code=$(cat "$WORK/$2.code")
    if [ -e "$file" ]; then
      echo "kept     $file"
      return
    fi
    case "$code" in
      2?? | 4??) cp "$WORK/$2" "$file"; echo "captured $file ($code)" ;;
      *) echo "skipped  $file (HTTP $code)" ;;
    esac
  }
  field() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$WORK/$1" "$2"
  }

  fetch enroll -X POST -H "$JSON" \
    -d "{\"name\":\"laptop\",\"public_key\":\"$KEY\",\"agent\":\"$AGENT\"}" \
    "$URL/v1/devices/enroll"
  keep enroll_response_pending.json enroll
  ENROLLMENT=$(field enroll enrollment_id)

  # Before anyone approves it: RFC 8628's authorization_pending.
  fetch pending -X POST -H "$JSON" -d "{\"enrollment_id\":\"$ENROLLMENT\"}" \
    "$URL/v1/devices/enroll/poll"
  keep enroll_poll_error.json pending

  # What the approver sees before deciding.
  fetch looked -H "$AUTH" "$URL/v1/devices/pending/$(field enroll user_code)"
  keep device_pending_response.json looked

  fetch approve -X POST -H "$AUTH" -H "$JSON" \
    -d "{\"user_code\":\"$(field enroll user_code)\",\"scope\":\"sync\",\"fingerprint\":\"$(field looked fingerprint)\"}" \
    "$URL/v1/devices/approve"
  keep device_approve_response.json approve

  fetch polled -X POST -H "$JSON" -d "{\"enrollment_id\":\"$ENROLLMENT\"}" \
    "$URL/v1/devices/enroll/poll"
  keep enroll_poll_response.json polled

  # The one route only a device can call, signed the way RFC 9421 says,
  # by openssl rather than by Recall's own code: the published private half
  # of test-key-ed25519, which the device above enrolled with.
  if command -v openssl >/dev/null 2>&1; then
    # A server refuses every signature dated up to five seconds past its
    # start (see crates/recall-server/src/server/auth.rs), and this one
    # started a moment ago.
    sleep 6
    printf '%s\n' '-----BEGIN PRIVATE KEY-----' \
      'MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF' \
      '-----END PRIVATE KEY-----' >"$WORK/key.pem"
    CREATED=$(date +%s)
    DIGEST="sha-256=:$(printf '' | openssl dgst -sha256 -binary | base64):"
    PARAMS="(\"@method\" \"@authority\" \"@path\" \"@query\" \"content-digest\" \"recall-protocol\");created=$CREATED;keyid=\"$(field approve id)\";nonce=\"capture-$CREATED\";alg=\"ed25519\""
    printf '"@method": GET\n"@authority": 127.0.0.1:%s\n"@path": /v1/devices/me\n"@query": ?\n"content-digest": %s\n"recall-protocol": 1\n"@signature-params": %s' \
      "$PORT" "$DIGEST" "$PARAMS" >"$WORK/base"
    SIG=$(openssl pkeyutl -sign -inkey "$WORK/key.pem" -rawin -in "$WORK/base" | base64 | tr -d '\n')
    fetch me -H "Recall-Protocol: 1" -H "Content-Digest: $DIGEST" \
      -H "Signature-Input: sig1=$PARAMS" -H "Signature: sig1=:$SIG:" "$URL/v1/devices/me"
    keep device_me_response.json me
  else
    echo "skipped  $OUT/device_me_response.json (no openssl to sign with)"
  fi

  fetch enroll2 -X POST -H "$JSON" \
    -d "{\"name\":\"phone\",\"public_key\":\"$KEY\",\"agent\":\"$AGENT\"}" \
    "$URL/v1/devices/enroll"
  fetch deny -X POST -H "$AUTH" -H "$JSON" -d "{\"user_code\":\"$(field enroll2 user_code)\"}" \
    "$URL/v1/devices/deny"
  keep device_deny_response.json deny

  fetch key -X POST -H "$AUTH" -H "$JSON" -d '{"tag":"cloud","expires_in_days":90,"ephemeral":true,"max_devices":10}' \
    "$URL/v1/authkeys"
  keep authkey_create_response.json key

  fetch approved -X POST -H "$JSON" \
    -d "{\"name\":\"cloud-session\",\"public_key\":\"$KEY\",\"agent\":\"$AGENT\",\"authkey\":\"$(field key key)\"}" \
    "$URL/v1/devices/enroll"
  keep enroll_response_approved.json approved

  fetch devices -H "$AUTH" "$URL/v1/devices"
  keep device_list_response.json devices
  fetch keys -H "$AUTH" "$URL/v1/authkeys"
  keep authkey_list_response.json keys

  # The audit log, from the version that added it: a push the device above
  # signs (openssl again, and that key's published private half), so the
  # push leaf carries a real signature and the approve leaf the key it
  # verifies with; then the checkpoint, every leaf, and a proof, with
  # nothing appended between them, since reading the log appends nothing.
  if curl -s "$URL/.well-known/recall" | grep -q '"audit"' && command -v openssl >/dev/null 2>&1; then
    BODY='{"project_key":"acme/app","file_path":"topics/auth.md","content":"- tokens expire after an hour\n"}'
    CREATED=$(date +%s)
    DIGEST="sha-256=:$(printf '%s' "$BODY" | openssl dgst -sha256 -binary | base64):"
    PARAMS="(\"@method\" \"@authority\" \"@path\" \"@query\" \"content-digest\" \"recall-protocol\");created=$CREATED;keyid=\"$(field approve id)\";nonce=\"capture-push-$CREATED\";alg=\"ed25519\""
    printf '"@method": POST\n"@authority": 127.0.0.1:%s\n"@path": /sync\n"@query": ?\n"content-digest": %s\n"recall-protocol": 1\n"@signature-params": %s' \
      "$PORT" "$DIGEST" "$PARAMS" >"$WORK/push-base"
    SIG=$(openssl pkeyutl -sign -inkey "$WORK/key.pem" -rawin -in "$WORK/push-base" | base64 | tr -d '\n')
    curl -s -o /dev/null -X POST -H "$JSON" -H "Recall-Protocol: 1" -H "Content-Digest: $DIGEST" \
      -H "Signature-Input: sig1=$PARAMS" -H "Signature: sig1=:$SIG:" -d "$BODY" "$URL/sync"

    fetch checkpoint -H "$AUTH" "$URL/v1/audit/checkpoint"
    keep audit_checkpoint_response.json checkpoint
    SIZE=$(field checkpoint tree_size)
    fetch entries -H "$AUTH" "$URL/v1/audit/entries?start=0&end=$SIZE"
    keep audit_entries_response.json entries
    fetch proof -H "$AUTH" "$URL/v1/audit/consistency?first=1&second=$SIZE"
    keep audit_consistency_response.json proof

    # One leaf of each kind the fixtures pin, exactly as the entries page
    # holds it: the signed push, the approve that carries its key, and the
    # enrolment the authkey made.
    leaf() { # fixture, action, actor kind
      python3 -c '
import json, sys
for leaf in json.load(open(sys.argv[1]))["entries"]:
    parsed = json.loads(leaf)
    if parsed["action"] == sys.argv[2] and parsed["actor"]["kind"] == sys.argv[3]:
        sys.stdout.write(leaf)
        break' "$WORK/entries" "$2" "$3" >"$WORK/$1"
      echo 200 >"$WORK/$1.code"
      [ -s "$WORK/$1" ] || echo 500 >"$WORK/$1.code"
      keep "$1.json" "$1"
    }
    leaf audit_leaf_push push device
    leaf audit_leaf_approve approve operator
    leaf audit_leaf_enroll enroll authkey
  fi

  fetch key_revoked -X POST -H "$AUTH" -H "$JSON" -d '{"revoke_devices":false}' \
    "$URL/v1/authkeys/$(field key id)/revoke"
  keep authkey_revoke_response.json key_revoked
  fetch device_revoked -X POST -H "$AUTH" "$URL/v1/devices/$(field approve id)/revoke"
  keep device_revoke_response.json device_revoked
fi

# The merge queue, from the version that added it (0.4.2). It needs a
# server with merging on and a worker enrolled, so it runs a second one on
# a scratch database of its own, with a `claude` that does not exist: no
# merge runs here, and none is needed, since the queue is what is being
# captured. The worker signs with openssl and RFC 9421's published test
# key, as device_me_response above does.
if curl -s "$URL/.well-known/recall" | grep -q '"merge_queue"' && command -v openssl >/dev/null 2>&1; then
  JPORT=8934
  JURL="http://127.0.0.1:$JPORT"
  RECALL_TOKEN=fixture-token RECALL_PORT="$JPORT" RECALL_DB_PATH="$WORK/jobs.sqlite" \
    RECALL_MERGE_ENABLED=true RECALL_CLAUDE_BIN="$WORK/no-claude-here" RECALL_GIT_COMMIT=abc1234 \
    "$BIN" >"$WORK/jobs.log" 2>&1 &
  JOBS_SERVER=$!
  trap 'kill "$JOBS_SERVER" 2>/dev/null; cleanup' EXIT
  for _ in $(seq 1 40); do
    curl -sf "$JURL/health" >/dev/null 2>&1 && break
    sleep 0.25
  done
  printf '%s\n' '-----BEGIN PRIVATE KEY-----' \
    'MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF' \
    '-----END PRIVATE KEY-----' >"$WORK/key.pem"
  # A server refuses signatures dated in its first five seconds, since the
  # process before it may have accepted them; the worker signs below.
  sleep 6

  fetch wenroll -X POST -H "$JSON" \
    -d "{\"name\":\"worker\",\"public_key\":\"$KEY\",\"agent\":\"recall-worker/$VERSION (linux-x86_64)\"}" \
    "$JURL/v1/devices/enroll"
  fetch wapprove -X POST -H "$AUTH" -H "$JSON" \
    -d "{\"user_code\":\"$(field wenroll user_code)\",\"scope\":\"worker\"}" \
    "$JURL/v1/devices/approve"
  WORKER_ID=$(field wapprove id)

  # signed <name> <path> <body>: POSTs <body> to <path> signed as the
  # worker, keeping the answer as fetch does.
  NONCE=0
  signed() {
    local name=$1 path=$2 body=$3 created digest params sig
    NONCE=$((NONCE + 1))
    created=$(date +%s)
    digest="sha-256=:$(printf '%s' "$body" | openssl dgst -sha256 -binary | base64):"
    params="(\"@method\" \"@authority\" \"@path\" \"@query\" \"content-digest\" \"recall-protocol\");created=$created;keyid=\"$WORKER_ID\";nonce=\"capture-$created-$NONCE\";alg=\"ed25519\""
    printf '"@method": POST\n"@authority": 127.0.0.1:%s\n"@path": %s\n"@query": ?\n"content-digest": %s\n"recall-protocol": 1\n"@signature-params": %s' \
      "$JPORT" "$path" "$digest" "$params" >"$WORK/base"
    sig=$(openssl pkeyutl -sign -inkey "$WORK/key.pem" -rawin -in "$WORK/base" | base64 | tr -d '\n')
    fetch "$name" -X POST -H "$JSON" -H "Recall-Protocol: 1" -H "Content-Digest: $digest" \
      -H "Signature-Input: sig1=$params" -H "Signature: sig1=:$sig:" \
      -H "User-Agent: recall-worker/$VERSION (linux-x86_64)" \
      --data-binary "$body" "$JURL$path"
  }

  # A conflict: the stored version, then a push from an older base.
  curl -s -o /dev/null -X POST -H "$AUTH" -H "$JSON" \
    -d '{"project_key":"acme/app","file_path":"topics/auth.md","content":"# Auth\n- tokens live in 1Password\n","source_env":"laptop"}' \
    "$JURL/sync"
  fetch queued -X POST -H "$AUTH" -H "$JSON" \
    -d '{"project_key":"acme/app","file_path":"topics/auth.md","content":"# Auth\n- rotate them monthly\n","source_env":"cloud","base_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"}' \
    "$JURL/sync"
  keep push_response_queued.json queued

  CLI='"claude_cli":{"checked_at":"2026-10-02T09:13:40.002Z","available":true,"logged_in":true,"error":""}'
  signed claim /v1/jobs/claim "{\"kinds\":[\"merge\"],\"wait_seconds\":0,\"lease_seconds\":120,$CLI}"
  keep job_claim_response.json claim
  signed empty /v1/jobs/claim "{\"kinds\":[\"merge\"],\"wait_seconds\":0,\"lease_seconds\":120,$CLI}"
  keep job_claim_response_empty.json empty
  JOB=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["job"]["id"])' "$WORK/claim")
  LEASE=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["job"]["lease_id"])' "$WORK/claim")

  # While the job is held: what /health shows with a worker enrolled.
  if [ -n "${HEALTH_CAPTURED_HERE:-}" ]; then
    rm -f "$OUT/health.json"
    save health.json "$JURL/health"
  fi

  signed result "/v1/jobs/$JOB/result" \
    "{\"lease_id\":\"$LEASE\",\"merge\":{\"content\":\"# Auth\\n- tokens live in 1Password\\n- rotate them monthly\\n\"}}"
  keep job_result_response.json result
  fetch jobs -H "$AUTH" "$JURL/v1/jobs"
  keep job_list_response.json jobs
fi

# Evaluation reports, from the version that added them (0.4.5): on the
# queue's server, with its worker, one run asked for, claimed and
# reported, each response a real one. The report names the file the
# conflict above wrote; its details are what a worker writes.
if [ -n "${WORKER_ID:-}" ] && curl -s "$JURL/.well-known/recall" | grep -q '"evaluation"'; then
  fetch evaluation -X POST -H "$AUTH" -H "$JSON" -d '{"projects":["acme/app"]}' \
    "$JURL/v1/evaluations"
  keep evaluation_created_response.json evaluation
  EVAL=$(field evaluation id)
  signed eclaim /v1/jobs/claim "{\"kinds\":[\"evaluate\"],\"wait_seconds\":0,\"lease_seconds\":120,$CLI}"
  keep job_claim_response_evaluate.json eclaim
  EJOB=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["job"]["id"])' "$WORK/eclaim")
  ELEASE=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["job"]["lease_id"])' "$WORK/eclaim")
  signed ereport "/v1/jobs/$EJOB/result" \
    "{\"lease_id\":\"$ELEASE\",\"evaluate\":{\"findings\":[{\"id\":\"f1\",\"kind\":\"stale\",\"severity\":\"low\",\"project_key\":\"acme/app\",\"file_path\":\"topics/auth.md\",\"lines\":[2,2],\"related\":[]}],\"details\":{\"findings\":{\"f1\":{\"excerpt\":\"- tokens live in 1Password\\n\",\"reasoning\":\"Unchanged since 2026-01-02 (120 days), and it names paths or commands.\",\"suggested_edit\":null}},\"skipped\":[]}}}"
  fetch evaluations -H "$AUTH" "$JURL/v1/evaluations"
  keep evaluation_list_response.json evaluations
  fetch shown -H "$AUTH" "$JURL/v1/evaluations/$EVAL"
  keep evaluation_response.json shown
fi
