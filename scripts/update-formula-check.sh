#!/usr/bin/env bash
# scripts/update-formula.py, checked without a network or a release.
#
# The committed Formula/recall.rb was written by a real release, so its own
# url/sha256 pairs are a valid checksums.txt. Blank every hash and the
# version, rebuild from those pairs, and the result must be byte-identical.
# Then the two ways it must refuse: a platform missing from checksums.txt,
# and — the bug this caught while being written — refusing must leave the
# formula as it was, not truncated.
set -euo pipefail
cd "$(dirname "$0")/.."

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
pass=0; fail=0
check() { if "$@"; then pass=$((pass+1)); else echo "FAIL: $*"; fail=$((fail+1)); fi; }

version=$(grep -m1 '^  version "' Formula/recall.rb | cut -d'"' -f2)

# checksums.txt, in the release's own format, from the formula's pairs.
python3 - Formula/recall.rb >"$WORK/checksums.txt" <<'PY'
import re, sys
s = open(sys.argv[1]).read()
for name, digest in re.findall(r'url "[^"]*/([^/"]+)"\n\s*sha256 "([a-f0-9]+)"', s):
    print(f"{digest}  {name}")
PY
check test "$(wc -l <"$WORK/checksums.txt")" -eq 4

sed -e 's/sha256 "[a-f0-9]*"/sha256 ""/' -e "s/^  version \"$version\"/  version \"0.0.0\"/" \
  Formula/recall.rb >"$WORK/blank.rb"
check test "$(grep -c 'sha256 ""' "$WORK/blank.rb")" -eq 4

python3 scripts/update-formula.py "$version" "$WORK/checksums.txt" "$WORK/blank.rb" >/dev/null
check cmp -s "$WORK/blank.rb" Formula/recall.rb

# Pairing, not position: swap two lines of checksums.txt and the output must
# not change.
{ sed -n 2p "$WORK/checksums.txt"; sed -n 1p "$WORK/checksums.txt"; sed -n '3,$p' "$WORK/checksums.txt"; } \
  >"$WORK/shuffled.txt"
cp "$WORK/blank.rb" "$WORK/again.rb"
sed -i -e 's/sha256 "[a-f0-9]*"/sha256 ""/' "$WORK/again.rb"
python3 scripts/update-formula.py "$version" "$WORK/shuffled.txt" "$WORK/again.rb" >/dev/null
check cmp -s "$WORK/again.rb" Formula/recall.rb

# A platform missing: refused, and the formula untouched.
grep -v linux_arm64 "$WORK/checksums.txt" >"$WORK/short.txt"
cp Formula/recall.rb "$WORK/keep.rb"
check bash -c "! python3 scripts/update-formula.py '$version' '$WORK/short.txt' '$WORK/keep.rb' 2>/dev/null"
check cmp -s "$WORK/keep.rb" Formula/recall.rb

echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
