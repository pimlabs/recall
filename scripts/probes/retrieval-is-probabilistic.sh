#!/usr/bin/env bash
# The exact files the e2e produced, asked two ways:
#   1. a question whose words do not appear in the MEMORY.md gloss
#   2. the same fact, asked the way a person actually would
#
# If (2) works and (1) does not, the index is a retrieval surface and my
# earlier probes were measuring my own question wording, not the structure.
set -u
run() {
  local name=$1 question=$2
  local P=/tmp/claude-0/probe_gloss_$name
  rm -rf "$P"; mkdir -p "$P/proj"
  local SLUG M
  SLUG=$(python3 -c 'import re,sys; print(re.sub(r"[^a-zA-Z0-9]","-",sys.argv[1]))' "$P/proj")
  M="$P/home/projects/$SLUG/memory"
  mkdir -p "$M/global"
  printf -- '- [editor](global/editor.md) — Preferred editor\n' > "$M/MEMORY.md"
  printf -- '---\nname: editor-pref\ndescription: "Preferred editor"\n---\n\nThe user preferred editor codename is CRIMSON-FALCON-77.\n' > "$M/global/editor.md"
  echo "--- $name"
  (cd "$P/proj" && CLAUDE_CODE_REMOTE_MEMORY_DIR="$P/home" timeout 240 claude -p "$question" </dev/null 2>&1 | tail -2)
}

run mismatched "Without using any tools other than reading your own memory, reply with just the editor codename, or UNKNOWN."
run natural "What is my preferred editor? Answer from memory in one short line."
