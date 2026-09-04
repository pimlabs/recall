#!/usr/bin/env bash
# Is a memory directory that Claude Code did not create itself read at all?
#
# This isolates the question Recall depends on entirely: it writes memory
# files out-of-band, and if the CLI only loads files it knows about from its
# own index, nothing Recall does is ever seen.
set -u
P=/tmp/claude-0/probe_handmade
rm -rf "$P"
mkdir -p "$P/proj"
SLUG=$(python3 -c 'import re,sys; print(re.sub(r"[^a-zA-Z0-9]","-",sys.argv[1]))' "$P/proj")
M="$P/home/projects/$SLUG/memory"
mkdir -p "$M"

printf -- '- [Editor preference](editor.md) — editor codename\n' > "$M/MEMORY.md"
printf -- '---\nname: editor\ndescription: "Preferred editor"\n---\n\nThe editor codename is AMBER-LYNX-31.\n' > "$M/editor.md"

echo "planted:"
find "$M" -type f | sed "s|$M|  memory|"

cd "$P/proj" || exit 1
CLAUDE_CODE_REMOTE_MEMORY_DIR="$P/home" timeout 240 claude -p \
  "Without using any tools other than reading your own memory, reply with just the editor codename, or UNKNOWN." \
  </dev/null 2>&1 | tail -3
