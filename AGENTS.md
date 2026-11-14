# AGENTS.md

Conventions for agents working in this repo that `CLAUDE.md` and `CONTRIBUTING.md` do not cover.

## Branch names

| work | branch | example |
|---|---|---|
| everything else | `<type>/<slug>`, type one of `feat`, `fix`, `refactor`, `docs`, `chore`, `ci` | `fix/refused-push-retry` |
| a batch tested together before `main` | `integration/<yyyy-mm-dd>` | `integration/2026-11-16` |
| the version bump of a release | `release-<version>` | `release-0.4.18` |
| the Formula pull request after a release | `formula-<version>` | `formula-0.4.18` |

The two release branches keep their own shape because every release since 0.4.9 has used it, and
both pull requests are fixed steps of `docs/reference/releasing.md` whose title already carries the
`release:` area. Dependabot names its own `dependabot/...` branches; leave them as they are.

The desktop app's `claude/*` names and the Agent and Workflow tools' `worktree-*` names are
placeholders. Rename a `claude/*` branch with `git branch -m <type>/<slug>` before its first
commit, and never push a `worktree-*` branch: merge it into your own branch, then remove it with
its worktree.

## CI runs once per batch, and only when dispatched

No workflow starts on a push or a pull request: `ci.yml` and `audit-dependencies.yml` run on
`workflow_dispatch` (plus the audit's weekly schedule, which `audit-dependencies.yml` explains),
and the release workflows start from Start a release. Opening a pull request, pushing to it and
merging it start nothing. Each tree is tested once, the tree that lands on `main`:

1. **Cut an integration branch from `main`:** `git switch -c integration/<yyyy-mm-dd> origin/main`.
2. **Merge the batch's pull requests into it, in order, with `--no-ff`:**
   `git merge --no-ff origin/<pr-branch>` for each. A conflict is fixed on the pull request's own
   branch, never in the integration branch, or `main` will not end up with the tree that was
   tested. Size the batch so N × p stays below 1 (N pull requests, p the share of recent batch runs
   that came back red); with no history yet, about five.
3. **Run the local gate on the tip**, `.githooks/pre-push` (tests too, `cargo test --workspace`,
   when the batch changes code), then push the integration branch.
4. **One dispatch:** `gh workflow run ci.yml --ref integration/<yyyy-mm-dd>`, asked of the owner
   first with the workflow, the ref, the estimated billed minutes and the month's usage so far.
   Working sessions never dispatch; they hand a ready branch to the orchestrator. The batch is
   green when that run's `ci-passed` is.
5. **On green, merge the same pull requests into `main` in the same order** (squash, as
   `CLAUDE.md` says), then check that `main` holds the tested tree:
   `git fetch origin && git diff origin/integration/<yyyy-mm-dd> origin/main` prints nothing. If it
   prints anything, the tree on `main` was never tested: dispatch CI on `main` before anything is
   released from it.

**A red batch is bisected locally first:** reproduce the failure on the integration branch, then
`git bisect start --first-parent <integration tip> origin/main` and `git bisect run <gate>`, one
pull request per step and no minutes spent. Only a failure this machine cannot reproduce (Windows,
a clean runner) is bisected with dispatches, halving the batch each time, each one asked for.
Drop the pull request that broke it, rebuild the integration branch without it, dispatch again.

**A Dependabot pull request gets no run of its own.** It joins a batch like any other: its
`dependabot/...` branch is merged into the integration branch, and the local gate in step 3 runs
over it, since Dependabot never runs the pre-push hook. Its changes are usually commit SHAs of
actions; one in `ci.yml` makes the batch run every job, but one used only by `build-release.yml`,
`publish.yml` or `deploy-release.yml` is first exercised by the next release.

**Before a release**, Start a release looks for the newest dispatched CI run whose commit has the
tree `main` has now, and refuses unless it is green and skipped no image or Windows job. A batch
merged as above satisfies that while the repository is public. Otherwise, dispatch CI on `main`,
which runs every job, including `check-release-image` and with it
`scripts/check-release-environments.sh`.
