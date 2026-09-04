#!/usr/bin/env bash
# End-to-end: does a global memory pushed from "machine A" reach a *different*
# project on "machine B", and does Claude Code actually read it there?
set -uo pipefail
BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")  # the script cd's about
WORK=$(mktemp -d)
PORT=8941
URL="http://127.0.0.1:$PORT"
TOKEN="e2e-token"

RECALL_TOKEN="$TOKEN" RECALL_PORT="$PORT" RECALL_DB_PATH="$WORK/db.sqlite" \
  RECALL_MERGE_ENABLED=false "$BIN" serve >"$WORK/server.log" 2>&1 &
SERVER=$!
trap 'kill $SERVER 2>/dev/null; rm -rf "$WORK"' EXIT
for _ in $(seq 1 40); do curl -sf "$URL/health" >/dev/null 2>&1 && break; sleep 0.25; done

# --- machine A, project alpha -------------------------------------------
mkdir -p "$WORK/a/alpha" "$WORK/a/home"
git -C "$WORK/a/alpha" init -q
git -C "$WORK/a/alpha" remote add origin git@github.com:acme/alpha.git
MEM_A="$WORK/a/home/projects/$(python3 -c "
import sys,re
print(re.sub(r'[^a-zA-Z0-9]','-',sys.argv[1]))" "$WORK/a/alpha")/memory"
mkdir -p "$MEM_A/global"
cat > "$MEM_A/global/editor.md" <<'EOF'
---
name: editor-pref
description: "Preferred editor"
---

The user's preferred editor codename is CRIMSON-FALCON-77.
EOF

env -i PATH="$PATH" HOME="$WORK/a/home" \
  RECALL_URL="$URL" RECALL_TOKEN="$TOKEN" RECALL_SOURCE_ENV=machine-a \
  RECALL_GLOBAL_KEY=eko CLAUDE_CONFIG_DIR="$WORK/a/home" \
  sh -c "cd '$WORK/a/alpha' && echo '{\"tool_input\":{\"file_path\":\"$MEM_A/global/editor.md\"}}' | '$BIN' push" 2>&1 | sed 's/^/  A push: /'

echo "  server now holds:"
curl -s -H "Authorization: Bearer $TOKEN" -G "$URL/sync" \
  --data-urlencode "project_key=global:eko" | python3 -m json.tool | sed 's/^/    /'

# --- machine B, a DIFFERENT project -------------------------------------
mkdir -p "$WORK/b/beta" "$WORK/b/home"
git -C "$WORK/b/beta" init -q
git -C "$WORK/b/beta" remote add origin git@github.com:acme/beta.git
MEM_B="$WORK/b/home/projects/$(python3 -c "
import sys,re
print(re.sub(r'[^a-zA-Z0-9]','-',sys.argv[1]))" "$WORK/b/beta")/memory"

env -i PATH="$PATH" HOME="$WORK/b/home" \
  RECALL_URL="$URL" RECALL_TOKEN="$TOKEN" RECALL_SOURCE_ENV=machine-b \
  RECALL_GLOBAL_KEY=eko CLAUDE_CONFIG_DIR="$WORK/b/home" \
  sh -c "cd '$WORK/b/beta' && '$BIN' pull" 2>&1 | sed 's/^/  B pull: /'

echo "  B memory tree:"
find "$MEM_B" -type f 2>/dev/null | sed "s|$MEM_B|    memory|"
echo "  B MEMORY.md:"
sed 's/^/    /' "$MEM_B/MEMORY.md" 2>/dev/null

# --- the real question: does Claude read it in project beta? ------------
echo
echo "  asking Claude, in project beta, with machine B's memory:"
(cd "$WORK/b/beta" && CLAUDE_CODE_REMOTE_MEMORY_DIR="$WORK/b/home" \
  timeout 240 claude -p "Without using any tools other than reading your own memory, reply with just the editor codename, or UNKNOWN." 2>&1 | sed 's/^/    /')
echo
echo "  Expect the codename. A single UNKNOWN is not a failure — retrieval is"
echo "  probabilistic; see docs/memory-loading-findings.md. Run this a few times."
