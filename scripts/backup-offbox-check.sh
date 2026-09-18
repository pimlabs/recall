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
printf 'passed %d, failed %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
