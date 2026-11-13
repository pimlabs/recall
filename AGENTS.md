# AGENTS.md

Conventions for agents working in this repo that `CLAUDE.md` and `CONTRIBUTING.md` do not cover.

## Branch names

| work | branch | example |
|---|---|---|
| everything else | `<type>/<slug>`, type one of `feat`, `fix`, `refactor`, `docs`, `chore` | `fix/refused-push-retry` |
| the version bump of a release | `release-<version>` | `release-0.4.18` |
| the Formula pull request after a release | `formula-<version>` | `formula-0.4.18` |

The two release branches keep their own shape because every release since 0.4.9 has used it, and
both pull requests are fixed steps of `docs/reference/releasing.md` whose title already carries the
`release:` area. Dependabot names its own `dependabot/...` branches; leave them as they are.

The desktop app's `claude/*` names and the Agent and Workflow tools' `worktree-*` names are
placeholders. Rename a `claude/*` branch with `git branch -m <type>/<slug>` before its first
commit, and never push a `worktree-*` branch: merge it into your own branch, then remove it with
its worktree.
