#!/usr/bin/env bash
#
# Checks deploy/backup-offbox.sh, against a stub rclone rather than a bucket.
#
#   ./scripts/backup-offbox-check.sh
#
# The stub records the arguments it was called with, which is what lets this
# assert the property the script exists for: that it never asks rclone to
# delete anything. That cannot be checked by reading the output, because a run
# that wrongly deleted the remote would look exactly like a run that did not.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${1:-$ROOT/deploy/backup-offbox.sh}"
[ -x "$SCRIPT" ] || { echo "not executable: $SCRIPT" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/bin" "$WORK/backups"
CALLS="$WORK/calls.log"

cat >"$WORK/bin/rclone" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
[ "${STUB_FAIL:-}" = "$1" ] && exit 1
exit 0
STUB
chmod +x "$WORK/bin/rclone"

pass=0
fail=0
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail + 1)); }

run() {
  local label="$1" expect="$2"; shift 2
  : >"$CALLS"
  local out code got
  out=$(PATH="$WORK/bin:$PATH" CALLS="$CALLS" "$@" 2>&1); code=$?
  got=ok; [ "$code" -ne 0 ] && got=fail
  if [ "$got" = "$expect" ]; then
    ok "$label"
  else
    bad "$label (exited $code, wanted $expect)"
    printf '%s\n' "$out" | sed 's/^/        /'
  fi
}

echo "1. It refuses rather than guesses"
run "no RECALL_BACKUP_REMOTE" fail \
  env RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT"
run "the source directory does not exist" fail \
  env RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/nope" "$SCRIPT"
# Nothing to copy is not nothing to do: a moved directory would otherwise be
# reported as a clean run, every day, until someone needed a restore.
run "the source is empty" fail \
  env RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT"

echo
echo "2. With snapshots present"
touch "$WORK/backups/recall-20260918-120000.db" \
      "$WORK/backups/recall-20260917-120000.db" \
      "$WORK/backups/notes.txt"
run "copies and verifies" ok \
  env RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT"

if grep -qE '(^| )(sync|delete|purge|rmdirs|cleanup)( |$)' "$CALLS"; then
  bad "a destructive rclone subcommand was used"
  sed 's/^/        /' "$CALLS"
else
  ok "never asks rclone to sync, delete, purge, rmdirs or cleanup"
fi

if grep -q -- '--one-way' "$CALLS"; then
  ok "verifies one-way, so snapshots this box has rotated away are not a mismatch"
else
  bad "the verify is not one-way, so it will report older remote copies as errors"
fi

if grep -q -- "--include recall-\*.db" "$CALLS"; then
  ok "only the snapshots are copied, not whatever else is in the directory"
else
  bad "no include filter — anything dropped in the backups directory would be uploaded"
fi

echo
echo "3. A failure is a failure"
run "rclone copy fails" fail \
  env STUB_FAIL=copy RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT"
run "rclone check fails" fail \
  env STUB_FAIL=check RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT"

echo
echo "4. Dry run"
run "exits clean" ok \
  env RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT" --dry-run
if grep -q -- '--dry-run' "$CALLS" && ! grep -q '^check ' "$CALLS"; then
  ok "passes --dry-run to rclone and skips the verify"
else
  bad "dry run did not pass the flag through, or verified anyway"
fi

echo
echo "5. The stamp /health reads"
# The whole point of the stamp is that it means "a copy reached the remote and
# matched" — so a run that failed the verify must not leave one behind, or a
# stopped backup keeps reporting itself as current, which is worse than having
# no stamp at all.
rm -f "$WORK/backups/.last-offbox"
run "a verified run writes it" ok \
  env RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT"
if [ -s "$WORK/backups/.last-offbox" ]; then
  ok "the stamp exists and is not empty"
else
  bad "no stamp, so GET /health can never know this ran"
fi

stamp=$(cat "$WORK/backups/.last-offbox" 2>/dev/null || true)
if printf '%s' "$stamp" | grep -Eq '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z$'; then
  ok "in the API's timestamp shape, so the client can read it"
else
  bad "stamp is not the API's timestamp shape: '$stamp'"
fi

# It must not be picked up as a snapshot, or it would be uploaded and counted.
: > "$CALLS"
PATH="$WORK/bin:$PATH" CALLS="$CALLS" \
  env RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT" >/dev/null 2>&1 || true
if grep -q 'last-offbox' "$CALLS"; then
  bad "the stamp was handed to rclone — the dot prefix is not keeping it out"
else
  ok "the stamp is never uploaded"
fi

rm -f "$WORK/backups/.last-offbox"
PATH="$WORK/bin:$PATH" CALLS="$CALLS" \
  env STUB_FAIL=check RECALL_BACKUP_REMOTE=r: RECALL_BACKUP_SRC="$WORK/backups" "$SCRIPT" >/dev/null 2>&1 || true
if [ -e "$WORK/backups/.last-offbox" ]; then
  bad "a run that failed the verify still stamped itself as successful"
else
  ok "a failed verify leaves no stamp"
fi
rm -f "$WORK/backups/.last-offbox"

echo
printf 'passed %d, failed %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
