//! What Recall syncs, and under which key.
//!
//! A **scope** pairs a `project_key` on the wire with a subtree of the local
//! memory directory. Until now there was exactly one — the project — and it
//! was implicit. Naming it makes room for the second (global preferences
//! that should follow you into every project) without the server learning a
//! new concept: a scope key is just another opaque `project_key`, so the
//! frozen HTTP surface and the SQLite schema are untouched.
//!
//! ```
//! # use recall_hooks::scope::{route, scopes, GLOBAL_DIR};
//! let s = scopes("acme/app".into(), Some("global:eko".into()));
//!
//! // A file at the root of the memory directory belongs to the project.
//! let (scope, path) = route(&s, "MEMORY.md").unwrap();
//! assert_eq!((scope.key.as_str(), path.as_str()), ("acme/app", "MEMORY.md"));
//!
//! // One under `global/` belongs to the global scope, and loses the prefix
//! // on the way out — the server stores it as a plain path.
//! let (scope, path) = route(&s, "global/editor.md").unwrap();
//! assert_eq!((scope.key.as_str(), path.as_str()), ("global:eko", "editor.md"));
//! # assert_eq!(GLOBAL_DIR, "global");
//! ```

/// The reserved subdirectory of the memory directory that holds globally
/// synced memories.
///
/// Reserved means exactly that: a project's own topic file may not live here,
/// because anything under it is pushed to the global scope instead. Chosen
/// over a hidden name so it is obvious in a directory listing what is
/// shared with every other project.
pub const GLOBAL_DIR: &str = "global";

/// One thing Recall syncs: a key on the wire, and where it lives locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The `project_key` this scope's files are stored under.
    pub key: String,
    /// The subdirectory of the memory directory this scope owns, or [`None`]
    /// for the memory directory itself.
    ///
    /// Paths are relative to the *scope*, not to the memory directory, so a
    /// file stored as `editor.md` in the global scope lands at
    /// `global/editor.md` on disk. That keeps the wire format free of
    /// Recall's local layout: move the directory tomorrow and every stored
    /// row is still correct.
    pub prefix: Option<String>,
}

impl Scope {
    /// The scope for one repository, rooted at the memory directory itself.
    pub fn project(key: impl Into<String>) -> Self {
        Scope {
            key: key.into(),
            prefix: None,
        }
    }

    /// The scope that follows the user into every project.
    pub fn global(key: impl Into<String>) -> Self {
        Scope {
            key: key.into(),
            prefix: Some(GLOBAL_DIR.to_string()),
        }
    }

    /// Whether this is the global scope.
    pub fn is_global(&self) -> bool {
        self.prefix.as_deref() == Some(GLOBAL_DIR)
    }

    /// The memory-directory-relative path of a file this scope stores as
    /// `path`.
    pub fn local_path(&self, path: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}/{path}"),
            None => path.to_string(),
        }
    }
}

/// Every scope in effect, most specific first.
///
/// Order is load-bearing: [`route`] takes the first match, and the project
/// scope matches everything, so it has to come last.
pub fn scopes(project_key: String, global_key: Option<String>) -> Vec<Scope> {
    let mut out = Vec::new();
    if let Some(key) = global_key {
        out.push(Scope::global(key));
    }
    out.push(Scope::project(project_key));
    out
}

/// Normalises whatever the user put in `RECALL_GLOBAL_KEY` into a key that
/// cannot collide with a repository's.
///
/// A project key is `owner/repo`. Prefixing with `global:` keeps the two
/// namespaces apart on a server that stores both, and makes a stray key
/// obvious in `/admin/stats`. A value that already carries the prefix is
/// left alone, so setting the variable to what `recall status` printed does
/// the expected thing.
///
/// ```
/// # use recall_hooks::scope::global_key;
/// assert_eq!(global_key("eko"), Some("global:eko".to_string()));
/// assert_eq!(global_key("global:eko"), Some("global:eko".to_string()));
/// assert_eq!(global_key("  "), None);
/// ```
pub fn global_key(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with("global:") {
        return Some(raw.to_string());
    }
    Some(format!("global:{raw}"))
}

/// The first segment of `rel` when it names the reserved global directory
/// in a casing other than the canonical one.
///
/// This exists because the answer depends on a filesystem Recall cannot see.
/// macOS's default is case-insensitive, so a directory created as `Global/`
/// **is** the global directory there — one inode, and `read_dir` reports the
/// name it happened to be created with. On Linux, where the cloud sessions
/// run, `Global/` is a different directory that merely looks like this one.
/// Both readings are defensible and they route the same file to different
/// keys, so [`route`] refuses rather than guess. This names the directory so
/// the refusal can say which one it meant.
///
/// ASCII case folding on purpose. [`GLOBAL_DIR`] is ASCII, and full Unicode
/// folding brings in cases — the Kelvin sign, the dotless i — that have no
/// business deciding whose history someone's notes land in.
///
/// ```
/// # use recall_hooks::scope::miscased_global_dir;
/// assert_eq!(miscased_global_dir("Global/editor.md"), Some("Global"));
/// assert_eq!(miscased_global_dir("global/editor.md"), None); // canonical
/// assert_eq!(miscased_global_dir("globals/thing.md"), None); // another word
/// ```
pub fn miscased_global_dir(rel: &str) -> Option<&str> {
    let head = rel.split('/').next()?;
    (head != GLOBAL_DIR && head.eq_ignore_ascii_case(GLOBAL_DIR)).then_some(head)
}

/// Which scope owns `rel`, and what that scope calls it.
///
/// `rel` is relative to the memory directory, slash-separated. Returns
/// [`None`] for a path that belongs to no scope. Two things land there, and
/// both must *not* fall through to the project scope — pushing someone's
/// global notes into one repository's history is the one outcome worth
/// refusing:
///
/// - the global directory itself, and anything under it while global sync is
///   off;
/// - a directory that names the global one in the wrong case, which on a
///   case-insensitive filesystem *is* that directory — see
///   [`miscased_global_dir`].
pub fn route<'a>(scopes: &'a [Scope], rel: &str) -> Option<(&'a Scope, String)> {
    // Ahead of the loop, because this has to hold whether global sync is on
    // or off. With it on, a miscased directory missed the global prefix and
    // fell through; with it off, it missed the guard below. Both landed in
    // the project scope, which is the one outcome this module exists to
    // refuse. See [`miscased_global_dir`] for why refusing beats guessing.
    if miscased_global_dir(rel).is_some() {
        return None;
    }
    for scope in scopes {
        let Some(prefix) = &scope.prefix else {
            // The project scope matches anything left, except the global
            // directory — see above.
            if rel == GLOBAL_DIR || rel.starts_with(&format!("{GLOBAL_DIR}/")) {
                return None;
            }
            return Some((scope, rel.to_string()));
        };
        if let Some(rest) = rel.strip_prefix(&format!("{prefix}/")) {
            if rest.is_empty() {
                return None;
            }
            return Some((scope, rest.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn both() -> Vec<Scope> {
        scopes("acme/app".into(), Some("global:eko".into()))
    }

    #[test]
    fn project_files_route_to_the_project() {
        let s = both();
        for rel in [
            "MEMORY.md",
            "topics/auth.md",
            "a/b/c/deep.md",
            "globalish.md",
        ] {
            let (scope, path) = route(&s, rel).expect("should route");
            assert_eq!(scope.key, "acme/app", "for {rel}");
            assert_eq!(path, rel, "for {rel}");
        }
    }

    #[test]
    fn global_files_route_to_the_global_scope_without_the_prefix() {
        let s = both();
        let (scope, path) = route(&s, "global/editor.md").unwrap();
        assert_eq!(scope.key, "global:eko");
        assert_eq!(path, "editor.md");

        // Nesting inside the global scope is allowed and survives.
        let (_, path) = route(&s, "global/prefs/editor.md").unwrap();
        assert_eq!(path, "prefs/editor.md");
    }

    /// A name that merely starts with the reserved word is not in it.
    #[test]
    fn a_prefix_match_is_not_a_directory_match() {
        let s = both();
        let (scope, path) = route(&s, "globals/thing.md").unwrap();
        assert_eq!(scope.key, "acme/app");
        assert_eq!(path, "globals/thing.md");
    }

    /// The directory itself is not a file in any scope.
    #[test]
    fn the_global_directory_itself_routes_nowhere() {
        let s = both();
        assert!(route(&s, "global").is_none());
        assert!(route(&s, "global/").is_none());
    }

    /// The case that would leak someone's global notes into one repo's
    /// history: global sync off, but a `global/` directory left on disk from
    /// when it was on. It must be ignored, not swept into the project.
    #[test]
    fn with_global_off_the_global_directory_is_ignored_not_absorbed() {
        let only_project = scopes("acme/app".into(), None);
        assert!(route(&only_project, "global/editor.md").is_none());
        assert!(route(&only_project, "global").is_none());

        // Everything else still routes normally.
        let (scope, path) = route(&only_project, "MEMORY.md").unwrap();
        assert_eq!(
            (scope.key.as_str(), path.as_str()),
            ("acme/app", "MEMORY.md")
        );
    }

    /// The finding this guard was added for: on macOS a directory created as
    /// `Global/` is the global directory, and Recall was filing everything
    /// under it into the repository's own history instead — with global sync
    /// on *or* off, which is what made it worth refusing rather than
    /// re-routing.
    #[test]
    fn a_miscased_global_directory_is_never_swept_into_the_project() {
        let on = both();
        let off = scopes("acme/app".into(), None);

        for rel in [
            "Global/editor.md",
            "GLOBAL/editor.md",
            "gLoBaL/deep/nested.md",
            "Global",
        ] {
            assert!(route(&on, rel).is_none(), "global on, for {rel}");
            assert!(route(&off, rel).is_none(), "global off, for {rel}");
        }
    }

    /// The refusal is narrow on purpose: it is the *directory name*, not any
    /// word that resembles it, and not a file.
    #[test]
    fn only_the_directory_name_is_reserved_in_any_case() {
        let s = both();
        for rel in [
            "Globalish.md",
            "Globals/thing.md",
            "Global.md",
            "a/Global/b.md",
        ] {
            let (scope, path) = route(&s, rel).expect("should still route");
            assert_eq!(scope.key, "acme/app", "for {rel}");
            assert_eq!(path, rel, "for {rel}");
        }
    }

    #[test]
    fn miscased_global_dir_names_the_directory_it_refused() {
        assert_eq!(miscased_global_dir("Global/editor.md"), Some("Global"));
        assert_eq!(miscased_global_dir("GLOBAL"), Some("GLOBAL"));
        // The canonical one is not miscased, and neither is another word.
        assert_eq!(miscased_global_dir("global/editor.md"), None);
        assert_eq!(miscased_global_dir("globals/thing.md"), None);
        assert_eq!(miscased_global_dir(""), None);
    }

    #[test]
    fn local_path_is_the_inverse_of_route() {
        let s = both();
        for rel in [
            "MEMORY.md",
            "topics/auth.md",
            "global/editor.md",
            "global/a/b.md",
        ] {
            let (scope, path) = route(&s, rel).unwrap();
            assert_eq!(scope.local_path(&path), rel, "for {rel}");
        }
    }

    #[test]
    fn scope_order_puts_the_catch_all_last() {
        let s = both();
        assert!(s[0].is_global(), "global must be matched first");
        assert!(s[1].prefix.is_none(), "the project scope matches anything");
        assert_eq!(scopes("acme/app".into(), None).len(), 1);
    }

    #[test]
    fn a_global_key_cannot_be_mistaken_for_a_repository() {
        assert_eq!(global_key("eko"), Some("global:eko".into()));
        assert_eq!(global_key("global:eko"), Some("global:eko".into()));
        // Something that looks like a repo still gets namespaced.
        assert_eq!(global_key("acme/app"), Some("global:acme/app".into()));
        for empty in ["", "   ", "\t"] {
            assert_eq!(global_key(empty), None, "for {empty:?}");
        }
    }
}
