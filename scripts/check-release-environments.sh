#!/usr/bin/env bash
# Are the environments Publish a release names there, and protected?
#
#   scripts/check-release-environments.sh
#
# For each release-* environment publish.yml's jobs run in: it must exist,
# need a required reviewer, not let administrators bypass that, and admit
# only `v*` tags. GitHub creates an environment a job names but that does
# not exist, on the spot and unprotected, and that job would then publish
# with nobody's approval: trusted publishing included, since npm and
# crates.io match the environment by name. So a missing or unprotected one
# has to be caught before a publishing job starts, not by one.
#
# The list is read from publish.yml's own `environment:` lines, so a job
# added there is checked without anyone remembering to add it here.
#
# Needs gh with a token that can read this repository (GH_TOKEN) and
# GITHUB_REPOSITORY; exits 1 naming each environment that fails.
#
# Run by publish.yml's check-release before anything publishes, and by
# ci.yml's check-release-image, so a setting changed in the web UI shows as
# red on main rather than at the next release.
set -uo pipefail

workflow=.github/workflows/publish.yml
repo="${GITHUB_REPOSITORY:-pimlabs/recall}"

envs=$(sed -n 's/^ *environment: *\(release-[a-z0-9-]*\) *$/\1/p' "$workflow" | sort -u)
if [ -z "$envs" ]; then
  echo "::error::$workflow names no release-* environment; this check has nothing to check."
  exit 1
fi

bad=0
for env in $envs; do
  if ! json=$(gh api "repos/$repo/environments/$env" 2>&1); then
    echo "::error::Environment $env could not be read ($json). Create it as docs/reference/releasing.md's One-time setup says, before publishing."
    bad=1
    continue
  fi
  reviewers=$(jq '[.protection_rules[]? | select(.type == "required_reviewers") | .reviewers[]?] | length' <<<"$json")
  bypass=$(jq -r '.can_admins_bypass' <<<"$json")
  custom=$(jq -r '.deployment_branch_policy.custom_branch_policies // false' <<<"$json")
  tags=0
  if [ "$custom" = true ]; then
    tags=$(gh api "repos/$repo/environments/$env/deployment-branch-policies" \
      --jq '[.branch_policies[] | select(.type == "tag" and .name == "v*")] | length') || tags=0
  fi
  problems=
  [ "$reviewers" -gt 0 ] || problems="$problems no required reviewer;"
  [ "$bypass" != true ] || problems="$problems administrators may bypass it;"
  [ "$tags" -gt 0 ] || problems="$problems no deployment tag rule v*;"
  if [ -n "$problems" ]; then
    echo "::error::Environment $env is not protected:$problems see docs/reference/releasing.md, One-time setup."
    bad=1
  else
    echo "$env: $reviewers required reviewer(s), no admin bypass, v* tags only"
  fi
done
exit "$bad"
