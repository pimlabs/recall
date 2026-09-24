#!/usr/bin/env bash
#
# Runs deploy/README.md's database procedures, as the README has them,
# against a real recall-server, and checks what they leave behind.
#
#   cargo build --release -p recall-server
#   ./scripts/restore-check.sh target/release/recall-server
#
# The database is the only copy of someone's memory, and these are the
# commands an owner pastes when something has already gone wrong, so they
# are asserted, not reviewed. Under WAL the easy mistakes are silent: a
# restore that leaves the old recall.db-wal beside the new file gets the old
# database back; one that moves the files under a running server loses every
# push after it; one that moves them before it knows the snapshot exists
# leaves the server an empty database. Each of those makes this fail.
#
# No Docker daemon is needed. `docker` here is a stand-in, first on PATH,
# that does what these procedures ask of the real one: `stop` and `start`
# the recall-server container (this script's own server process) by the
# container names the compose files give, `inspect` which volume it mounts
# (the one they declare), and `run` an alpine `sh` with the volume and the backups
# directory mounted, which it does by running the same script with /data and
# /backups pointed at this script's directories. Inside it, `apk` does
# nothing and `sqlite3` is Python's sqlite3. `docker compose` fails the way
# it does on a host that runs another compose file than docker-compose.yml:
# a procedure that needs `-f` to act on the right stack must not use it.
set -u
# Absolute, since the procedures change directory before they start it.
BIN="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
README="$ROOT/deploy/README.md"
WORK=$(mktemp -d)
W="$WORK"
TOKEN="restore-check-token"
KEY="restore/check"
PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
URL="http://127.0.0.1:$PORT"
pass=0; fail=0

ok()  { printf '  \033[32mPASS\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=$((fail+1)); }
chk() { [ "$2" = "$3" ] && ok "$1" || { bad "$1"; printf '        got  %s\n        want %s\n' "$2" "$3"; }; }

cleanup() {
  [ -f "$W/server.pid" ] && kill -9 "$(cat "$W/server.pid")" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

# ---------------------------------------------------------------- names

# The container and volume names every compose file resolves to, which the
# procedures must use: read with PyYAML, or `docker compose config`.
read -r CONTAINER SQLITE_WEB VOLUME < <(python3 - "$ROOT" <<'EOF'
import json, os, subprocess, sys
root = sys.argv[1]
seen = set()
for f in ('docker-compose.yml', 'docker-compose.traefik.yml', 'docker-compose.direct.yml'):
    path = os.path.join(root, 'deploy', f)
    try:
        import yaml
        doc = yaml.safe_load(open(path))
    except ImportError:
        doc = json.loads(subprocess.run(
            ['docker', 'compose', '-f', path, 'config', '--format', 'json', '--no-interpolate'],
            check=True, capture_output=True, text=True).stdout)
    volume = (doc.get('volumes') or {}).get('recall-data') or {}
    seen.add((doc['services']['recall-server']['container_name'],
              doc['services']['sqlite-web']['container_name'],
              volume.get('name') or f"{doc['name']}_recall-data"))
if len(seen) != 1:
    sys.exit(f'the compose files disagree on the names: {sorted(seen)}')
print(*seen.pop())
EOF
)
[ -n "${VOLUME:-}" ] || { echo "could not read the names from the compose files"; exit 1; }
echo "compose: containers $CONTAINER, $SQLITE_WEB; volume $VOLUME"

# ---------------------------------------------------------------- blocks

# The fenced sh block of README that contains $1; the lines before one
# starting with $2, when given.
block() {
  python3 - "$README" "$1" "${2:-}" <<'EOF'
import re, sys
text, marker, stop = open(sys.argv[1]).read(), sys.argv[2], sys.argv[3]
found = [b for b in re.findall(r'```sh\n(.*?)```', text, re.S) if marker in b]
if len(found) != 1:
    sys.exit(f'{len(found)} sh blocks in deploy/README.md contain {marker!r}')
lines = found[0].splitlines(keepends=True)
if stop:
    lines = lines[:next(i for i, l in enumerate(lines) if l.startswith(stop))]
print(''.join(lines), end='')
EOF
}
RESTORE=$(block 'recall.db.restoring') || exit 1
ROLLBACK=$(block 'journal_mode=DELETE') || exit 1
INGRESS=$(block 'before-ingress-switch') || exit 1
LAST_RESORT=$(block 'before-cleanup' '# 3.') || exit 1

# ---------------------------------------------------------------- stand-ins

mkdir -p "$W/bin" "$W/deploy/backups" "$W/volume"
cat >"$W/ctl" <<EOF
#!/usr/bin/env bash
# start | stop | kill: this check's recall-server, as the container.
set -u
pidf="$W/server.pid"
running() { [ -f "\$pidf" ] && kill -0 "\$(cat "\$pidf")" 2>/dev/null; }
case "\$1" in
  start)
    running && exit 0
    RECALL_TOKEN=$TOKEN RECALL_PORT=$PORT RECALL_DB_PATH="$W/volume/recall.db" \\
      RECALL_BACKUP_DIR="$W/deploy/backups" RECALL_MERGE_ENABLED=false \\
      RECALL_RATE_LIMIT_MAX=100000 RECALL_TRUSTED_IP_HEADER= \\
      "$BIN" >>"$W/server.log" 2>&1 &
    echo \$! >"\$pidf"
    for _ in \$(seq 1 100); do
      curl -sf "$URL/health" >/dev/null 2>&1 && exit 0
      running || break
      sleep 0.1
    done
    echo "recall-server did not start:" >&2; tail -5 "$W/server.log" >&2; exit 1 ;;
  stop)
    running || exit 0
    kill -TERM "\$(cat "\$pidf")"
    for _ in \$(seq 1 600); do running || exit 0; sleep 0.1; done
    echo "recall-server did not stop" >&2; exit 1 ;;
  kill)
    running && kill -9 "\$(cat "\$pidf")"
    for _ in \$(seq 1 100); do running || exit 0; sleep 0.1; done ;;
esac
EOF
chmod +x "$W/ctl"

cat >"$W/bin/docker" <<EOF
#!/usr/bin/env bash
set -u
case "\$1" in
  compose)
    echo 'error while interpolating services.cloudflared.environment.TUNNEL_TOKEN: required variable CLOUDFLARE_TUNNEL_TOKEN is missing a value' >&2
    exit 1 ;;
  stop|start)
    cmd=\$1; shift
    for c in "\$@"; do
      case "\$c" in
        $CONTAINER)
          if [ "\$cmd" = stop ] && [ -n "\${STUB_STOP_DOES_NOTHING:-}" ]; then :; else "$W/ctl" "\$cmd" || exit 1; fi ;;
        $SQLITE_WEB) : ;;
        *) echo "Error response from daemon: No such container: \$c" >&2; exit 1 ;;
      esac
      echo "\$c"
    done ;;
  inspect)
    # Only the one question the restore asks: the names of the volumes the
    # server's container mounts, as the compose files declare them (and an
    # empty one for the backups bind mount).
    [ "\$2" = -f ] && [ "\$3" = '{{range .Mounts}}{{.Name}} {{end}}' ] \
      || { echo "stand-in docker: unsupported inspect: \$*" >&2; exit 1; }
    case "\${4:-}" in
      $CONTAINER) echo "\${STUB_MOUNTS:-$VOLUME}  " ;;
      *) echo "Error: No such object: \${4:-}" >&2; exit 1 ;;
    esac ;;
  run)
    shift; maps=()
    while [ \$# -gt 0 ]; do
      case "\$1" in
        --rm|-it|-i|-t) shift ;;
        -e) export "\$2"; shift 2 ;;
        -v) spec=\$2; src=\${spec%%:*}; rest=\${spec#*:}; dst=\${rest%%:*}
            case "\$src" in
              $VOLUME) src="$W/volume" ;;
              /*) ;;
              *) echo "docker: no such volume \$src (a new, empty one would be created)" >&2; exit 1 ;;
            esac
            maps+=("\$dst=\$src"); shift 2 ;;
        *) break ;;
      esac
    done
    [ "\$1" = alpine ] && [ "\$2" = sh ] || { echo "stand-in docker runs only alpine sh" >&2; exit 1; }
    flags=\$3; script=\$4
    for m in "\${maps[@]}"; do script=\${script//\${m%%=*}/\${m#*=}}; done
    PATH="$W/bin:\$PATH" exec sh "\$flags" "\$script" ;;
  *) echo "stand-in docker: unsupported: \$*" >&2; exit 1 ;;
esac
EOF
chmod +x "$W/bin/docker"

printf '#!/bin/sh\nexit 0\n' >"$W/bin/apk"
cat >"$W/bin/sqlite3" <<'EOF'
#!/usr/bin/env python3
# sqlite3 [-readonly] FILE SQL, printing rows as the shell's list mode does.
import sqlite3, sys
args = sys.argv[1:]
ro = args[0] == '-readonly'
if ro:
    args = args[1:]
db, sql = args
con = sqlite3.connect(f'file:{db}?mode=ro' if ro else db, uri=ro, isolation_level=None)
for row in con.execute(sql):
    print('|'.join('' if v is None else str(v) for v in row))
EOF
chmod +x "$W/bin/apk" "$W/bin/sqlite3"
export PATH="$W/bin:$PATH"

# ---------------------------------------------------------------- helpers

push() { # path -> status
  curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/sync" \
    -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -d "{\"project_key\":\"$KEY\",\"file_path\":\"$1\",\"content\":\"fact in $1\n\",\"source_env\":\"check\"}"
}
served() { # the files the server serves, or its status when not 200
  local out code
  out=$(curl -s -w '\n%{http_code}' "$URL/sync?project_key=$KEY" -H "authorization: Bearer $TOKEN")
  code=${out##*$'\n'}
  [ "$code" = 200 ] || { echo "status $code"; return; }
  printf '%s' "${out%$'\n'*}" | python3 -c 'import json, sys; print(" ".join(sorted(f["file_path"] for f in json.load(sys.stdin)["files"])))'
}
rows() { # the files a database file holds, read as SQLite reads it (its WAL included)
  python3 - "$1" "$KEY" <<'EOF'
import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
print(' '.join(r[0] for r in con.execute(
    'SELECT file_path FROM memory_files WHERE project_key = ? ORDER BY file_path', (sys.argv[2],))))
EOF
}
listing() { # the volume's files, with their inodes
  (cd "$W/volume" && ls -i1 | sort -k2)
}
walsize() { stat -c %s "$W/volume/recall.db-wal" 2>/dev/null || echo 0; }
paste_block() { # text: run it as pasted into a shell, from the deployment directory
  local code
  (cd "$W" && bash -c "$1") >"$W/block.out" 2>&1
  code=$?
  [ "$code" = 0 ] || [ -n "${EXPECT_FAILURE:-}" ] || sed 's/^/        | /' "$W/block.out"
  return "$code"
}
with_snapshot() { # block, snapshot name
  printf '%s' "${1//recall-<timestamp>.db/$2}"
}

SNAPSHOT_FILES="a1.md a2.md a3.md"
ALL_FILES="a1.md a2.md a3.md b1.md b2.md b3.md"

# A server that has served a1..a3, taken a snapshot of them as it started
# (its backup loop's first run), and then acknowledged b1..b3.
fresh() {
  "$W/ctl" kill
  rm -rf "$W/volume" "$W/deploy/backups"; mkdir -p "$W/volume" "$W/deploy/backups"
  "$W/ctl" start || exit 1
  for f in $SNAPSHOT_FILES; do [ "$(push "$f")" = 200 ] || { echo "setup push $f failed"; exit 1; }; done
  "$W/ctl" stop && "$W/ctl" start || exit 1
  # One snapshot from the first start, of nothing; the second is this one.
  for _ in $(seq 1 100); do
    [ "$(ls "$W"/deploy/backups/recall-*.db 2>/dev/null | wc -l)" -ge 2 ] && break
    sleep 0.1
  done
  SNAP=$(cd "$W/deploy/backups" && ls recall-*.db | sort | tail -1)
  for f in b1.md b2.md b3.md; do [ "$(push "$f")" = 200 ] || { echo "setup push $f failed"; exit 1; }; done
}

aside_rows() { # what the database moved aside holds
  local d; d=$(ls -d "$W"/volume/replaced-* 2>/dev/null | head -1)
  [ -n "$d" ] && rows "$d/recall.db" || echo "nothing moved aside"
}

# ---------------------------------------------------------------- checks

echo "== no procedure stops or starts through docker compose"
chk "no sh block in deploy/README.md runs 'docker compose stop' or 'start'" \
  "$(python3 -c 'import re, sys; print(sum(len(re.findall(r"docker compose (?:stop|start)", b)) for b in re.findall(r"```sh\n(.*?)```", open(sys.argv[1]).read(), re.S)))' "$README")" "0"

echo "== the restore, with the server running"
fresh
chk "the snapshot holds the three pushes before it" "$(rows "$W/deploy/backups/$SNAP")" "$SNAPSHOT_FILES"
paste_block "$(with_snapshot "$RESTORE" "$SNAP")"; code=$?
chk "the block succeeds" "$code" "0"
chk "the restarted server serves the snapshot" "$(served)" "$SNAPSHOT_FILES"
chk "the database moved aside is still whole, every acknowledged push" "$(aside_rows)" "$ALL_FILES"
chk "no recall.db.restoring is left" "$(ls "$W/volume" | grep -c restoring)" "0"

echo "== the restore, after a crash left commits in the WAL"
fresh
"$W/ctl" kill
wal=$(walsize)
[ "$wal" -gt 0 ] && ok "the crash left a WAL of $wal bytes" || bad "the crash left no WAL, so this case tests nothing"
paste_block "$(with_snapshot "$RESTORE" "$SNAP")"; code=$?
chk "the block succeeds" "$code" "0"
chk "the server serves the snapshot, not the WAL it replaced" "$(served)" "$SNAPSHOT_FILES"
chk "the database moved aside is still whole, WAL and all" "$(aside_rows)" "$ALL_FILES"

echo "== the restore, with a snapshot name that is not there"
fresh
before=$(listing); pid=$(cat "$W/server.pid")
EXPECT_FAILURE=1 paste_block "$(with_snapshot "$RESTORE" "recall-2000-01-01T00-00-00-000Z.db")"; code=$?
[ "$code" != 0 ] && ok "the block fails" || bad "the block succeeded with no snapshot"
chk "the live database is untouched, file for file" "$(listing)" "$before"
kill -0 "$pid" 2>/dev/null && ok "the server was never stopped" || bad "the server was stopped"
chk "and it still serves every acknowledged push" "$(served)" "$ALL_FILES"
chk "and still takes pushes" "$(push after.md)" "200"

echo "== the restore, on a stack started with -p, whose volume has another name"
fresh
before=$(listing); pid=$(cat "$W/server.pid")
STUB_MOUNTS=other_recall-data EXPECT_FAILURE=1 paste_block "$(with_snapshot "$RESTORE" "$SNAP")"; code=$?
[ "$code" != 0 ] && ok "the block fails" || bad "the block restored into a volume the server does not use"
chk "the live database is untouched, file for file" "$(listing)" "$before"
kill -0 "$pid" 2>/dev/null && ok "the server was never stopped" || bad "the server was stopped"

echo "== the restore, with the server left running (a stop that stopped nothing)"
fresh
STUB_STOP_DOES_NOTHING=1 paste_block "$(with_snapshot "$RESTORE" "$SNAP")"
chk "a push to the server still running is refused, not answered 200" "$(push lost.md)" "500"
chk "and a pull" "$(served)" "status 500"
chk "nothing was written into the database moved aside" "$(aside_rows)" "$ALL_FILES"
"$W/ctl" stop && "$W/ctl" start
chk "restarted, it serves the snapshot" "$(served)" "$SNAPSHOT_FILES"

echo "== a backup before switching ingress, beside the running server"
fresh
[ "$(walsize)" -gt 0 ] && ok "the newest pushes are in the WAL" || bad "the WAL is empty, so this case tests nothing"
paste_block "$INGRESS"; code=$?
chk "the block succeeds" "$code" "0"
taken=$(ls "$W"/deploy/backups/before-ingress-switch-*.db 2>/dev/null | head -1)
chk "it holds every acknowledged push" "$(rows "${taken:-/nonexistent}" 2>&1)" "$ALL_FILES"

echo "== the last resort's backup, after a crash"
fresh
"$W/ctl" kill
paste_block "$LAST_RESORT"; code=$?
chk "steps 1 and 2 succeed" "$code" "0"
taken=$(ls "$W"/deploy/backups/before-cleanup-*.db 2>/dev/null | head -1)
chk "it holds every acknowledged push, those only in the WAL included" "$(rows "${taken:-/nonexistent}" 2>&1)" "$ALL_FILES"

echo "== putting the file back in the rollback journal, for an older image"
fresh
"$W/ctl" kill
paste_block "$ROLLBACK"; code=$?
chk "the block succeeds" "$code" "0"
chk "it prints delete" "$(tail -1 "$W/block.out")" "delete"
chk "recall.db-wal and recall.db-shm are gone" "$(ls "$W/volume" | grep -c -- '-wal\|-shm')" "0"
chk "the header says rollback journal" \
  "$(python3 -c 'import sys; h = open(sys.argv[1], "rb").read(20); print(h[18], h[19])' "$W/volume/recall.db")" "1 1"
chk "every acknowledged push is in recall.db alone" "$(rows "$W/volume/recall.db")" "$ALL_FILES"
"$W/ctl" start
chk "a server with WAL serves it all again" "$(served)" "$ALL_FILES"

echo
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
