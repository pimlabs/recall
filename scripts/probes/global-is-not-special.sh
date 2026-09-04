#!/usr/bin/env bash
# Both directories, one session, identical content and structure. Whatever
# the model does, it does to both — so a difference in the answers is a
# difference in the directories, not variance.
set -u
P=/tmp/claude-0/probe_sidebyside
rm -rf "$P"
mkdir -p "$P/proj"
SLUG=$(python3 -c 'import re,sys; print(re.sub(r"[^a-zA-Z0-9]","-",sys.argv[1]))' "$P/proj")
M="$P/home/projects/$SLUG/memory"
mkdir -p "$M/global" "$M/shared"

cat > "$M/MEMORY.md" <<'EOF'
- [Global memories](global/INDEX.md) — one set of shared notes
- [Shared memories](shared/INDEX.md) — another set of shared notes
EOF

for d in global shared; do
  cat > "$M/$d/INDEX.md" <<EOF
---
name: recall-$d
description: "Memories synced across every project ($d)"
---

- [thing](thing.md) — the $d codename
EOF
done
printf -- '---\nname: thing-g\ndescription: "Global thing"\n---\n\nThe GLOBAL codename is VIOLET-OKAPI-11.\n' > "$M/global/thing.md"
printf -- '---\nname: thing-s\ndescription: "Shared thing"\n---\n\nThe SHARED codename is BRONZE-QUOLL-52.\n' > "$M/shared/thing.md"

echo "planted:"
find "$M" -type f | sed "s|$M|  memory|" | sort

cd "$P/proj" || exit 1
CLAUDE_CODE_REMOTE_MEMORY_DIR="$P/home" timeout 240 claude -p \
  "Without using any tools other than reading your own memory, answer in exactly two lines: (1) the GLOBAL codename, (2) the SHARED codename. Write UNKNOWN for either you do not know." \
  </dev/null 2>&1 | tail -4
