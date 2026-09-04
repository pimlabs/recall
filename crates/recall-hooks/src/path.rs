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
}
