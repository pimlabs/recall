//! The audit leaf, version 1: one per authenticated push, pull, delete,
//! and change to a device or authkey, and one per action the server
//! takes itself. See `docs/design/part5-plan.md`'s "PR 1: audit" for the
//! wire shape this mirrors exactly.
//!
//! **What is hashed** is the leaf exactly as written: compact JSON, fields
//! in declaration order, UTF-8. [`encode`] is the one place that produces
//! those bytes, so nothing downstream — the Merkle hash, storage, or an
//! export — ever re-serializes a leaf; each holds or hashes what this wrote.
//!
//! Field order is built by hand with an ordered [`serde_json::Map`] rather
//! than a `#[derive(Serialize)]` struct, because `subject` differs by
//! `action` — a device's public key for `approve`, a project and file for
//! `push` — and no single Rust type covers all of them without either a
//! sea of `Option`s (each action's leaf would then carry visible nulls for
//! every other action's fields) or an enum whose variant name would leak
//! into the JSON as untagged data does not want it to. `serde_json`'s
//! `preserve_order` feature, already enabled workspace-wide so a person's
//! edited `.claude/settings.json` keeps its key order, is what makes the
//! map's insertion order its serialization order.

use serde_json::{json, Map, Value};

/// The leaf format this server writes. Stored as the leaf's own `v` field,
/// so a verifier reading an old export knows which rules applied.
pub const VERSION: u32 = 1;

/// The `action` values a leaf may carry.
pub mod action {
    /// A file was written (`POST /sync` with `deleted: false`).
    pub const PUSH: &str = "push";
    /// A file was tombstoned (`POST /sync` with `deleted: true`).
    pub const DELETE: &str = "delete";
    /// `GET /sync` — a client fetched a project's files.
    pub const PULL: &str = "pull";
    /// A pending enrolment was approved.
    pub const APPROVE: &str = "approve";
    /// A pending enrolment was denied.
    pub const DENY: &str = "deny";
    /// A device was revoked.
    pub const REVOKE: &str = "revoke";
    /// The server removed an idle ephemeral device.
    pub const SWEEP: &str = "sweep";
    /// An authkey was created.
    pub const AUTHKEY_CREATE: &str = "authkey_create";
    /// An authkey was revoked.
    pub const AUTHKEY_REVOKE: &str = "authkey_revoke";
    /// The server started.
    pub const START: &str = "start";
}

/// Who did it (`actor.kind`).
pub enum Actor<'a> {
    /// A signed request from an enrolled device.
    Device {
        /// `dev_…`.
        id: &'a str,
        /// The name it enrolled as.
        name: &'a str,
        /// Its `User-Agent`.
        agent: &'a str,
    },
    /// `RECALL_TOKEN`.
    Operator,
    /// The server's own doing: a sweep, or `start`.
    Server,
}

impl Actor<'_> {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        match self {
            Actor::Device { id, name, agent } => {
                m.insert("kind".into(), json!("device"));
                m.insert("id".into(), json!(id));
                m.insert("name".into(), json!(name));
                m.insert("agent".into(), json!(agent));
            }
            Actor::Operator => {
                m.insert("kind".into(), json!("operator"));
            }
            Actor::Server => {
                m.insert("kind".into(), json!("server"));
            }
        }
        Value::Object(m)
    }
}

/// The signed-request material a device-authenticated leaf carries in its
/// `request` field: enough for an offline verifier to check the signature
/// against nothing but the device's key, itself carried by an earlier
/// `approve` leaf (see `docs/design/part5-plan.md`'s "Verifying offline").
pub struct SignedRequest<'a> {
    /// `Content-Digest`'s sha-256 value, base64 standard — matches
    /// `subject.stored_sha256`'s hash of the same body only when the body
    /// itself is the stored content, which is true for a push.
    pub body_sha256: &'a str,
    /// The exact bytes RFC 9421 §2.5 built and the device signed.
    pub signature_base: &'a str,
    /// The signature itself, base64 standard.
    pub signature: &'a str,
}

impl SignedRequest<'_> {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("body_sha256".into(), json!(self.body_sha256));
        m.insert("signature_base".into(), json!(self.signature_base));
        m.insert("signature".into(), json!(self.signature));
        Value::Object(m)
    }
}

/// Encodes one leaf as [`VERSION`]'s compact JSON, in the field order the
/// design shows: `v`, `seq`, `at`, `action`, `actor`, `subject`, `request`.
///
/// This is the exact byte sequence that gets hashed, stored, and exported —
/// see the module docs.
pub fn encode(
    seq: u64,
    at: &str,
    action: &str,
    actor: &Actor<'_>,
    subject: Value,
    request: Option<&SignedRequest<'_>>,
) -> Vec<u8> {
    let mut m = Map::new();
    m.insert("v".into(), json!(VERSION));
    m.insert("seq".into(), json!(seq));
    m.insert("at".into(), json!(at));
    m.insert("action".into(), json!(action));
    m.insert("actor".into(), actor.to_value());
    m.insert("subject".into(), subject);
    m.insert(
        "request".into(),
        request.map(SignedRequest::to_value).unwrap_or(Value::Null),
    );
    // A leaf is never pretty-printed or re-ordered: `to_string` is
    // serde_json's compact writer, and the map above preserves insertion
    // order (`preserve_order`), so this is deterministic input for
    // `merkle::hash_leaf`.
    serde_json::to_vec(&Value::Object(m)).expect("a leaf built from valid JSON values serializes")
}

/// `subject` for [`action::PUSH`] and [`action::DELETE`]: a file's identity
/// and the hash of what is now stored, never the content.
///
/// `merge_job` is always `null` in this pull request — jobs and the worker
/// are `part5-plan.md`'s PR 2 — and is carried now so its field position is
/// part of the frozen leaf shape rather than an addition later that would
/// need a `v: 2`.
pub fn subject_file(
    project_key: &str,
    file_path: &str,
    deleted: bool,
    stored_sha256: &str,
    base_sha256: Option<&str>,
) -> Value {
    let mut m = Map::new();
    m.insert("project_key".into(), json!(project_key));
    m.insert("file_path".into(), json!(file_path));
    m.insert("deleted".into(), json!(deleted));
    m.insert("stored_sha256".into(), json!(stored_sha256));
    m.insert("base_sha256".into(), json!(base_sha256));
    m.insert("merge_job".into(), Value::Null);
    Value::Object(m)
}

/// `subject` for [`action::PULL`]: which project was fetched.
pub fn subject_pull(project_key: &str) -> Value {
    let mut m = Map::new();
    m.insert("project_key".into(), json!(project_key));
    Value::Object(m)
}

/// `subject` for [`action::APPROVE`]: the device's own identity and public
/// key, so a signature it made can still be checked after the device row
/// itself is gone (revoked and later reused, or swept).
pub fn subject_device(
    device_id: &str,
    name: &str,
    scope: &str,
    public_key: &str,
    fingerprint: &str,
) -> Value {
    let mut m = Map::new();
    m.insert("device_id".into(), json!(device_id));
    m.insert("name".into(), json!(name));
    m.insert("scope".into(), json!(scope));
    m.insert("public_key".into(), json!(public_key));
    m.insert("fingerprint".into(), json!(fingerprint));
    Value::Object(m)
}

/// `subject` for [`action::REVOKE`] and [`action::SWEEP`]: which device,
/// without needing its key again — the `approve` leaf that enrolled it
/// already carries that.
pub fn subject_device_id(device_id: &str, name: &str) -> Value {
    let mut m = Map::new();
    m.insert("device_id".into(), json!(device_id));
    m.insert("name".into(), json!(name));
    Value::Object(m)
}

/// `subject` for [`action::DENY`]: the code and the name it would have
/// taken. No device exists to name by id.
pub fn subject_denied(user_code: &str, name: &str) -> Value {
    let mut m = Map::new();
    m.insert("user_code".into(), json!(user_code));
    m.insert("name".into(), json!(name));
    Value::Object(m)
}

/// `subject` for [`action::AUTHKEY_CREATE`]: the key's metadata, never
/// the secret itself — which the server never stores past the one response
/// that shows it.
pub fn subject_authkey(id: &str, tag: &str, ephemeral: bool, max_devices: Option<u32>) -> Value {
    let mut m = Map::new();
    m.insert("authkey_id".into(), json!(id));
    m.insert("tag".into(), json!(tag));
    m.insert("ephemeral".into(), json!(ephemeral));
    m.insert("max_devices".into(), json!(max_devices));
    Value::Object(m)
}

/// `subject` for [`action::AUTHKEY_REVOKE`].
pub fn subject_authkey_revoke(id: &str, revoke_devices: bool) -> Value {
    let mut m = Map::new();
    m.insert("authkey_id".into(), json!(id));
    m.insert("revoke_devices".into(), json!(revoke_devices));
    Value::Object(m)
}

/// `subject` for [`action::START`]: the version the server started as.
pub fn subject_start(version: &str) -> Value {
    let mut m = Map::new();
    m.insert("version".into(), json!(version));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact field order and compactness the design shows, so a
    /// verifier hashing "what it was given" is hashing what this wrote.
    #[test]
    fn a_push_leaf_has_the_documented_shape() {
        let bytes = encode(
            1001,
            "2026-10-02T09:14:05.402Z",
            action::PUSH,
            &Actor::Device {
                id: "dev_eerivjyffuwecbgzybcesz5hwi",
                name: "laptop",
                agent: "recall/0.4.5 (macos-aarch64)",
            },
            subject_file("acme/app", "topics/auth.md", false, "4b1f", Some("9f2c")),
            Some(&SignedRequest {
                body_sha256: "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
                signature_base: "\"@method\": POST\n",
                signature: "sig",
            }),
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"v":1,"seq":1001,"at":"2026-10-02T09:14:05.402Z","action":"push","#,
                r#""actor":{"kind":"device","id":"dev_eerivjyffuwecbgzybcesz5hwi","name":"laptop","agent":"recall/0.4.5 (macos-aarch64)"},"#,
                r#""subject":{"project_key":"acme/app","file_path":"topics/auth.md","deleted":false,"stored_sha256":"4b1f","base_sha256":"9f2c","merge_job":null},"#,
                r#""request":{"body_sha256":"47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=","signature_base":"\"@method\": POST\n","signature":"sig"}}"#,
            ),
            "got {text}"
        );
        // Compact, one line: a real newline inside a field is escaped, not
        // left to break the export's one-leaf-per-line shape.
        assert!(!text.contains('\n'), "got {text}");
    }

    /// A leaf with no signed request (operator or server actor) carries
    /// `request: null`, not an omitted field — consistent with the rest of
    /// the API's null-versus-absent rule.
    #[test]
    fn an_unsigned_leaf_carries_request_null() {
        let bytes = encode(
            0,
            "2026-10-02T09:00:00.000Z",
            action::START,
            &Actor::Server,
            subject_start("0.4.1"),
            None,
        );
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(r#""request":null"#), "got {text}");
        assert!(text.contains(r#""actor":{"kind":"server"}"#), "got {text}");
    }
}
