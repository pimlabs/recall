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
    /// A `project_key` longer than [`MAX_PROJECT_KEY_BYTES`].
    #[error("project_key must be at most 4096 bytes")]
    ProjectKeyTooLong,
    /// A `file_path` longer than [`MAX_FILE_PATH_BYTES`].
    #[error("file_path must be at most 4096 bytes")]
    FilePathTooLong,
    /// A `base_sha256` that is not a SHA-256 in hex.
    #[error("base_sha256 must be 64 hexadecimal characters")]
    BaseSha256,
}

/// The longest `project_key` either side accepts, in bytes: `PATH_MAX`, so
/// a key derived from a checkout's path always fits, while every push and
/// pull's audit leaf, which names its project, stays small.
pub const MAX_PROJECT_KEY_BYTES: usize = 4096;

/// The longest `file_path` either side accepts, in bytes: `PATH_MAX`, for
/// the same two reasons.
pub const MAX_FILE_PATH_BYTES: usize = 4096;

/// Enforces that a `project_key` is present and no longer than
/// [`MAX_PROJECT_KEY_BYTES`].
pub fn validate_project_key(key: &str) -> Result<(), ValidationError> {
    if key.is_empty() {
        return Err(ValidationError::MissingProjectKey);
    }
    if key.len() > MAX_PROJECT_KEY_BYTES {
        return Err(ValidationError::ProjectKeyTooLong);
    }
    Ok(())
}

/// Enforces that a `base_sha256` is what [`crate::content_sha256`] writes: 64
/// hex digits. Either case is accepted, as the server compares them without
/// regard to case.
pub fn validate_base_sha256(base: &str) -> Result<(), ValidationError> {
    if base.len() == 64 && base.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ValidationError::BaseSha256)
    }
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
    if path.len() > MAX_FILE_PATH_BYTES {
        return Err(ValidationError::FilePathTooLong);
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
        let longest = "a".repeat(MAX_FILE_PATH_BYTES);
        assert!(validate_file_path(&longest).is_ok());
        assert_eq!(
            validate_file_path(&format!("{longest}b")),
            Err(ValidationError::FilePathTooLong)
        );
    }

    #[test]
    fn validates_project_keys_and_bases() {
        assert!(validate_project_key("acme/app").is_ok());
        assert!(validate_project_key(&"k".repeat(MAX_PROJECT_KEY_BYTES)).is_ok());
        assert_eq!(
            validate_project_key(""),
            Err(ValidationError::MissingProjectKey)
        );
        assert_eq!(
            validate_project_key(&"k".repeat(MAX_PROJECT_KEY_BYTES + 1)),
            Err(ValidationError::ProjectKeyTooLong)
        );

        let base = crate::content_sha256("hello");
        assert!(validate_base_sha256(&base).is_ok());
        assert!(validate_base_sha256(&base.to_uppercase()).is_ok());
        for bad in ["", "abc", &base[1..], &format!("{base}0"), &base.replacen('a', "g", 1)] {
            assert_eq!(
                validate_base_sha256(bad),
                Err(ValidationError::BaseSha256),
                "{bad:?}"
            );
        }
    }
}
