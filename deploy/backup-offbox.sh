#!/usr/bin/env bash
#
# Copies the server's own snapshots off this box.
#
#   RECALL_BACKUP_REMOTE=recall-crypt: ./deploy/backup-offbox.sh
#   RECALL_BACKUP_REMOTE=recall-crypt: ./deploy/backup-offbox.sh --dry-run
#   ./deploy/backup-offbox.sh init
#
# The server already writes consistent snapshots to deploy/backups/ every
# RECALL_BACKUP_INTERVAL_HOURS and keeps the last RECALL_BACKUP_KEEP of them.
# This does one job those cannot: put them somewhere the loss of this machine
# does not reach. See "Backups" in deploy/README.md.
#
# It copies. It never deletes, and that is the single decision this script is
# built around.
#
# The obvious implementation is `rclone sync`, which would mirror the
# directory — including the deletions the keep-N rotation makes, so the remote
# would hold the same last seven and nothing older. That is also what makes it
# unusable here: `sync` propagates *any* emptiness. A bind mount that did not
# come up, a container that never started, a path edited by one character, and
# the remote is emptied to match, in one run, without an error — because
# deleting everything is exactly what a correct mirror of an empty directory
# looks like. A backup that can delete is a backup that can lose everything,
# and it does it at the moment you need it, quietly, having reported success.
#
# So: copy only, and let the remote grow. These are SQLite snapshots of a
# personal memory store, measured in kilobytes; a bucket lifecycle rule is the
# right place to expire them if it ever matters, because that decision lives
# with the thing that holds the copies rather than with the thing that makes
# them.
#
# `init` walks the one-time setup this needs: pick a provider, create the raw
# and crypt remotes, generate and show the crypt password(s) once, run an
# encrypted round-trip test, do the first copy, and install the cron line for
# whichever user is actually running it. See "Off-box" in deploy/README.md.
set -uo pipefail

die() { printf 'backup-offbox: %s\n' "$1" >&2; exit 1; }

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
# Fixed, not randomised: init only ever runs by hand or from this one
# command, so there is no concurrency to guard against, and a fixed name
# keeps the check script's assertions simple.
TESTOBJ=".recall-offbox-roundtrip-test"

# The trap deploy/README.md and ROADMAP.md both name: `rclone config` reads
# and writes ~/.config/rclone/rclone.conf for whoever's $HOME is in effect,
# while `crontab` acts on whoever's crontab the effective user owns. Under
# `sudo` those can quietly stop being the same person — usually because sudo
# preserved the original $HOME while the effective user became root — and the
# result is a cron job that fails every night with "didn't find section in
# config file", into a mailbox nobody reads.
check_same_user() {
  if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "$(id -un)" ]; then
    die "running via sudo as $(id -un), invoked by $SUDO_USER. rclone config and crontab must belong to the same user, or the cron job will not find the remotes this creates. Log in as $SUDO_USER and run this without sudo."
  fi
}

# 24 random bytes is plenty for a crypt password; base64 keeps it one line.
gen_secret() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -base64 24
  elif [ -r /dev/urandom ]; then
    head -c 24 /dev/urandom | base64
  else
    die "no openssl and no /dev/urandom to generate a crypt password from"
  fi
}

# require VAR --flag "prompt text" [secret]
#
# If $VAR already has a value (env or flag), leaves it alone. On a terminal,
# prompts for it. Off one, records it as missing instead of asking, so every
# missing value can be named at once rather than one refusal per re-run.
require() {
  local var="$1" flag="$2" prompt="$3" secret="${4:-}"
  local cur="${!var}"
  if [ -n "$cur" ]; then
    return 0
  fi
  if [ "$TTY" = true ]; then
    local val=""
    if [ "$secret" = secret ]; then
      read -r -s -p "$prompt: " val
      printf '\n'
    else
      read -r -p "$prompt: " val
    fi
    [ -n "$val" ] || die "$prompt cannot be empty"
    printf -v "$var" '%s' "$val"
  else
    MISSING+=("$flag (or \$$var)")
  fi
}

# Shown exactly once, from exactly one call site.
show_and_confirm_password() {
  local password="$1" password2="$2" tty="$3"
  printf '\n'
  printf 'backup-offbox: crypt password (encrypts file contents):\n  %s\n' "$password"
  printf 'backup-offbox: crypt password2 (encrypts file names):\n  %s\n' "$password2"
  printf '\n'
  printf 'A copy you cannot decrypt is not a copy. Store both lines somewhere\n'
  printf 'that will still exist after this machine is gone: a password manager,\n'
  printf 'a printed sheet, anywhere except only on this screen.\n\n'
  if [ "$tty" = true ]; then
    local reply=""
    read -r -p "Type yes once they are stored somewhere else: " reply
    [ "$reply" = "yes" ] \
      || die "not confirmed. Stopping before the round-trip test; nothing else was touched."
  else
    printf 'backup-offbox: confirmation received ahead of time, continuing.\n'
  fi
}

# Writes one object through the crypt remote, reads it back, compares, and
# deletes only that object. This is the one place besides a restore that this
# script ever asks rclone to delete anything, and it only ever names $TESTOBJ.
round_trip_test() {
  local cryptremote="$1"
  local tmp content readback
  tmp=$(mktemp)
  content="recall backup-offbox round-trip $(date -u +%Y-%m-%dT%H:%M:%S.000Z)"
  printf '%s' "$content" > "$tmp"
  if ! rclone copyto "$tmp" "${cryptremote}:${TESTOBJ}"; then
    rm -f "$tmp"
    die "could not write the round-trip test object through $cryptremote. Nothing else was touched."
  fi
  rm -f "$tmp"
  readback=$(rclone cat "${cryptremote}:${TESTOBJ}")
  if [ "$readback" != "$content" ]; then
    rclone deletefile "${cryptremote}:${TESTOBJ}" >/dev/null 2>&1 || true
    die "round-trip mismatch: wrote one thing through $cryptremote, read back another. Stopping before the first copy."
  fi
  rclone deletefile "${cryptremote}:${TESTOBJ}" \
    || die "round-trip matched, but deleting the test object failed. Remove $TESTOBJ from $cryptremote by hand before relying on this."
  printf 'backup-offbox: encrypted round-trip test passed (wrote, read back and removed %s)\n' "$TESTOBJ"
}

# Idempotent: a line tagged with $tag already present means leave it alone,
# so a re-run of init never duplicates it.
install_cron() {
  local cryptremote="$1" selfpath="$2" schedule="${3:-}"
  [ -n "$schedule" ] || schedule="17 */6 * * *"
  local tag="recall-backup-offbox-init"
  local selfdir
  selfdir="$(dirname "$selfpath")"
  local line="$schedule cd $selfdir && RECALL_BACKUP_REMOTE=${cryptremote}: /usr/bin/flock -n /tmp/recall-backup.lock $selfpath # $tag"
  local existing
  existing=$(crontab -l 2>/dev/null || true)
  if printf '%s\n' "$existing" | grep -qF "$tag"; then
    printf 'backup-offbox: cron already installed for %s, leaving it as is\n' "$(id -un)"
    return 0
  fi
  if [ -n "$existing" ]; then
    { printf '%s\n' "$existing"; printf '%s\n' "$line"; } | crontab -
  else
    printf '%s\n' "$line" | crontab -
  fi \
    || die "could not install the cron line. Add it yourself: $line"
  printf 'backup-offbox: installed the cron line for %s (%s)\n' "$(id -un)" "$schedule"
}

cmd_init() {
  check_same_user

  local TTY=false
  [ -t 0 ] && TTY=true

  : "${RECALL_BACKUP_INIT_PROVIDER:=}"
  : "${RECALL_BACKUP_INIT_BUCKET:=}"
  : "${RECALL_BACKUP_INIT_ACCESS_KEY_ID:=}"
  : "${RECALL_BACKUP_INIT_SECRET_ACCESS_KEY:=}"
  : "${RECALL_BACKUP_INIT_ENDPOINT:=}"
  : "${RECALL_BACKUP_INIT_REGION:=}"
  : "${RECALL_BACKUP_INIT_ACCOUNT_ID:=}"
  : "${RECALL_BACKUP_INIT_RAW_REMOTE:=}"
  : "${RECALL_BACKUP_INIT_CRYPT_REMOTE:=}"
  : "${RECALL_BACKUP_INIT_CRYPT_PATH:=}"
  : "${RECALL_BACKUP_INIT_PASSWORD:=}"
  : "${RECALL_BACKUP_INIT_PASSWORD2:=}"
  : "${RECALL_BACKUP_INIT_CRON:=}"
  : "${RECALL_BACKUP_INIT_CONFIRM:=}"

  while [ $# -gt 0 ]; do
    case "$1" in
      --provider) RECALL_BACKUP_INIT_PROVIDER="${2:-}"; shift 2 ;;
      --bucket) RECALL_BACKUP_INIT_BUCKET="${2:-}"; shift 2 ;;
      --access-key-id) RECALL_BACKUP_INIT_ACCESS_KEY_ID="${2:-}"; shift 2 ;;
      --secret-access-key) RECALL_BACKUP_INIT_SECRET_ACCESS_KEY="${2:-}"; shift 2 ;;
      --endpoint) RECALL_BACKUP_INIT_ENDPOINT="${2:-}"; shift 2 ;;
      --region) RECALL_BACKUP_INIT_REGION="${2:-}"; shift 2 ;;
      --account-id) RECALL_BACKUP_INIT_ACCOUNT_ID="${2:-}"; shift 2 ;;
      --raw-remote) RECALL_BACKUP_INIT_RAW_REMOTE="${2:-}"; shift 2 ;;
      --crypt-remote) RECALL_BACKUP_INIT_CRYPT_REMOTE="${2:-}"; shift 2 ;;
      --crypt-path) RECALL_BACKUP_INIT_CRYPT_PATH="${2:-}"; shift 2 ;;
      --password) RECALL_BACKUP_INIT_PASSWORD="${2:-}"; shift 2 ;;
      --password2) RECALL_BACKUP_INIT_PASSWORD2="${2:-}"; shift 2 ;;
      --cron) RECALL_BACKUP_INIT_CRON="${2:-}"; shift 2 ;;
      --confirm) RECALL_BACKUP_INIT_CONFIRM=yes; shift ;;
      *) die "init: unknown argument $1" ;;
    esac
  done

  command -v rclone >/dev/null 2>&1 || die "rclone is not installed. See deploy/README.md."
  command -v crontab >/dev/null 2>&1 || die "crontab is not installed, so the cron line cannot be installed."

  local raw_remote crypt_remote crypt_path
  raw_remote="${RECALL_BACKUP_INIT_RAW_REMOTE:-recall-raw}"
  crypt_remote="${RECALL_BACKUP_INIT_CRYPT_REMOTE:-recall-crypt}"
  crypt_path="${RECALL_BACKUP_INIT_CRYPT_PATH:-recall}"

  local already_have_crypt=false
  if rclone listremotes 2>/dev/null | grep -qx "${crypt_remote}:"; then
    already_have_crypt=true
  fi

  if [ "$already_have_crypt" = true ]; then
    printf 'backup-offbox: %s already exists. Leaving its remote and password as they are.\n' "$crypt_remote"
  else
    local MISSING=()
    require RECALL_BACKUP_INIT_PROVIDER --provider "Provider (s3, r2, b2, existing)"

    case "$RECALL_BACKUP_INIT_PROVIDER" in
      s3)
        require RECALL_BACKUP_INIT_BUCKET --bucket "Bucket name"
        require RECALL_BACKUP_INIT_ENDPOINT --endpoint "S3-compatible endpoint URL"
        require RECALL_BACKUP_INIT_ACCESS_KEY_ID --access-key-id "Access key ID"
        require RECALL_BACKUP_INIT_SECRET_ACCESS_KEY --secret-access-key "Secret access key" secret
        ;;
      r2)
        require RECALL_BACKUP_INIT_ACCOUNT_ID --account-id "Cloudflare account ID"
        require RECALL_BACKUP_INIT_BUCKET --bucket "R2 bucket name"
        require RECALL_BACKUP_INIT_ACCESS_KEY_ID --access-key-id "R2 access key ID"
        require RECALL_BACKUP_INIT_SECRET_ACCESS_KEY --secret-access-key "R2 secret access key" secret
        ;;
      b2)
        require RECALL_BACKUP_INIT_REGION --region "B2 region, from the bucket's S3 endpoint (e.g. us-west-002)"
        require RECALL_BACKUP_INIT_BUCKET --bucket "B2 bucket name"
        require RECALL_BACKUP_INIT_ACCESS_KEY_ID --access-key-id "B2 application key ID"
        require RECALL_BACKUP_INIT_SECRET_ACCESS_KEY --secret-access-key "B2 application key" secret
        ;;
      existing)
        require RECALL_BACKUP_INIT_RAW_REMOTE --raw-remote "Name of the already-configured remote"
        raw_remote="$RECALL_BACKUP_INIT_RAW_REMOTE"
        ;;
      "")
        : # already recorded as missing, above
        ;;
      *)
        die "unknown provider '$RECALL_BACKUP_INIT_PROVIDER': expected s3, r2, b2 or existing"
        ;;
    esac

    if [ "$TTY" = false ]; then
      require RECALL_BACKUP_INIT_CONFIRM --confirm "confirmation that the crypt password(s) will be stored somewhere else"
    fi

    if [ "${#MISSING[@]}" -gt 0 ]; then
      printf 'backup-offbox init: no terminal to ask on, and missing:\n' >&2
      local m
      for m in "${MISSING[@]}"; do printf '  %s\n' "$m" >&2; done
      exit 1
    fi

    if [ "$RECALL_BACKUP_INIT_PROVIDER" = existing ]; then
      rclone listremotes 2>/dev/null | grep -qx "${raw_remote}:" \
        || die "no remote named $raw_remote. Set it up with rclone config first, or pick a different --provider."
      printf 'backup-offbox: using the existing remote %s\n' "$raw_remote"
    else
      case "$RECALL_BACKUP_INIT_PROVIDER" in
        s3)
          rclone config create "$raw_remote" s3 \
            provider=Other \
            access_key_id="$RECALL_BACKUP_INIT_ACCESS_KEY_ID" \
            secret_access_key="$RECALL_BACKUP_INIT_SECRET_ACCESS_KEY" \
            endpoint="$RECALL_BACKUP_INIT_ENDPOINT" \
            region="${RECALL_BACKUP_INIT_REGION:-auto}" \
            --non-interactive --obscure \
            || die "could not create the $raw_remote remote"
          ;;
        r2)
          rclone config create "$raw_remote" s3 \
            provider=Cloudflare \
            access_key_id="$RECALL_BACKUP_INIT_ACCESS_KEY_ID" \
            secret_access_key="$RECALL_BACKUP_INIT_SECRET_ACCESS_KEY" \
            endpoint="https://${RECALL_BACKUP_INIT_ACCOUNT_ID}.r2.cloudflarestorage.com" \
            region=auto \
            --non-interactive --obscure \
            || die "could not create the $raw_remote remote"
          ;;
        b2)
          rclone config create "$raw_remote" s3 \
            provider=Other \
            access_key_id="$RECALL_BACKUP_INIT_ACCESS_KEY_ID" \
            secret_access_key="$RECALL_BACKUP_INIT_SECRET_ACCESS_KEY" \
            endpoint="https://s3.${RECALL_BACKUP_INIT_REGION}.backblazeb2.com" \
            region="$RECALL_BACKUP_INIT_REGION" \
            --non-interactive --obscure \
            || die "could not create the $raw_remote remote"
          ;;
      esac
      printf 'backup-offbox: created the %s remote (%s)\n' "$raw_remote" "$RECALL_BACKUP_INIT_PROVIDER"
    fi

    local password password2
    password="${RECALL_BACKUP_INIT_PASSWORD:-$(gen_secret)}"
    password2="${RECALL_BACKUP_INIT_PASSWORD2:-$(gen_secret)}"

    rclone config create "$crypt_remote" crypt \
      remote="${raw_remote}:${crypt_path}" \
      password="$password" \
      password2="$password2" \
      --non-interactive --obscure \
      || die "could not create the $crypt_remote remote"
    printf 'backup-offbox: created the %s remote (crypt over %s:%s)\n' "$crypt_remote" "$raw_remote" "$crypt_path"

    show_and_confirm_password "$password" "$password2" "$TTY"
  fi

  round_trip_test "$crypt_remote"
  install_cron "$crypt_remote" "$SELF" "$RECALL_BACKUP_INIT_CRON"

  printf 'backup-offbox: running the first copy\n'
  RECALL_BACKUP_REMOTE="${crypt_remote}:" "$SELF"
}

if [ "${1:-}" = "init" ]; then
  shift
  cmd_init "$@"
  exit $?
fi

DRY_RUN=false
[ "${1:-}" = "--dry-run" ] && DRY_RUN=true

REMOTE="${RECALL_BACKUP_REMOTE:-}"
SRC="${RECALL_BACKUP_SRC:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/backups}"
PATTERN='recall-*.db'

command -v rclone >/dev/null 2>&1 \
  || die "rclone is not installed. See deploy/README.md."

[ -n "$REMOTE" ] \
  || die "RECALL_BACKUP_REMOTE is not set (e.g. 'recall-crypt:'). Refusing to guess where your backups go."

[ -d "$SRC" ] \
  || die "$SRC does not exist. Has the server ever run? GET /health reports last_backup_at."

# An empty source is the case this script must never treat as ordinary. With
# `copy` it cannot delete anything, but silence would still read as success on
# a run that backed up nothing at all — which is how you discover, months
# later, that the directory moved.
count=$(find "$SRC" -maxdepth 1 -name "$PATTERN" -type f | wc -l | tr -d ' ')
[ "$count" -gt 0 ] \
  || die "no $PATTERN files in $SRC. Nothing to copy, which is not the same as nothing to do."

flags=(--include "$PATTERN" --no-traverse)
$DRY_RUN && flags+=(--dry-run)

printf 'backup-offbox: %s snapshot(s) in %s → %s\n' "$count" "$SRC" "$REMOTE"
rclone copy "$SRC" "$REMOTE" "${flags[@]}" \
  || die "rclone copy failed — nothing was removed anywhere, so this is safe to re-run."

if $DRY_RUN; then
  printf 'backup-offbox: dry run, nothing was uploaded\n'
  exit 0
fi

# One-way on purpose: the remote holds snapshots this box has already rotated
# away, and those extras are the point, not a discrepancy. This asks only
# whether everything here arrived there.
rclone check "$SRC" "$REMOTE" --include "$PATTERN" --one-way \
  || die "copied, but the remote does not match. Do not trust this run."

# Written only after the verify, so the stamp means "a copy reached the
# remote and matched", not "the script got this far". The server reads it
# into GET /health, which is how a stopped backup becomes something anyone
# can see: this runs from cron, and a cron job that dies mails its error to
# a mailbox nobody reads. Silence here used to be indistinguishable from
# success, and that is the failure you discover when you need a restore.
#
# The `.` prefix keeps it out of the PATTERN glob, so it is never itself
# uploaded and never counted as a snapshot. Seconds precision with literal
# milliseconds: the shape has to match the timestamps the rest of the API
# uses, and nothing here needs more than a date.
printf '%s\n' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" > "$SRC/.last-offbox" 2>/dev/null \
  || printf 'backup-offbox: could not write the stamp to %s, so /health will not know this ran\n' "$SRC" >&2

printf 'backup-offbox: verified %s snapshot(s) on %s\n' "$count" "$REMOTE"
