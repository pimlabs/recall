// Package project derives the identity Recall syncs under.
//
// This is deliberately NOT the same derivation Claude Code uses for its own
// memory scoping (see internal/claudecode): that one is the local
// filesystem path, which differs on every machine and every clone. Recall
// needs a key two environments agree on without ever having met, so it
// comes from the git remote instead. The two solve different problems and
// are meant to disagree; see docs/phase-0-findings.md §6.
package project

import (
	"strings"

	"github.com/pimlabs/recall/internal/claudecode"
)

// KeyFromRemote normalizes a git remote URL to "owner/repo", lowercased.
// Returns "" when nothing usable can be derived.
//
// Only the last two path segments are used, which is what makes SSH,
// HTTPS, and the locally-proxied form agree. That proxied form is not
// hypothetical: cloud sandboxes rewrite origin to something like
// http://local_proxy@127.0.0.1:41729/git/owner/repo, with a port that
// changes every session — parsing host+path would break cross-machine
// agreement outright.
func KeyFromRemote(remoteURL string) string {
	s := strings.TrimSpace(remoteURL)
	if s == "" {
		return ""
	}

	// Trailing slashes first, then the .git suffix, then any slash it was
	// hiding — so "…/repo.git/" normalizes the same as "…/repo".
	s = strings.TrimRight(s, "/")
	s = strings.TrimSuffix(s, ".git")
	s = strings.TrimRight(s, "/")

	// Segments are split on both "/" and ":" so the SSH form
	// (git@host:owner/repo) yields the same pair as the HTTPS one.
	fields := strings.FieldsFunc(s, func(r rune) bool {
		return r == '/' || r == ':'
	})
	if len(fields) < 2 {
		return ""
	}

	owner, repo := fields[len(fields)-2], fields[len(fields)-1]
	if owner == "" || repo == "" {
		return ""
	}
	return strings.ToLower(owner + "/" + repo)
}

// LocalKey is the fallback for a project with no git remote at all. Two
// clones in different directories will disagree, which is a real and
// documented limitation rather than something this can solve: without a
// remote there is nothing stable to agree on.
func LocalKey(projectRoot string) string {
	return "local:" + claudecode.Slug(projectRoot)
}

// Key prefers the remote-derived identity and falls back to the local one.
func Key(remoteURL, projectRoot string) string {
	if k := KeyFromRemote(remoteURL); k != "" {
		return k
	}
	return LocalKey(projectRoot)
}
