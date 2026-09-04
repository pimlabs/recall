#!/bin/sh
# Runs as root just long enough to fix ownership on /data and /backups
# (when mounted) — needed because a bind/named-volume mount can arrive
# owned by root regardless of what runs inside the container, and a fresh
# bind mount on a real Linux host defaults to root:root, unlike some
# Docker Desktop/VM setups where this went unnoticed. Then drops to the
# unprivileged `node` user for the actual long-running process.
set -e
mkdir -p /data
chown -R node:node /data
[ -d /backups ] && chown -R node:node /backups
exec su-exec node "$@"
