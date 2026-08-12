#!/usr/bin/env bash
# Shared helpers for recall-push / recall-pull. Sourced, not executed directly.
#
# recall_memory_dir() reproduces Claude Code's own algorithm for where it
# reads/writes auto-memory files, empirically confirmed against the
# installed CLI (v2.1.42) source:
#   root  = $CLAUDE_CODE_REMOTE_MEMORY_DIR, else $CLAUDE_CONFIG_DIR, else ~/.claude
#   slug  = git root (or cwd if no git root), with every non-alnum char -> "-"
#   dir   = $root/projects/$slug/memory
#
# recall_project_key() is deliberately NOT the same derivation — Claude
# Code's own scoping is local-filesystem-path-based, which differs between
# every machine/clone. Recall needs a key that's the same on a laptop and a
# fresh cloud clone of the same repo, so it derives from the git remote's
# owner/repo instead (see comment below for why only the last two path
# segments are used).

recall_memory_root() {
  if [[ -n "${CLAUDE_CODE_REMOTE_MEMORY_DIR:-}" ]]; then
    printf '%s' "$CLAUDE_CODE_REMOTE_MEMORY_DIR"
  else
    printf '%s' "${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  fi
}

recall_project_root() {
  git rev-parse --show-toplevel 2>/dev/null || pwd
}

recall_slug() {
  printf '%s' "$1" | sed 's/[^a-zA-Z0-9]/-/g'
}

recall_memory_dir() {
  printf '%s/projects/%s/memory' "$(recall_memory_root)" "$(recall_slug "$(recall_project_root)")"
}

recall_project_key() {
  local url segment
  url="$(git remote get-url origin 2>/dev/null || true)"
  if [[ -z "$url" ]]; then
    # No git remote at all — fall back to the local path slug. Two clones
    # without a remote will disagree; that's a documented Phase 0 limitation.
    printf 'local:%s' "$(recall_slug "$(recall_project_root)")"
    return
  fi
  url="${url%.git}"
  url="${url%/}"
  # Take the last two path segments (owner/repo). This is deliberately more
  # robust than parsing full host+path: it produces the same key for
  # git@github.com:owner/repo.git, https://github.com/owner/repo.git, AND
  # for the locally-proxied remote that cloud sandboxes rewrite origin to
  # (e.g. http://local_proxy@127.0.0.1:PORT/git/owner/repo) — confirmed
  # empirically in a real cloud session, where the proxy port is random per
  # session and would otherwise break cross-machine key agreement.
  # Known limitation: repos nested under sub-groups (GitLab) collapse to
  # their last two segments too. Acceptable for Phase 0; revisit in Phase 2.
  segment="$(printf '%s' "$url" | grep -oE '[^/:]+/[^/:]+$')"
  printf '%s' "${segment:-$(recall_slug "$url")}" | tr '[:upper:]' '[:lower:]'
}
