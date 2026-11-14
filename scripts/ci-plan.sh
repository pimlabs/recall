#!/usr/bin/env bash
# Which of ci.yml's gated jobs a dispatched run needs: ci.yml's `plan` job.
#
#   scripts/ci-plan.sh [<base>]        # base defaults to origin/main
#
# Prints image=, windows=, deploy= and cargo= as true or false, to
# $GITHUB_OUTPUT when it is set and to stdout otherwise, so the same script
# runs in CI and on a laptop. REPO_PRIVATE=true is what CI sets for a
# private repository; anything else counts as public.
#
# The diff is what the dispatched commit (HEAD) changes since it left
# <base>: `git diff $(git merge-base <base> HEAD) HEAD`. Two cases run every
# job instead of reading the diff, because neither is evidence that nothing
# changed:
#
#   - The diff is empty. HEAD is <base> or behind it, which is what a
#     dispatch of `main` is: the run of main a push used to start, and the
#     one start-release.yml asks for before tagging a private repository.
#   - The diff cannot be read: <base> is missing, or shares no history with
#     HEAD.
set -euo pipefail

base_ref="${1:-origin/main}"
out="${GITHUB_OUTPUT:-/dev/stdout}"

run_everything() {
  echo "::notice::$1, so every job runs."
  printf '%s\n' image=true windows=true deploy=true cargo=true >> "$out"
  exit 0
}

base=$(git merge-base "$base_ref" HEAD 2>/dev/null) \
  || run_everything "Could not find where HEAD left $base_ref"

# --no-renames deliberately. With rename detection on, moving
# crates/foo.rs to docs/foo.md is reported as the single path docs/foo.md:
# the side of the move that matters here vanishes from the list, and the
# change reads as docs-only.
changed=$(git diff --no-renames --name-only "$base" HEAD) \
  || run_everything "Could not diff $base against HEAD"
[ -n "$changed" ] \
  || run_everything "HEAD changes nothing since $base_ref (a dispatch of main itself)"

echo "Changed since $base_ref (merge base ${base:0:12}):"
echo "$changed"

matches() { printf '%s\n' "$changed" | grep -Eq "$1"; }

# Asked of the diff whatever the repository's visibility: what
# check-release-image costs is other people's uptime, not runner minutes.
if matches '^(scripts/(verify-release-image|check-release-environments|ci-plan)\.sh|\.github/workflows/(ci|deploy-release|build-release|publish)\.yml)$'; then
  echo "deploy=true" >> "$out"
else
  echo "deploy=false" >> "$out"
  echo "::notice::Nothing in scripts/verify-release-image.sh, scripts/check-release-environments.sh, scripts/ci-plan.sh, deploy-release.yml, build-release.yml, publish.yml or ci.yml changed, so check-release-image was skipped. A dispatch of main runs it."
fi

# What audit-dependencies.yml used to watch on every push and pull request.
if matches '(^|/)Cargo\.(toml|lock)$|^scripts/ci-plan\.sh$|^\.github/workflows/(ci|audit-dependencies)\.yml$'; then
  echo "cargo=true" >> "$out"
else
  echo "cargo=false" >> "$out"
  echo "::notice::No Cargo.toml, Cargo.lock, scripts/ci-plan.sh, audit-dependencies.yml or ci.yml changed, so audit-dependencies was skipped. Its weekly run and a dispatch of main still run it."
fi

if [ "${REPO_PRIVATE:-false}" != "true" ]; then
  echo "Public repository: runners are free, so the image and the Windows jobs run whatever changed."
  printf '%s\n' image=true windows=true >> "$out"
  exit 0
fi

# ci.yml and this script count for every gate: a change to a gate, or to
# the jobs it feeds, is only proven by running them.
if matches '^(crates/|deploy/|Cargo\.(toml|lock)$|scripts/(release-image|image-pullable|podman-check|verify-release-image|ci-plan)\.sh$|\.github/workflows/(ci|build-release)\.yml$)'; then
  echo "image=true" >> "$out"
else
  echo "image=false" >> "$out"
  # One line on purpose: a `\` continuation inside a quoted string keeps
  # the next line's indentation in the annotation.
  echo "::notice::Nothing under crates/, deploy/, Cargo.toml, Cargo.lock, scripts/release-image.sh, scripts/image-pullable.sh, scripts/podman-check.sh, scripts/verify-release-image.sh, scripts/ci-plan.sh, build-release.yml or ci.yml changed, so the server image builds and check-podman were skipped. A dispatch of main runs them."
fi

# .github/actions/ too: the Windows jobs set up Rust through the local
# rust-setup action. And what the Windows installer test runs: install.ps1,
# its own script, the packer build-release.yml uses and the stand-in
# release server.
if matches '^(crates/|Cargo\.(toml|lock)$|install\.ps1$|npm/install\.js$|scripts/(installer-test\.ps1|package-release\.py|fake-release-server\.py|ci-plan\.sh)$|\.github/workflows/(ci|build-release)\.yml$|\.github/actions/)'; then
  echo "windows=true" >> "$out"
else
  echo "windows=false" >> "$out"
  echo "::notice::Nothing under crates/, .github/actions/, Cargo.toml, Cargo.lock, install.ps1, npm/install.js, the installer test's scripts, scripts/ci-plan.sh, build-release.yml or ci.yml changed, so the Windows jobs were skipped. A dispatch of main runs them."
fi
