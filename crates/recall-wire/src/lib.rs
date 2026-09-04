//! The contract between Recall's client and server: request and response
//! shapes, and the validation rules that apply to both.
//!
//! Keeping this in one crate is the point of a workspace rather than two
//! programs. Before the implementations were unified, `file_path`
//! validation and the tombstone/empty-file distinction existed twice — in
//! JavaScript on the server and in bash on the client — with nothing
//! keeping them in agreement, so a drift between them would only surface in
//! production.
//!
//! **These JSON shapes are frozen.** The deployed Node server speaks them,
//! its SQLite rows were written against them, and during any migration a
//! machine on the old client and one on the new binary talk to the same
//! deployment. Field names and ordering here are compatibility surface, not
//! style.

use serde::{Deserialize, Serialize};

/// Body of `POST /sync`.
///
/// `content` is skipped when empty so a delete serializes without it, while
/// `deleted` carries that intent explicitly — an empty file is a legitimate
/// push and must never be mistaken for a tombstone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushRequest {
    pub project_key: String,
    pub file_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_env: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

/// Body returned by `POST /sync`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushResponse {
    pub ok: bool,
    pub project_key: String,
    pub file_path: String,
    pub deleted: bool,
    pub merged: bool,
    pub updated_at: String,
}

/// One memory file as returned by `GET /sync`.
///
/// `content` is an `Option` so a tombstoned row reports JSON `null` rather
/// than `""`: the server withholds deleted content so a pull cannot
/// resurrect it, and an empty string would be indistinguishable from a
/// genuinely empty file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct File {
    pub file_path: String,
    pub content: Option<String>,
    #[serde(default)]
    pub source_env: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub deleted: bool,
}

/// Body returned by `GET /sync`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncResponse {
    pub project_key: String,
    pub files: Vec<File>,
}

/// Whether the server can actually perform a semantic merge, so a degraded
/// deployment is visible from outside instead of silently falling back
/// forever.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClaudeCliStatus {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checked_at: String,
    pub available: Option<bool>,
    pub logged_in: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// The most recent failed merge attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeError {
    pub message: String,
    pub at: String,
}

/// The `merge` object inside `GET /health`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MergeStatus {
    pub enabled: bool,
    pub claude_cli: ClaudeCliStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_merge_at: String,
    pub last_merge_error: Option<MergeError>,
}

/// Body returned by `GET /health`, which is deliberately unauthenticated so
/// uptime tooling can poll it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub git_commit: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_sync_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_backup_at: String,
    pub merge: MergeStatus,
}

/// One row of `GET /admin/stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectStats {
    pub project_key: String,
    pub file_count: i64,
    pub deleted_count: i64,
    pub sources: Vec<String>,
    pub last_updated_at: String,
}

/// The aggregate line of `GET /admin/stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminTotals {
    pub project_count: i64,
    pub file_count: i64,
    pub deleted_count: i64,
}

/// Body returned by `GET /admin/stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdminStats {
    pub projects: Vec<ProjectStats>,
    pub totals: AdminTotals,
    pub git_commit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_backup_at: String,
}

/// Body returned for any non-2xx.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

/// Why a request was rejected. Exported so the server (refusing a request)
/// and the client (refusing to send one) act on the same reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("project_key is required")]
    MissingProjectKey,
    #[error("file_path is required")]
    MissingFilePath,
    #[error("file_path must be relative")]
    FilePathAbsolute,
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
    if path
        .split(['/', '\\'])
        .any(|segment| segment == "..")
    {
        return Err(ValidationError::FilePathTraversal);
    }
    Ok(())
}

impl PushRequest {
    /// Checks a push is well-formed, applying the same rules on both sides
    /// of the wire.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.project_key.is_empty() {
            return Err(ValidationError::MissingProjectKey);
        }
        validate_file_path(&self.file_path)
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
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
            ("topics/../../outside.md", ValidationError::FilePathTraversal),
            (r"topics\..\..\outside.md", ValidationError::FilePathTraversal),
            ("..", ValidationError::FilePathTraversal),
        ] {
            assert_eq!(validate_file_path(path), Err(want), "for {path:?}");
        }
    }

    #[test]
    fn validates_push_requests() {
        let valid = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            content: "hi".into(),
            ..Default::default()
        };
        assert!(valid.validate().is_ok());

        let no_key = PushRequest {
            file_path: "MEMORY.md".into(),
            ..Default::default()
        };
        assert_eq!(no_key.validate(), Err(ValidationError::MissingProjectKey));

        // A delete carries no content; that must not read as invalid.
        let delete = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            deleted: true,
            ..Default::default()
        };
        assert!(delete.validate().is_ok());
    }

    /// Byte-for-byte compatibility with the deployed Node server. These
    /// exact strings were captured from it.
    #[test]
    fn push_body_matches_the_node_server() {
        let push = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            content: "hello".into(),
            source_env: "laptop".into(),
            deleted: false,
        };
        assert_eq!(
            serde_json::to_string(&push).unwrap(),
            r#"{"project_key":"acme/app","file_path":"MEMORY.md","content":"hello","source_env":"laptop"}"#
        );

        let delete = PushRequest {
            project_key: "acme/app".into(),
            file_path: "gone.md".into(),
            source_env: "laptop".into(),
            deleted: true,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&delete).unwrap(),
            r#"{"project_key":"acme/app","file_path":"gone.md","source_env":"laptop","deleted":true}"#
        );
    }

    #[test]
    fn tombstoned_file_serializes_content_as_null() {
        let resp = SyncResponse {
            project_key: "acme/app".into(),
            files: vec![File {
                file_path: "gone.md".into(),
                content: None,
                source_env: "laptop".into(),
                updated_at: "2026-01-01T00:00:00.000Z".into(),
                deleted: true,
            }],
        };
        assert_eq!(
            serde_json::to_string(&resp).unwrap(),
            r#"{"project_key":"acme/app","files":[{"file_path":"gone.md","content":null,"source_env":"laptop","updated_at":"2026-01-01T00:00:00.000Z","deleted":true}]}"#
        );
    }

    /// An empty file is a legitimate memory file. If it serialized like a
    /// tombstone, a pull would skip writing it.
    #[test]
    fn empty_file_is_distinguishable_from_a_tombstone() {
        let empty = File {
            file_path: "empty.md".into(),
            content: Some(String::new()),
            ..Default::default()
        };
        let json = serde_json::to_string(&empty).unwrap();
        assert!(json.contains(r#""content":"""#), "got {json}");

        let tombstone = File {
            file_path: "empty.md".into(),
            content: None,
            ..Default::default()
        };
        assert!(serde_json::to_string(&tombstone).unwrap().contains(r#""content":null"#));
    }

    /// Responses from the deployed Node server must deserialize as-is.
    #[test]
    fn parses_a_real_node_server_response() {
        let body = r#"{"project_key":"acme/app","files":[
            {"file_path":"MEMORY.md","content":"written by the NODE server\n","source_env":"node-era","updated_at":"2026-09-03T21:49:55.191Z","deleted":false},
            {"file_path":"gone.md","content":null,"source_env":"node-era","updated_at":"2026-09-03T21:49:55.212Z","deleted":true}
        ]}"#;
        let parsed: SyncResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[0].content.as_deref(), Some("written by the NODE server\n"));
        assert!(parsed.files[1].deleted);
        assert_eq!(parsed.files[1].content, None);
    }
}
