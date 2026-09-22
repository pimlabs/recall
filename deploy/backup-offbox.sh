#!/usr/bin/env bash
#
# Copies the server's own snapshots off this box.
#
#   RECALL_BACKUP_REMOTE=recall-crypt: ./deploy/backup-offbox.sh
#   RECALL_BACKUP_REMOTE=recall-crypt: ./deploy/backup-offbox.sh --dry-run
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
set -uo pipefail

DRY_RUN=false
[ "${1:-}" = "--dry-run" ] && DRY_RUN=true

REMOTE="${RECALL_BACKUP_REMOTE:-}"
SRC="${RECALL_BACKUP_SRC:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/backups}"
PATTERN='recall-*.db'

die() { printf 'backup-offbox: %s\n' "$1" >&2; exit 1; }

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
