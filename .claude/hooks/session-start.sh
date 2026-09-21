#!/usr/bin/env bash
#
# Puts a current `recall` on PATH before the session's own hooks use it.
#
# Cloud sessions arrive with whatever binary is baked into the environment
# image. In this one that was v0.1.0, dated 15 September, months of releases
# behind the checkout it was running against — and nothing said so, because a
# stale client syncs perfectly well. The wire format is frozen, so the only
# symptom is features quietly missing: no machine scope, no `promote --to`,
# no `recall doctor`. The gap widens every release.
#
# Synchronous on purpose. `recall pull` is a SessionStart hook too, and the
# whole point is that it runs against the binary this installs rather than
# the one it replaces. Async would make which of the two wins a race.
set -euo pipefail

# A laptop has its own install and its own idea of which version it wants;
# this exists for environments that get one handed to them.
[ "${CLAUDE_CODE_REMOTE:-}" = "true" ] || exit 0

repo="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
cd "$repo" || exit 0

command -v cargo >/dev/null 2>&1 || {
    echo "session-start: no cargo, leaving recall alone" >&2
    exit 0
}

want=$(git rev-parse HEAD 2>/dev/null) || exit 0

# `recall version` prints `recall <semver> (<commit>)`. Reading the commit
# back out is what makes this idempotent: a resumed or compacted session
# re-runs SessionStart, and rebuilding every time would cost a minute for
# nothing.
installed() {
    recall version 2>/dev/null |
        sed -n 's/^recall [^ ]* (\([0-9a-f]\{7,40\}\)).*/\1/p'
}

[ "$(installed)" = "$want" ] && exit 0

# --root matters more than it looks. cargo's default is ~/.cargo/bin, and in
# this image $HOME/.local/bin comes *earlier* on PATH — so installing the
# normal way would report success while `recall` kept resolving to the stale
# binary. Installing over the one that wins is the point.
#
# RECALL_GIT_COMMIT is read through option_env!, so it has to be set here or
# the binary reports "unknown" and the check above can never match, turning
# this into a rebuild on every single session start.
# --force because there is already a `recall` there and replacing it is the
# entire job. Without it cargo refuses — "binary `recall` already exists in
# destination" — and the stale one stays exactly where it was.
echo "session-start: building recall @ ${want:0:8}" >&2
RECALL_GIT_COMMIT="$want" \
    cargo install --path crates/recall --root "$HOME/.local" --locked --force --quiet

# Assert the outcome, not the mechanism. `cargo install` succeeding says
# where a file was written; it says nothing about which `recall` the hooks
# are about to run, and that is the question this script exists to answer.
got=$(installed)
if [ "$got" != "$want" ]; then
    echo "session-start: installed ${want:0:8} but recall still resolves to ${got:-unknown}" >&2
    echo "session-start:   $(command -v recall 2>/dev/null || echo 'not on PATH')" >&2
    exit 0
fi

echo "session-start: recall @ ${want:0:8} ready" >&2
