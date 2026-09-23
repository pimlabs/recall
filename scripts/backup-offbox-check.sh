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
REMOTES="$WORK/remotes.list"
OBJDIR="$WORK/objects"
CRONTAB_FILE="$WORK/crontab.txt"

# Records every call it was given, like the plain stub the rest of this file
# uses. On top of that it fakes just enough of `config create`, `listremotes`,
# `copyto`, `cat` and `deletefile` for init's remotes and round-trip test to
# run against something, without ever touching a real bucket.
cat >"$WORK/bin/rclone" <<'STUB'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
[ "${STUB_FAIL:-}" = "$1" ] && exit 1

sanitize() { printf '%s' "$1" | tr '/:' '__'; }

case "${1:-}" in
  config)
    if [ "${2:-}" = create ]; then
      name="${3:-}"
      touch "$REMOTES"
      grep -qx "${name}:" "$REMOTES" 2>/dev/null || printf '%s:\n' "$name" >> "$REMOTES"
    fi
    ;;
  listremotes)
    [ -f "$REMOTES" ] && cat "$REMOTES"
    ;;
  copyto)
    mkdir -p "$OBJDIR"
    cp "${2:-}" "$OBJDIR/$(sanitize "${3:-}")" 2>/dev/null
    ;;
  cat)
    f="$OBJDIR/$(sanitize "${2:-}")"
    [ -f "$f" ] && cat "$f"
    ;;
  deletefile)
    rm -f "$OBJDIR/$(sanitize "${2:-}")"
    ;;
esac
exit 0
STUB
chmod +x "$WORK/bin/rclone"

# `crontab -l` reads a fake per-run crontab file, `crontab -` (or a filename)
# replaces it. Good enough to check what init installs without touching the
# real crontab of whoever runs this check.
cat >"$WORK/bin/crontab" <<'STUB'
#!/usr/bin/env bash
echo "crontab $*" >> "$CALLS"
case "${1:-}" in
  -l)
    if [ -s "$CRONTAB_FILE" ]; then cat "$CRONTAB_FILE"; else echo "no crontab for $(id -un)" >&2; exit 1; fi
    ;;
  -)
    cat > "$CRONTAB_FILE"
    ;;
  "")
    exit 1
    ;;
  *)
    cat "$1" > "$CRONTAB_FILE" 2>/dev/null
    ;;
esac
exit 0
STUB
chmod +x "$WORK/bin/crontab"

pass=0
fail=0
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail + 1)); }

run() {
  local label="$1" expect="$2"; shift 2
  : >"$CALLS"
  local out code got
  out=$(PATH="$WORK/bin:$PATH" CALLS="$CALLS" REMOTES="$REMOTES" OBJDIR="$OBJDIR" \
        CRONTAB_FILE="$CRONTAB_FILE" "$@" </dev/null 2>&1); code=$?
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

INIT_SRC="$WORK/init-backups"
mkdir -p "$INIT_SRC"
touch "$INIT_SRC/recall-20260101-000000.db"

# The env vars a full, non-interactive `init` needs for the s3 provider.
# Reused as-is for the idempotent re-run in section 7.
INIT_ENV=(
  env
  RECALL_BACKUP_SRC="$INIT_SRC"
  RECALL_BACKUP_INIT_PROVIDER=s3
  RECALL_BACKUP_INIT_BUCKET=recall-bucket
  RECALL_BACKUP_INIT_ENDPOINT=https://s3.example.com
  RECALL_BACKUP_INIT_ACCESS_KEY_ID=key-id
  RECALL_BACKUP_INIT_SECRET_ACCESS_KEY=shh
  RECALL_BACKUP_INIT_CONFIRM=yes
)

echo
echo "6. init: a full non-interactive run"
: >"$CALLS"
run "creates both remotes, tests, copies, installs cron" ok \
  "${INIT_ENV[@]}" "$SCRIPT" init

if grep -q '^config create recall-raw s3' "$CALLS" \
   && grep -q '^config create recall-crypt crypt' "$CALLS"; then
  ok "created the raw remote and the crypt remote over it"
else
  bad "did not create both remotes as expected"
  sed 's/^/        /' "$CALLS"
fi

if grep -q '^copy ' "$CALLS" && grep -q '^check ' "$CALLS"; then
  ok "ran the first copy through the script's normal path"
else
  bad "did not run the first copy"
fi

cron_lines=$(grep -c 'recall-backup-offbox-init' "$CRONTAB_FILE" 2>/dev/null || echo 0)
if [ "$cron_lines" = "1" ]; then
  ok "installed exactly one cron line"
else
  bad "expected exactly one cron line after init, found $cron_lines"
fi

if grep -qE '(^| )(sync|delete|purge|rmdirs|cleanup)( |$)' "$CALLS"; then
  bad "init used a destructive rclone subcommand beyond deletefile"
  sed 's/^/        /' "$CALLS"
else
  ok "init never asks rclone to sync, delete, purge, rmdirs or cleanup"
fi

deletefile_calls=$(grep -c '^deletefile ' "$CALLS")
deletefile_own_object=$(grep -c '^deletefile recall-crypt:\.recall-offbox-roundtrip-test$' "$CALLS")
if [ "$deletefile_calls" = "1" ] && [ "$deletefile_own_object" = "1" ]; then
  ok "the round trip deleted exactly its own test object, and nothing else"
else
  bad "expected exactly one deletefile call against the round-trip test object, saw $deletefile_calls"
  sed 's/^/        /' "$CALLS"
fi

echo
echo "7. init: a re-run is idempotent"
: >"$CALLS"
run "running init again" ok \
  "${INIT_ENV[@]}" "$SCRIPT" init

if grep -q '^config create' "$CALLS"; then
  bad "a re-run recreated a remote instead of reusing the existing one"
  sed 's/^/        /' "$CALLS"
else
  ok "a re-run reused the existing remotes, no recreation"
fi

cron_lines=$(grep -c 'recall-backup-offbox-init' "$CRONTAB_FILE" 2>/dev/null || echo 0)
if [ "$cron_lines" = "1" ]; then
  ok "still exactly one cron line after the re-run"
else
  bad "expected exactly one cron line after the re-run, found $cron_lines"
fi

echo
echo "8. init: refuses when rclone config and crontab would split across users"
: >"$CALLS"
run "SUDO_USER differs from the effective user" fail \
  env SUDO_USER=someone-else \
      RECALL_BACKUP_SRC="$INIT_SRC" \
      RECALL_BACKUP_INIT_PROVIDER=s3 \
      RECALL_BACKUP_INIT_BUCKET=another-bucket \
      RECALL_BACKUP_INIT_ENDPOINT=https://s3.example.com \
      RECALL_BACKUP_INIT_ACCESS_KEY_ID=key-id \
      RECALL_BACKUP_INIT_SECRET_ACCESS_KEY=shh \
      RECALL_BACKUP_INIT_CONFIRM=yes \
      "$SCRIPT" init
if [ -s "$CALLS" ]; then
  bad "called rclone even though the user split should refuse before touching it"
  sed 's/^/        /' "$CALLS"
else
  ok "refused before calling rclone or crontab at all"
fi

echo
echo "9. init: a missing confirmation stops before any remote is created"
: >"$CALLS"
run "no RECALL_BACKUP_INIT_CONFIRM, no terminal" fail \
  env RECALL_BACKUP_SRC="$INIT_SRC" \
      RECALL_BACKUP_INIT_PROVIDER=s3 \
      RECALL_BACKUP_INIT_BUCKET=third-bucket \
      RECALL_BACKUP_INIT_ENDPOINT=https://s3.example.com \
      RECALL_BACKUP_INIT_ACCESS_KEY_ID=key-id \
      RECALL_BACKUP_INIT_SECRET_ACCESS_KEY=shh \
      RECALL_BACKUP_INIT_RAW_REMOTE=recall-raw-unconfirmed \
      RECALL_BACKUP_INIT_CRYPT_REMOTE=recall-crypt-unconfirmed \
      "$SCRIPT" init
if grep -q '^config create' "$CALLS"; then
  bad "created a remote despite the missing confirmation"
  sed 's/^/        /' "$CALLS"
else
  ok "stopped before creating any remote"
fi

echo
echo "10. init: an existing remote name is an escape hatch"
printf 'recall-preexisting:\n' >> "$REMOTES"
: >"$CALLS"
run "provider existing reuses it instead of creating one" ok \
  env RECALL_BACKUP_SRC="$INIT_SRC" \
      RECALL_BACKUP_INIT_PROVIDER=existing \
      RECALL_BACKUP_INIT_RAW_REMOTE=recall-preexisting \
      RECALL_BACKUP_INIT_CRYPT_REMOTE=recall-crypt-existing \
      RECALL_BACKUP_INIT_CONFIRM=yes \
      "$SCRIPT" init
if grep -q '^config create recall-preexisting' "$CALLS"; then
  bad "recreated the pre-existing raw remote instead of reusing it"
else
  ok "did not recreate the pre-existing raw remote"
fi
if grep -q '^config create recall-crypt-existing crypt' "$CALLS"; then
  ok "still created the crypt remote wrapping the existing raw remote"
else
  bad "did not create the crypt remote over the existing raw remote"
fi

echo
printf 'passed %d, failed %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
