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

# Across the rename. v0.4.5 is the last release with recall_<os>_<arch>
# archives; from 0.4.6 they are recall-<rust target>, holding a directory.
# A 0.4.6 checksums.txt carries only the new names (and the old names'
# absence must not matter), and the formula has to come out naming them,
# each with its own hash, with the install stanza for the new layout.
new_names="recall-aarch64-apple-darwin recall-x86_64-apple-darwin recall-aarch64-unknown-linux-gnu recall-x86_64-unknown-linux-gnu"
i=0
for n in $new_names; do
  i=$((i + 1))
  printf '%064d  %s.tar.gz\n' "$i" "$n"
done >"$WORK/new.txt"
printf '%064d  recall-x86_64-pc-windows-msvc.zip\n' 9 >>"$WORK/new.txt"
cp Formula/recall.rb "$WORK/new.rb"
python3 scripts/update-formula.py 0.4.6 "$WORK/new.txt" "$WORK/new.rb" >/dev/null
check grep -q '^  version "0.4.6"$' "$WORK/new.rb"
check test "$(grep -c 'recall_' "$WORK/new.rb")" -eq 0
i=0
for n in $new_names; do
  i=$((i + 1))
  check grep -Eq "releases/download/v#\{version\}/$n\.tar\.gz\"\$" "$WORK/new.rb"
  check python3 - "$WORK/new.rb" "$n" "$(printf '%064d' "$i")" <<'PY'
import re, sys
s, name, digest = open(sys.argv[1]).read(), sys.argv[2], sys.argv[3]
m = re.search(r'/' + re.escape(name) + r'\.tar\.gz"\n\s*sha256 "([a-f0-9]+)"', s)
sys.exit(0 if m and m.group(1) == digest else 1)
PY
done
check grep -q '^      bin.install "recall"$' "$WORK/new.rb"
check test "$(grep -c 'Dir\["recall_\*"\]' "$WORK/new.rb")" -eq 0
if command -v ruby >/dev/null; then
  check ruby -c "$WORK/new.rb"
fi

# An old release's checksums.txt against a formula already at the new
# names: every name and the stanza go back, byte for byte. The rewrite is
# decided by the version, not by what the formula last said.
python3 scripts/update-formula.py "$version" "$WORK/checksums.txt" "$WORK/new.rb" >/dev/null
check cmp -s "$WORK/new.rb" Formula/recall.rb

# A 0.4.6 checksums.txt with only the old names is a release that did not
# rename its archives: refused, naming a new one.
cp Formula/recall.rb "$WORK/keep.rb"
check bash -c "python3 scripts/update-formula.py 0.4.6 '$WORK/checksums.txt' '$WORK/keep.rb' 2>&1 | grep -q recall-aarch64-apple-darwin.tar.gz"
check cmp -s "$WORK/keep.rb" Formula/recall.rb

echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
