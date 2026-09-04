//! `POST /sync` and `GET /sync` — the endpoints that carry memory files.

use serde::{Deserialize, Serialize};

use crate::ValidationError;

/// Body of `POST /sync`: one memory file, or one delete.
///
/// `content` is an [`Option`], not a [`String`], and that is load-bearing:
/// it has to distinguish "this file is empty" ([`Some`]`("")`, serialized as
/// `"content":""`) from "this is a delete, there is no content" ([`None`],
/// omitted entirely).
///
/// Skipping on emptiness instead — the obvious-looking
/// `skip_serializing_if = "String::is_empty"` — omits the field for an
/// empty *file* too, and the deployed Node server rejects that push with a
/// 400, since `typeof undefined !== "string"`. Both the Go and the first
/// Rust implementation had exactly that bug, and neither test suite caught
/// it: every test that exercised an empty file used a stand-in server that
/// accepted anything. It surfaced only when the compatibility script ran a
/// real empty-file push against the real Node server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushRequest {
    /// Which project this file belongs to. See `recall_paths::project`.
    pub project_key: String,
    /// The file's path relative to the project's memory directory.
    pub file_path: String,
    /// The file's exact bytes, or [`None`] to mean "this is a delete".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// A label for the machine that sent this, for display only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_env: String,
    /// Whether this push is a delete rather than a write.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
}

impl PushRequest {
    /// Checks a push is well-formed, applying the same rules on both sides
    /// of the wire: the client refuses to send what the server would refuse
    /// to accept, so the user sees the real reason rather than a 400.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.project_key.is_empty() {
            return Err(ValidationError::MissingProjectKey);
        }
        crate::validate_file_path(&self.file_path)
    }
}

/// Body returned by `POST /sync`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushResponse {
    /// Always `true`; a failure is a non-2xx carrying an
    /// [`ErrorResponse`](crate::ErrorResponse) instead.
    pub ok: bool,
    /// Echoed back from the request.
    pub project_key: String,
    /// Echoed back from the request.
    pub file_path: String,
    /// Whether the stored row is now a tombstone.
    pub deleted: bool,
    /// Whether the stored content is the result of a semantic merge rather
    /// than a plain write.
    pub merged: bool,
    /// When the row was written, in the crate's frozen timestamp format.
    pub updated_at: String,
}

/// One memory file as returned by `GET /sync`.
///
/// `content` is an [`Option`] so a tombstoned row reports JSON `null` rather
/// than `""`: the server withholds deleted content so a pull cannot
/// resurrect it, and an empty string would be indistinguishable from a
/// genuinely empty file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct File {
    /// The file's path relative to the project's memory directory.
    pub file_path: String,
    /// The file's exact bytes, or [`None`] for a tombstone.
    pub content: Option<String>,
    /// The machine that last wrote this file.
    #[serde(default)]
    pub source_env: String,
    /// When it was last written, in the crate's frozen timestamp format.
    #[serde(default)]
    pub updated_at: String,
    /// Whether this row is a tombstone, meaning a puller should delete its
    /// local copy.
    #[serde(default)]
    pub deleted: bool,
}

/// Body returned by `GET /sync`: every file held for one project,
/// tombstones included.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Echoed back from the query string.
    pub project_key: String,
    /// Every file the server holds for the project.
    pub files: Vec<File>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_push_requests() {
        let valid = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            content: Some("hi".into()),
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
            content: Some("hello".into()),
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

    /// The bug the compatibility script caught: an empty memory file must
    /// still send `"content":""`. Omitting it makes the Node server answer
    /// 400, so a project containing one empty note could never sync.
    #[test]
    fn an_empty_file_still_sends_its_content_field() {
        let push = PushRequest {
            project_key: "acme/app".into(),
            file_path: "empty.md".into(),
            content: Some(String::new()),
            source_env: "laptop".into(),
            deleted: false,
        };
        let json = serde_json::to_string(&push).unwrap();
        assert!(
            json.contains(r#""content":"""#),
            "empty file must serialize its content field, got {json}"
        );

        // A delete still omits it, which is what keeps the two apart.
        let delete = PushRequest {
            project_key: "acme/app".into(),
            file_path: "gone.md".into(),
            deleted: true,
            ..Default::default()
        };
        assert!(!serde_json::to_string(&delete).unwrap().contains("content"));
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
        assert!(serde_json::to_string(&tombstone)
            .unwrap()
            .contains(r#""content":null"#));
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
        assert_eq!(
            parsed.files[0].content.as_deref(),
            Some("written by the NODE server\n")
        );
        assert!(parsed.files[1].deleted);
        assert_eq!(parsed.files[1].content, None);
    }
}
