#!/usr/bin/env bash
# Inside a git repository — as every real project is — is a linked file in a
# subdirectory of the memory directory read as readily as one at the root?
#
# Both files, one session, so the comparison is within-run and fair.
#
#   MEMORY.md -> root.md          (root of the memory directory)
#   MEMORY.md -> global/sub.md    (subdirectory, as the global scope uses)
set -u
P=/tmp/claude-0/probe_subdir_git
rm -rf "$P"
mkdir -p "$P/proj"
git -C "$P/proj" init -q
git -C "$P/proj" remote add origin git@github.com:acme/beta.git

SLUG=$(python3 -c 'import re,sys; print(re.sub(r"[^a-zA-Z0-9]","-",sys.argv[1]))' "$P/proj")
M="$P/home/projects/$SLUG/memory"
mkdir -p "$M/global"

cat > "$M/MEMORY.md" <<'EOF'
- [root note](root.md) — the ROOT codename
- [sub note](global/sub.md) — the SUB codename
EOF
printf -- '---\nname: root-note\ndescription: "The ROOT codename"\n---\n\nThe ROOT codename is MAROON-EGRET-19.\n' > "$M/root.md"
printf -- '---\nname: sub-note\ndescription: "The SUB codename"\n---\n\nThe SUB codename is JADE-CIVET-73.\n' > "$M/global/sub.md"

echo "planted:"
find "$M" -type f | sed "s|$M|  memory|" | sort

cd "$P/proj" || exit 1
CLAUDE_CODE_REMOTE_MEMORY_DIR="$P/home" timeout 240 claude -p \
  "Without using any tools other than reading your own memory, answer in exactly two lines: (1) the ROOT codename, (2) the SUB codename. Write UNKNOWN for either you do not know." \
  </dev/null 2>&1 | tail -4
