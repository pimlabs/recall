#!/bin/sh
# Runs as root just long enough to fix ownership on /data — needed
# because the named volume already existed (created back when the
# server ran as root) before this non-root user was introduced, so its
# contents don't automatically belong to `node`. Then drops to the
# unprivileged `node` user for the actual long-running process.
set -e
mkdir -p /data
chown -R node:node /data
exec su-exec node "$@"
