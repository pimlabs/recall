//! The identity Recall syncs under.
//!
//! This is deliberately NOT the same derivation Claude Code uses for its own
//! memory scoping (see [`crate::claude`]): that one is the local filesystem
//! path, which differs on every machine and every clone. Recall needs a key
//! two environments agree on without ever having met, so it comes from the
//! git remote instead. The two solve different problems and are meant to
//! disagree; see docs/phase-0-findings.md §6.

use crate::claude::slug;

/// Normalizes a git remote URL to `owner/repo`, lowercased. Returns `None`
/// when nothing usable can be derived.
///
/// Only the last two path segments are used, which is what makes SSH,
/// HTTPS, and the locally-proxied form agree. That proxied form is not
/// hypothetical: cloud sandboxes rewrite origin to something like
/// `http://local_proxy@127.0.0.1:41729/git/owner/repo`, with a port that
/// changes every session — parsing host+path would break cross-machine
/// agreement outright.
///
/// The cost of taking only the last two segments is that nested groups
/// collapse: `gitlab.com/some-group/sub-group/repo` keys as
/// `sub-group/repo`. That is a known limitation, not an oversight — two
/// sibling subgroups with same-named repos would collide.
///
/// These keys are load-bearing for data continuity: a project's synced
/// history lives under its key on the server, so any change here orphans it.
pub fn key_from_remote(remote_url: &str) -> Option<String> {
    // Trailing slashes first, then the `.git` suffix, then any slash it was
    // hiding — so "…/repo.git/" normalizes the same as "…/repo".
    let trimmed = remote_url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');

    // Segments are split on both "/" and ":" so the SSH form
    // (git@host:owner/repo) yields the same pair as the HTTPS one. Splitting
    // on ":" is also why an explicit port survives: in
    // "ssh://git@host:22/owner/repo" the "22" becomes its own segment and
    // falls out of the last two, instead of being read as a path segment.
    let mut owner = None;
    let mut repo = None;
    for segment in trimmed.split(['/', ':']).filter(|s| !s.is_empty()) {
        owner = repo;
        repo = Some(segment);
    }

    Some(format!("{}/{}", owner?, repo?).to_lowercase())
}

/// The fallback for a project with no git remote at all. Two clones in
/// different directories will disagree, which is a real and documented
/// limitation rather than something this can solve: without a remote there
/// is nothing stable to agree on.
pub fn local_key(project_root: &str) -> String {
    format!("local:{}", slug(project_root))
}

/// Prefers the remote-derived identity and falls back to the local one. An
/// empty or unusable `remote_url` is the no-remote case.
pub fn key(remote_url: &str, project_root: &str) -> String {
    key_from_remote(remote_url).unwrap_or_else(|| local_key(project_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first three rows are the exact forms verified live in
    /// docs/phase-0-findings.md and must keep producing what the shell
    /// implementation produced.
    #[test]
    fn normalizes_every_remote_form_to_the_same_key() {
        for (input, want, why) in [
            (
                "http://local_proxy@127.0.0.1:41729/git/pimlabs/recall",
                Some("pimlabs/recall"),
                "proxied remote a cloud sandbox rewrites origin to",
            ),
            (
                "git@github.com:pimlabs/recall.git",
                Some("pimlabs/recall"),
                "ssh form",
            ),
            (
                "https://github.com/pimlabs/recall.git",
                Some("pimlabs/recall"),
                "https form",
            ),
            (
                "ssh://git@github.com/pimlabs/recall",
                Some("pimlabs/recall"),
                "the three forms above must all agree — that is the point",
            ),
            (
                "ssh://git@github.com:22/pimlabs/recall.git",
                Some("pimlabs/recall"),
                "port number must not be mistaken for a path segment",
            ),
            (
                "https://github.com/pimlabs/recall/",
                Some("pimlabs/recall"),
                "trailing slash",
            ),
            (
                "https://github.com/pimlabs/recall.git/",
                Some("pimlabs/recall"),
                "trailing slash after .git",
            ),
            (
                "git@github.com:PimLabs/Recall.git",
                Some("pimlabs/recall"),
                "case is normalized so two clones can't disagree",
            ),
            (
                "  git@github.com:pimlabs/recall.git\n",
                Some("pimlabs/recall"),
                "surrounding whitespace from `git remote get-url` output",
            ),
            (
                "https://gitlab.com/some-group/sub-group/repo.git",
                Some("sub-group/repo"),
                "nested groups collapse to the last two segments — a known, documented limitation",
            ),
            ("", None, "empty input derives nothing"),
            ("repo", None, "single segment derives nothing"),
        ] {
            assert_eq!(
                key_from_remote(input).as_deref(),
                want,
                "{why}: key_from_remote({input:?})"
            );
        }
    }

    #[test]
    fn falls_back_to_the_local_key_without_a_remote() {
        assert_eq!(key("", "/home/user/scratch"), "local:-home-user-scratch");
    }

    #[test]
    fn prefers_the_remote_over_the_path() {
        assert_eq!(
            key("git@github.com:pimlabs/recall.git", "/anywhere/at/all"),
            "pimlabs/recall",
            "the whole point is the path must not affect it"
        );
    }

    /// Two machines with the same repo checked out at different paths, and
    /// different remote URL shapes, must land on the same key — otherwise
    /// sync silently splits into two histories. This agreement is the entire
    /// reason the key is derived here and not from [`crate::claude::slug`].
    #[test]
    fn a_laptop_and_a_cloud_sandbox_agree() {
        let laptop = key(
            "git@github.com:pimlabs/recall.git",
            "/Users/eko/code/recall",
        );
        let cloud = key(
            "http://local_proxy@127.0.0.1:9999/git/pimlabs/recall",
            "/home/user/recall",
        );
        assert_eq!(laptop, cloud);
        assert_ne!(
            local_key("/Users/eko/code/recall"),
            local_key("/home/user/recall"),
            "the local fallback is exactly what cannot agree across machines"
        );
    }
}
