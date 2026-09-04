#!/usr/bin/env bash
# probe_gitrepo, with one variable changed: the linked file sits in a
# subdirectory instead of at the root of the memory directory.
#
#   worked:  MEMORY.md -> editor.md
#   testing: MEMORY.md -> sub/editor.md
set -u
P=/tmp/claude-0/probe_subdir
rm -rf "$P"
mkdir -p "$P/proj"
SLUG=$(python3 -c 'import re,sys; print(re.sub(r"[^a-zA-Z0-9]","-",sys.argv[1]))' "$P/proj")
M="$P/home/projects/$SLUG/memory"
mkdir -p "$M/sub"
printf -- '- [Editor preference](sub/editor.md) — editor codename\n' > "$M/MEMORY.md"
printf -- '---\nname: editor\ndescription: "Preferred editor"\n---\n\nThe editor codename is SLATE-VIPER-96.\n' > "$M/sub/editor.md"

cd "$P/proj" || exit 1
CLAUDE_CODE_REMOTE_MEMORY_DIR="$P/home" timeout 240 claude -p \
  "Without using any tools other than reading your own memory, reply with just the editor codename, or UNKNOWN." \
  </dev/null 2>&1 | tail -3
