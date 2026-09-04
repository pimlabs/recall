#!/usr/bin/env bash
# The exact end-to-end configuration, hand-made: git repo, and a MEMORY.md
# whose only entry is the global link. No Recall, so no .recall-state.json
# and no atomic-write temp files.
#
# If this passes, the variable is something Recall leaves behind.
# If it fails, the variable is the configuration itself.
set -u
P=/tmp/claude-0/probe_onlyglobal
rm -rf "$P"
mkdir -p "$P/proj"
git -C "$P/proj" init -q
git -C "$P/proj" remote add origin git@github.com:acme/beta.git

SLUG=$(python3 -c 'import re,sys; print(re.sub(r"[^a-zA-Z0-9]","-",sys.argv[1]))' "$P/proj")
M="$P/home/projects/$SLUG/memory"
mkdir -p "$M/global"
printf -- '- [editor](global/editor.md) — Preferred editor\n' > "$M/MEMORY.md"
printf -- '---\nname: editor-pref\ndescription: "Preferred editor"\n---\n\nThe user preferred editor codename is CRIMSON-FALCON-77.\n' > "$M/global/editor.md"

cd "$P/proj" || exit 1
CLAUDE_CODE_REMOTE_MEMORY_DIR="$P/home" timeout 240 claude -p \
  "Without using any tools other than reading your own memory, reply with just the editor codename, or UNKNOWN." \
  </dev/null 2>&1 | tail -2
