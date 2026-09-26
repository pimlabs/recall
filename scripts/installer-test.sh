#!/usr/bin/env bash
# The installers against a release served from loopback, before any real
# release exists to test them against.
#
#   ./scripts/installer-test.sh
#
# A release's archive names are a contract between release.yml, which
# writes them, and everything that downloads them: install.sh,
# npm/install.js, deploy/fetch-release.sh (and through it the server image),
# install.ps1 (scripts/installer-test.ps1, on Windows), the Homebrew
# formula and the winget manifest. A mismatch used to surface only at the
# next real release. So this builds two releases the way release.yml does:
#
#   v0.4.6  the names and layout every release after v0.4.5 has, packed by
#           scripts/package-release.py, the script release.yml itself runs,
#           under the names release.yml's own matrix gives
#   v0.4.5  the names and layout v0.4.5 and older have, packed the way
#           release.yml packed them then
#
# Each holds a tiny fake binary per archive that prints which archive it
# came from, plus a checksums.txt made by the same command release.yml's
# publish job runs. scripts/fake-release-server.py serves them on loopback
# at GitHub's URLs, and each installer is pointed at it with
# RECALL_TEST_RELEASES_URL (fetch-release.sh takes the URL as an argument),
# then asked for "latest" and for pinned versions on both sides of the
# cutoff. Every install is checked by running what it installed, and by
# what that binary says: the right archive for this machine, from the right
# release. Then the refusals: a wrong checksum, and a release whose
# checksums.txt does not list the archive.
#
# Needs bash, python3 (with PyYAML), node, curl, tar and sha256sum. No
# network beyond 127.0.0.1, no Rust toolchain.
set -euo pipefail
cd "$(dirname "$0")/.."

# Nothing from the calling shell may steer an installer anywhere real.
unset RECALL_URL RECALL_TOKEN RECALL_PROJECT_KEY RECALL_AUTHKEY RECALL_WORKER_SERVER \
  RECALL_VERSION RECALL_BIN_DIR RECALL_TEST_RELEASES_URL

NEW=0.4.6 # the first release with the new names
OLD=0.4.5 # the last release with the old ones

WORK="$(mktemp -d)"
SERVER=""
cleanup() {
  if [ -n "$SERVER" ]; then kill "$SERVER" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

pass=0
fail=0
# check <what> <command...>: the command must succeed.
check() {
  local what=$1
  shift
  if "$@"; then
    pass=$((pass + 1))
    echo "ok    $what"
  else
    fail=$((fail + 1))
    echo "FAIL  $what"
  fi
}
# fails <command...>: the command must fail.
fails() { ! "$@"; }
# says <expected> <command...>: the command's whole output is <expected>.
says() {
  local want=$1 got
  shift
  got="$("$@" 2>&1)" || true
  [ "$got" = "$want" ] || { echo "      expected: $want"; echo "      got:      $got"; return 1; }
}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) HOST=x86_64-unknown-linux-gnu OLD_HOST=linux_amd64 ;;
  Linux-aarch64 | Linux-arm64) HOST=aarch64-unknown-linux-gnu OLD_HOST=linux_arm64 ;;
  Darwin-x86_64) HOST=x86_64-apple-darwin OLD_HOST=darwin_amd64 ;;
  Darwin-arm64) HOST=aarch64-apple-darwin OLD_HOST=darwin_arm64 ;;
  *) echo "installer-test: no release archive for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# A fake binary that says which archive it is: `<name> <version> <where>`,
# whatever it is asked.
fake() { # path, what it prints
  mkdir -p "$(dirname "$1")"
  printf '#!/bin/sh\necho "%s"\n' "$2" >"$1"
  chmod 755 "$1"
}

# What release.yml publishes, read from release.yml: its client matrix
# (asset, and whether the runner is Windows) and its server targets.
python3 - .github/workflows/release.yml >"$WORK/release-names" <<'PY'
import sys, yaml
jobs = yaml.safe_load(open(sys.argv[1]))["jobs"]
for e in jobs["build"]["strategy"]["matrix"]["include"]:
    ext = "zip" if e["os"].startswith("windows") else "tar.gz"
    print("client", e["target"], e["asset"], ext)
for e in jobs["build-server"]["strategy"]["matrix"]["include"]:
    print("server", e["target"], "-", "tar.gz")
PY

echo "== the release names, from release.yml"
# Every consumer builds a name as recall-<target>; the matrix must agree.
while read -r kind target asset _; do
  if [ "$kind" = client ]; then
    check "release.yml names $target's archive recall-$target" test "$asset" = "recall-$target"
  fi
done <"$WORK/release-names"

# ---- v0.4.6: packed by the script release.yml runs --------------------------
rel_new="$WORK/root/releases/download/v$NEW"
mkdir -p "$rel_new"
while read -r kind target asset ext; do
  if [ "$kind" = client ]; then
    exe=recall
    [ "$ext" = zip ] && exe=recall.exe
    fake "$WORK/build/$target/$exe" "recall $NEW $target"
    python3 scripts/package-release.py "$WORK/build/$target/$exe" "$asset.$ext" "$rel_new" >/dev/null
  else
    for name in recall-server recall-worker; do
      fake "$WORK/build/$target/$name" "$name $NEW $target"
      python3 scripts/package-release.py "$WORK/build/$target/$name" "$name-$target.tar.gz" "$rel_new" >/dev/null
    done
  fi
done <"$WORK/release-names"
# The same command release.yml's publish job runs.
# shellcheck disable=SC2035 # every name here starts with "recall"
(cd "$rel_new" && sha256sum *.tar.gz *.zip >checksums.txt)

echo "== the v$NEW layout"
check "checksums.txt lists all ten archives" test "$(wc -l <"$rel_new/checksums.txt")" -eq 10
check "the client tar.gz holds one directory: recall, LICENSE, README.md" \
  says "$(printf '%s\n' "recall-$HOST/" "recall-$HOST/recall" "recall-$HOST/LICENSE" "recall-$HOST/README.md")" \
  tar -tzf "$rel_new/recall-$HOST.tar.gz"
check "the binary in it is executable" \
  bash -c "tar -tvzf '$rel_new/recall-$HOST.tar.gz' | grep -q '^-rwxr-xr-x .* recall-$HOST/recall\$'"
check "the Windows zip holds one directory: recall.exe, LICENSE, README.md" \
  says "$(printf '%s\n' recall-x86_64-pc-windows-msvc/ recall-x86_64-pc-windows-msvc/recall.exe \
    recall-x86_64-pc-windows-msvc/LICENSE recall-x86_64-pc-windows-msvc/README.md)" \
  python3 -c 'import sys, zipfile; print("\n".join(zipfile.ZipFile(sys.argv[1]).namelist()))' \
  "$rel_new/recall-x86_64-pc-windows-msvc.zip"
check "the server tar.gz holds recall-server-<target>/recall-server" \
  bash -c "tar -tzf '$rel_new/recall-server-x86_64-unknown-linux-musl.tar.gz' | grep -qx 'recall-server-x86_64-unknown-linux-musl/recall-server'"

# Every other consumer's per-platform names are in that release. Only this
# machine's archive is installed below; this catches a typo in the others.
python3 - "$rel_new/checksums.txt" >"$WORK/consumers" <<'PY'
import re, sys
published = {line.split()[1] for line in open(sys.argv[1])}
def triples(path, pattern):
    return set(re.findall(pattern, open(path).read()))
wanted = {
    "install.sh": {f"recall-{t}.tar.gz" for t in triples("install.sh", r'target="([a-z0-9_-]+)"')},
    "install.ps1": {f"recall-{t}.zip" for t in triples("install.ps1", r'\$target = "([a-z0-9_-]+)"')},
    "npm/install.js": {f"recall-{t}.{e}" for t, e in re.findall(r'target: "([a-z0-9_-]+)", old: "[a-z0-9_]+", ext: "([a-z.]+)"', open("npm/install.js").read())},
    "deploy/fetch-release.sh": {f"{b}-{t}.tar.gz" for t in triples("deploy/fetch-release.sh", r'target=([a-z0-9_-]+-linux-musl)') for b in ("recall-server", "recall-worker")},
}
for who, names in wanted.items():
    missing = sorted(names - published)
    print(who, len(names), " ".join(missing) or "-")
PY
while read -r who count missing; do
  check "$who: its $count archive names are all in the release" test "$missing" = "-"
done <"$WORK/consumers"

# ---- v0.4.5: packed the way release.yml packed it then ----------------------
rel_old="$WORK/root/releases/download/v$OLD"
mkdir -p "$rel_old"
for p in darwin_amd64 darwin_arm64 linux_amd64 linux_arm64; do
  fake "$rel_old/recall_$p" "recall $OLD $p"
  tar -czf "$rel_old/recall_$p.tar.gz" -C "$rel_old" "recall_$p"
  rm "$rel_old/recall_$p"
done
for p in windows_amd64 windows_arm64; do
  fake "$WORK/old-$p/recall.exe" "recall $OLD $p"
  python3 -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1], "w").write(sys.argv[2], "recall.exe")' \
    "$rel_old/recall_$p.zip" "$WORK/old-$p/recall.exe"
done
for name in recall-server recall-worker; do
  for arch in amd64 arm64; do
    fake "$rel_old/${name}_linux_$arch" "$name $OLD linux_$arch"
    tar -czf "$rel_old/${name}_linux_$arch.tar.gz" -C "$rel_old" "${name}_linux_$arch"
    rm "$rel_old/${name}_linux_$arch"
  done
done
# shellcheck disable=SC2035
(cd "$rel_old" && sha256sum *.tar.gz *.zip >checksums.txt)

# ---- two broken releases, to be refused -------------------------------------
# v0.4.7: every archive of v0.4.6, and a checksums.txt whose digests are all
# wrong. v0.4.8: the same archives, and a checksums.txt listing none of the
# client's.
for v in 0.4.7 0.4.8; do
  mkdir -p "$WORK/root/releases/download/v$v"
  cp "$rel_new"/*.tar.gz "$rel_new"/*.zip "$WORK/root/releases/download/v$v/"
done
sed 's/^[0-9a-f]*/'"$(printf '0%.0s' $(seq 64))"'/' "$rel_new/checksums.txt" \
  >"$WORK/root/releases/download/v0.4.7/checksums.txt"
grep -v ' recall-[xa]' "$rel_new/checksums.txt" >"$WORK/root/releases/download/v0.4.8/checksums.txt"

# ---- serve it ---------------------------------------------------------------
echo "$NEW" >"$WORK/root/LATEST"
python3 scripts/fake-release-server.py "$WORK/root" "$WORK/port" 2>"$WORK/server.log" &
SERVER=$!
for _ in $(seq 1 50); do
  [ -s "$WORK/port" ] && break
  sleep 0.1
done
[ -s "$WORK/port" ] || { echo "installer-test: the fake release server did not start" >&2; cat "$WORK/server.log" >&2; exit 1; }
RELEASES="http://127.0.0.1:$(cat "$WORK/port")/releases"
echo "== serving $RELEASES"

# ---- install.sh ---------------------------------------------------------------
# install_sh <dir> [RECALL_VERSION]: runs install.sh into <dir>.
install_sh() {
  local dir=$1
  shift
  if [ $# -gt 0 ]; then
    RECALL_TEST_RELEASES_URL="$RELEASES" RECALL_BIN_DIR="$dir" RECALL_VERSION="$1" \
      bash install.sh >"$dir.log" 2>&1
  else
    RECALL_TEST_RELEASES_URL="$RELEASES" RECALL_BIN_DIR="$dir" bash install.sh >"$dir.log" 2>&1
  fi
}

echo "== install.sh"
echo "$NEW" >"$WORK/root/LATEST"
check "latest ($NEW): installs" install_sh "$WORK/sh-latest-new"
check "latest ($NEW): the binary is recall-$HOST's" \
  says "recall $NEW $HOST" "$WORK/sh-latest-new/recall" version

# Until 0.4.6 ships, latest is 0.4.5, and install.sh is served from main:
# it has to find the old name for it.
echo "$OLD" >"$WORK/root/LATEST"
check "latest ($OLD): installs" install_sh "$WORK/sh-latest-old"
check "latest ($OLD): the binary is recall_$OLD_HOST's" \
  says "recall $OLD $OLD_HOST" "$WORK/sh-latest-old/recall" version

check "pinned v$NEW (latest $OLD): installs" install_sh "$WORK/sh-pin-new" "v$NEW"
check "pinned v$NEW: the binary is recall-$HOST's" \
  says "recall $NEW $HOST" "$WORK/sh-pin-new/recall" version

echo "$NEW" >"$WORK/root/LATEST"
check "pinned $OLD, without the v (latest $NEW): installs" install_sh "$WORK/sh-pin-old" "$OLD"
check "pinned $OLD: the binary is recall_$OLD_HOST's" \
  says "recall $OLD $OLD_HOST" "$WORK/sh-pin-old/recall" version

check "a wrong checksum is refused" fails install_sh "$WORK/sh-bad" v0.4.7
check "  ... saying so" grep -q 'checksum mismatch' "$WORK/sh-bad.log"
check "  ... and installs nothing" test ! -e "$WORK/sh-bad/recall"
check "an archive checksums.txt does not list is refused" fails install_sh "$WORK/sh-unlisted" v0.4.8
check "  ... saying so" grep -q 'checksums.txt has no line for' "$WORK/sh-unlisted.log"
check "  ... and installs nothing" test ! -e "$WORK/sh-unlisted/recall"

# ---- npm/install.js -----------------------------------------------------------
# npm_install <dir> <version>: a copy of npm/ at <version>, its postinstall run.
npm_install() {
  rm -rf "$1"
  cp -R npm "$1"
  sed -i.bak 's/"version": "[^"]*"/"version": "'"$2"'"/' "$1/package.json"
  (cd "$1" && RECALL_TEST_RELEASES_URL="$RELEASES" node install.js) >"$1.log" 2>&1
}

echo "== npm/install.js"
check "npm at $NEW: installs" npm_install "$WORK/npm-new" "$NEW"
check "npm at $NEW: bin/recall is recall-$HOST's binary" \
  says "recall $NEW $HOST" "$WORK/npm-new/bin/recall" version
check "npm at $NEW: nothing left behind from unpacking" \
  test -z "$(find "$WORK/npm-new" -name '.recall-extract-*' -o -name "recall-$HOST" | head -1)"
check "npm at $OLD: installs" npm_install "$WORK/npm-old" "$OLD"
check "npm at $OLD: bin/recall is recall_$OLD_HOST's binary" \
  says "recall $OLD $OLD_HOST" "$WORK/npm-old/bin/recall" version
check "npm with a wrong checksum is refused" fails npm_install "$WORK/npm-bad" 0.4.7
check "  ... saying so" grep -q 'checksum mismatch' "$WORK/npm-bad.log"
check "  ... and leaves the shim, not a binary" grep -q '^#!/usr/bin/env node' "$WORK/npm-bad/bin/recall"

# ---- deploy/fetch-release.sh --------------------------------------------------
# fetch <version> <arch> <binary>: what deploy/Dockerfile's release stages
# run, into $WORK/fetch/<version>-<arch>/<binary>.
fetch() {
  sh deploy/fetch-release.sh "$1" "$2" "$WORK/fetch/$1-$2/$3" "$RELEASES/download" "$3" \
    >"$WORK/fetch-$1-$2-$3.log" 2>&1
}

echo "== deploy/fetch-release.sh"
for arch in amd64 arm64; do
  case $arch in
    amd64) musl=x86_64-unknown-linux-musl ;;
    arm64) musl=aarch64-unknown-linux-musl ;;
  esac
  for name in recall-server recall-worker; do
    check "$name $NEW for $arch: fetched and checked" fetch "$NEW" "$arch" "$name"
    check "$name $NEW for $arch: it is $name-$musl's" \
      says "$name $NEW $musl" "$WORK/fetch/$NEW-$arch/$name" version
    check "$name $OLD for $arch (a rollback): fetched and checked" fetch "$OLD" "$arch" "$name"
    check "$name $OLD for $arch: it is ${name}_linux_$arch's" \
      says "$name $OLD linux_$arch" "$WORK/fetch/$OLD-$arch/$name" version
  done
done
check "a wrong checksum is refused" fails fetch 0.4.7 amd64 recall-server
check "  ... and installs nothing" test ! -e "$WORK/fetch/0.4.7-amd64/recall-server"
check "a version that is not one is refused" fails fetch latest amd64 recall-server
check "  ... saying so" grep -q 'latest is not a release version' "$WORK/fetch-latest-amd64-recall-server.log"

echo "passed $pass, failed $fail"
if [ "$fail" -ne 0 ]; then
  for log in "$WORK"/*.log; do
    echo "---- $log"
    cat "$log"
  done
fi
[ "$fail" -eq 0 ]
