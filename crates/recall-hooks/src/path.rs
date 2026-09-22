//! Deciding whether a path is a memory file, and what to call it.
//!
//! This is the security boundary in both directions. On the way out it
//! decides what leaves the machine; on the way in it decides where a pulled
//! file is allowed to land. Both answers are computed lexically, never by
//! touching the filesystem — the memory directory may not exist yet on a
//! first pull, and resolving symlinks here would let a link inside the
//! directory decide what counts as being inside it.

use std::path::{Component, Path, PathBuf};

/// Whether `path` is a memory file of the memory directory at `dir`.
///
/// Public because a caller has to be able to answer this *before* it has a
/// server URL or a token: the push hook runs on every Edit and Write in a
/// session, and on a machine that has cloned a wired project but not yet
/// been configured, demanding configuration first would turn every unrelated
/// edit into a hook error.
pub fn is_memory_file(dir: &Path, path: &Path) -> bool {
    is_under(dir, path)
}

/// The project slug of a memory file that belongs to a *different* project
/// under the same Claude Code root — [`None`] when `path` is this project's
/// memory, or is not memory at all.
///
/// Exists because those two are not the same kind of "no". A push hook that
/// skips an unrelated source file should say nothing; one that skips a
/// memory file because the caller happens to be standing somewhere else
/// should say so. The difference cost three edits to find: a git worktree
/// has its own project root, so its slug differs even though `project_key`
/// — derived from the git remote, which a worktree shares — is identical.
/// The hook exited 0 without a word, and the next pull restored the server's
/// older copy over the edit.
pub fn foreign_memory_slug(root: &Path, mine: &Path, path: &Path) -> Option<String> {
    if is_under(mine, path) {
        return None;
    }
    let rel = relative_slash(&root.join("projects"), path)?;
    let mut parts = rel.split('/');
    let slug = parts.next()?;
    // `<slug>/memory/<file>` — anything shorter is the project directory
    // itself rather than something inside its memory.
    if parts.next()? != "memory" || parts.next().is_none() {
        return None;
    }
    Some(slug.to_string())
}

/// Reports whether `path` sits strictly inside `dir`.
///
/// Compared segment-wise after a lexical clean, so `/a/memory-notes` is
/// correctly *not* treated as inside `/a/memory`. A plain string-prefix
/// check gets that wrong, and the shell version did.
pub(crate) fn is_under(dir: &Path, path: &Path) -> bool {
    relative_slash(dir, path).is_some()
}

/// The slash-separated path of `path` relative to `dir`, or `None` if it
/// isn't strictly inside it (equal counts as not inside).
///
/// Slashes rather than the platform separator because this string becomes
/// the wire's `file_path`, which a different machine will re-join.
pub(crate) fn relative_slash(dir: &Path, path: &Path) -> Option<String> {
    let dir = lexical_clean(dir);
    let path = lexical_clean(path);
    let rel = path.strip_prefix(&dir).ok()?;

    let mut out = String::new();
    for component in rel.components() {
        let Component::Normal(segment) = component else {
            // A `..` or a root that survived cleaning means this isn't a
            // plain descendant, whatever the prefix match said.
            return None;
        };
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&segment.to_string_lossy());
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Resolves `.` and `..` textually, without touching the filesystem —
/// the equivalent of Go's `filepath.Clean`.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the root is a no-op, as `/..` is `/`.
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containment_is_segment_wise() {
        let dir = Path::new("/a/memory");
        for inside in [
            "/a/memory/MEMORY.md",
            "/a/memory/topics/auth.md",
            "/a/./memory/MEMORY.md",
            "/a/memory/topics/../MEMORY.md",
        ] {
            assert!(
                is_under(dir, Path::new(inside)),
                "{inside} should be inside"
            );
        }
        for outside in [
            "/a/memory-notes/x.md",
            "/a/memoryx",
            "/a/memory",
            "/a/other/MEMORY.md",
            "/a/memory/../escaped.md",
            "/",
        ] {
            assert!(
                !is_under(dir, Path::new(outside)),
                "{outside} should be outside"
            );
        }
    }

    #[test]
    fn a_relative_path_uses_forward_slashes_whatever_the_platform() {
        let rel = relative_slash(
            Path::new("/a/memory"),
            Path::new("/a/memory/topics/auth/tokens.md"),
        );
        assert_eq!(rel.as_deref(), Some("topics/auth/tokens.md"));
    }

    #[test]
    fn a_memory_file_of_this_project_is_not_foreign() {
        let root = Path::new("/h/.claude");
        let mine = Path::new("/h/.claude/projects/-w-app/memory");
        assert_eq!(
            foreign_memory_slug(
                root,
                mine,
                Path::new("/h/.claude/projects/-w-app/memory/a.md")
            ),
            None
        );
    }

    /// The case that cost three edits: a git worktree has its own project
    /// root, so its slug differs — while `project_key` stays identical,
    /// because that comes from the git remote a worktree shares.
    #[test]
    fn memory_of_another_project_names_the_slug() {
        let root = Path::new("/h/.claude");
        let mine = Path::new("/h/.claude/projects/-w-app--claude-worktrees-x/memory");
        assert_eq!(
            foreign_memory_slug(
                root,
                mine,
                Path::new("/h/.claude/projects/-w-app/memory/a.md")
            ),
            Some("-w-app".to_string())
        );
    }

    /// Everything else must stay silent, or the push hook starts commenting
    /// on every file touched in a session.
    #[test]
    fn an_ordinary_file_is_not_foreign_memory() {
        let root = Path::new("/h/.claude");
        let mine = Path::new("/h/.claude/projects/-w-app/memory");
        for path in [
            "/w/app/src/main.rs",
            "/h/.claude/settings.json",
            // Under projects/, but not inside anyone's memory.
            "/h/.claude/projects/-w-other/notes.md",
            // The memory directory itself, with nothing inside it named.
            "/h/.claude/projects/-w-other/memory",
        ] {
            assert_eq!(
                foreign_memory_slug(root, mine, Path::new(path)),
                None,
                "{path}"
            );
        }
    }

    /// `memory-notes` is not `memory`, the same trap `is_under` exists for.
    #[test]
    fn a_directory_merely_starting_with_memory_is_not_it() {
        let root = Path::new("/h/.claude");
        let mine = Path::new("/h/.claude/projects/-w-app/memory");
        assert_eq!(
            foreign_memory_slug(
                root,
                mine,
                Path::new("/h/.claude/projects/-w-other/memory-notes/a.md")
            ),
            None
        );
    }
}
