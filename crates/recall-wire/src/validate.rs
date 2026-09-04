//! The rules both halves apply to a request, so neither can drift from the
//! other.

/// Why a request was rejected.
///
/// Shared so the server (refusing a request) and the client (refusing to
/// send one) act on the same reasons and produce the same wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// No `project_key`: there is nothing to file this under.
    #[error("project_key is required")]
    MissingProjectKey,
    /// No `file_path`.
    #[error("file_path is required")]
    MissingFilePath,
    /// An absolute path, including a Windows drive prefix.
    #[error("file_path must be relative")]
    FilePathAbsolute,
    /// A `..` segment, which would escape the memory directory.
    #[error("file_path must not contain a .. segment")]
    FilePathTraversal,
}

/// Enforces that a `file_path` is safe to join onto a memory directory on
/// any machine that later pulls it.
///
/// A pulled file is written to disk by whoever fetches it, so a bad path
/// here is not merely invalid data — it is a write outside the memory
/// directory on someone else's machine. Hence checking on the way in
/// (server) as well as on the way out (client).
///
/// Rejection is per-segment, not by substring: a filename like `..config.md`
/// is perfectly legitimate and must not be caught, while an `a/../../b`
/// segment must be. (The Node server used a substring check and wrongly
/// rejected the former.)
///
/// ```
/// # use recall_wire::{validate_file_path, ValidationError};
/// assert!(validate_file_path("topics/auth/tokens.md").is_ok());
/// assert!(validate_file_path("..config.md").is_ok());
/// assert_eq!(
///     validate_file_path("../outside.md"),
///     Err(ValidationError::FilePathTraversal),
/// );
/// ```
pub fn validate_file_path(path: &str) -> Result<(), ValidationError> {
    if path.is_empty() {
        return Err(ValidationError::MissingFilePath);
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(ValidationError::FilePathAbsolute);
    }
    // A Windows drive prefix ("C:...") is absolute too, and anything that
    // later joins this path would treat it that way.
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return Err(ValidationError::FilePathAbsolute);
    }
    if path.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(ValidationError::FilePathTraversal);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_file_paths() {
        for ok in [
            "MEMORY.md",
            "debugging.md",
            "topics/auth/tokens.md",
            ".hidden.md",
            // Leading dots in a filename are not traversal. The Node server's
            // substring check wrongly rejected this.
            "..config.md",
        ] {
            assert!(validate_file_path(ok).is_ok(), "{ok} should be accepted");
        }

        for (path, want) in [
            ("", ValidationError::MissingFilePath),
            ("/etc/passwd", ValidationError::FilePathAbsolute),
            ("C:/Windows/system32", ValidationError::FilePathAbsolute),
            (r"\etc\passwd", ValidationError::FilePathAbsolute),
            ("../outside.md", ValidationError::FilePathTraversal),
            (
                "topics/../../outside.md",
                ValidationError::FilePathTraversal,
            ),
            (
                r"topics\..\..\outside.md",
                ValidationError::FilePathTraversal,
            ),
            ("..", ValidationError::FilePathTraversal),
        ] {
            assert_eq!(validate_file_path(path), Err(want), "for {path:?}");
        }
    }
}
