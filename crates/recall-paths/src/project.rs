//! The identity Recall syncs under.
//!
//! This is deliberately NOT the same derivation Claude Code uses for its own
//! memory scoping (see [`crate::claude`]): that one is the local filesystem
//! path, which differs on every machine and every clone. Recall needs a key
//! two environments agree on without ever having met, so it comes from the
//! git remote instead. The two solve different problems and are meant to
//! disagree; see docs/history/phase-0-findings.md §6.

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
/// sibling subgroups with same-named repos would collide. A project that
/// hits that collision can declare its key instead; see
/// [`key_with_override`].
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
/// different directories will disagree, and no *derivation* can fix that:
/// with no remote there is nothing about the checkout both machines can
/// see. Declaring a key with `RECALL_PROJECT_KEY` is the way out — see
/// [`key_with_override`] — but that is the user supplying the answer, not
/// Recall working it out.
pub fn local_key(project_root: &str) -> String {
    format!("local:{}", slug(project_root))
}

/// The derived key: the remote-derived identity, falling back to the local
/// one. An empty or unusable `remote_url` is the no-remote case.
///
/// [`key_with_override`] is this plus a key the project declared for itself;
/// this one stays for callers that want only the derivation.
pub fn key(remote_url: &str, project_root: &str) -> String {
    key_from_remote(remote_url).unwrap_or_else(|| local_key(project_root))
}

/// Normalises a key a project declared for itself, or [`None`] when the
/// value is unusable and the derivation should stand.
///
/// Trimming and lowercasing are exactly what [`key_from_remote`] does,
/// because both kinds of key land in one namespace on the server: declaring
/// `PimLabs/Recall` on one machine has to reach the same rows as the
/// remote-derived `pimlabs/recall` on another, or the declaration splits the
/// history it was meant to join.
///
/// Two values are refused rather than used:
///
/// - one containing whitespace or a control character. The server stores
///   `project_key` opaquely and the client percent-encodes it, so such a key
///   would in fact work — but it is a key nobody retypes identically on the
///   second machine, and two machines failing to agree is the outcome this
///   module exists to prevent.
/// - one under `global:`, the namespace
///   [`scope::global_key`](crate::scope::global_key) owns. A project keyed
///   there would push its own files into the bucket it shares with every
///   other project.
///
/// Refusing means falling back to the derived key: leaving an already-working
/// project exactly as it was is a safer answer to a malformed declaration
/// than a mangled key that quietly starts a second history.
///
/// ```
/// # use recall_paths::project::explicit_key;
/// assert_eq!(explicit_key(" PimLabs/Recall "), Some("pimlabs/recall".to_string()));
/// assert_eq!(explicit_key("   "), None);
/// assert_eq!(explicit_key("global:eko"), None);
/// ```
pub fn explicit_key(raw: &str) -> Option<String> {
    let key = raw.trim().to_lowercase();
    if key.is_empty() || key.starts_with("global:") {
        return None;
    }
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(key)
}

/// The key a project syncs under: the one it declared, else the derived one.
///
/// `explicit` is the raw `RECALL_PROJECT_KEY` value; [`explicit_key`] decides
/// whether it is usable. A usable declaration beats the remote, which is the
/// whole point — it covers the four cases no derivation reaches: a repo with
/// no remote at all, sub-projects in a monorepo that should (or should not)
/// share one history, a fork that wants to keep reading the upstream's
/// memory, and the nested-subgroup collision [`key_from_remote`] documents.
///
/// A declared key is as load-bearing as a derived one: the server files
/// memory under the key it was pushed with and moves nothing, so changing
/// the declaration strands the old history under the old key.
pub fn key_with_override(explicit: Option<&str>, remote_url: &str, project_root: &str) -> String {
    explicit
        .and_then(explicit_key)
        .unwrap_or_else(|| key(remote_url, project_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first three rows are the exact forms verified live in
    /// docs/history/phase-0-findings.md and must keep producing what the shell
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

    #[test]
    fn a_declared_key_wins_over_both_derivations() {
        assert_eq!(
            key_with_override(
                Some("acme/monorepo-api"),
                "git@github.com:acme/monorepo.git",
                "/src/monorepo/api"
            ),
            "acme/monorepo-api",
            "a remote to derive from does not get the last word"
        );
        assert_eq!(
            key_with_override(Some("acme/notes"), "", "/home/eko/notes"),
            "acme/notes",
            "and with no remote there is nothing to lose to"
        );
    }

    /// The declaration only works if it survives the trip between two
    /// machines that typed it slightly differently — and if it can name a key
    /// a remote elsewhere derives, since joining an existing history is one
    /// of the reasons to declare one.
    #[test]
    fn a_declared_key_is_normalised_like_a_derived_one() {
        let derived = key("git@github.com:PimLabs/Recall.git", "/anywhere");
        for declared in ["pimlabs/recall", "PimLabs/Recall", "  pimlabs/recall\n"] {
            assert_eq!(
                key_with_override(Some(declared), "", "/home/eko/fork"),
                derived,
                "for {declared:?}"
            );
        }
    }

    /// Every rejected form falls back to the derivation rather than keying
    /// the project somewhere unusable — an unset variable and a malformed one
    /// must land in the same place.
    #[test]
    fn an_unusable_declaration_leaves_the_derivation_standing() {
        let derived = key("git@github.com:pimlabs/recall.git", "/anywhere");
        for (declared, why) in [
            (None, "unset is the default and always has been"),
            (Some(""), "an empty variable is not a key"),
            (Some("   \t"), "nor is whitespace"),
            (
                Some("global:eko"),
                "the global namespace is not a project's to claim",
            ),
            (
                Some("acme/my project"),
                "a space is a key the other machine will not retype identically",
            ),
            (Some("acme/app\u{7}"), "control characters likewise"),
        ] {
            assert_eq!(
                key_with_override(declared, "git@github.com:pimlabs/recall.git", "/anywhere"),
                derived,
                "{why}"
            );
        }
    }

    /// The four cases the derivation cannot serve, each stated as the
    /// agreement (or disagreement) the project actually wants.
    #[test]
    fn declaring_a_key_covers_what_deriving_one_cannot() {
        // Sub-projects of one monorepo: same remote, deliberately separate
        // histories.
        let remote = "git@github.com:acme/monorepo.git";
        assert_ne!(
            key_with_override(Some("acme/monorepo-api"), remote, "/src/monorepo/api"),
            key_with_override(Some("acme/monorepo-web"), remote, "/src/monorepo/web"),
        );

        // A fork keeps reading the upstream's memory despite its own remote.
        assert_eq!(
            key_with_override(
                Some("pimlabs/recall"),
                "git@github.com:eko/recall-fork.git",
                "/home/eko/recall-fork"
            ),
            key("git@github.com:pimlabs/recall.git", "/elsewhere"),
        );

        // Two GitLab subgroups whose repos share a name, which
        // `key_from_remote` collapses onto each other.
        assert_ne!(
            key_with_override(
                Some("group/alpha-app"),
                "https://gitlab.com/group/alpha/app.git",
                "/src/alpha/app"
            ),
            key_with_override(
                Some("group/beta-app"),
                "https://gitlab.com/group/beta/app.git",
                "/src/beta/app"
            ),
        );

        // No remote anywhere, and two machines still agree — the thing
        // `local_key` cannot do.
        assert_eq!(
            key_with_override(Some("eko/notes"), "", "/Users/eko/notes"),
            key_with_override(Some("eko/notes"), "", "/home/user/notes"),
        );
    }
}
