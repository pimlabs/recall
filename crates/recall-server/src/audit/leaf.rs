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
    /// A device enrolled with an authkey, approved at once by the key.
    pub const ENROLL: &str = "enroll";
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
    /// An authkey, which approves the device it enrols by itself: named by
    /// its id and tag, never by the key.
    Authkey {
        /// `ak_…`.
        id: &'a str,
        /// Its label.
        tag: &'a str,
    },
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
            Actor::Authkey { id, tag } => {
                m.insert("kind".into(), json!("authkey"));
                m.insert("id".into(), json!(id));
                m.insert("tag".into(), json!(tag));
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
/// `approve` or `enroll` leaf (see `docs/design/part5-plan.md`'s
/// "Verifying offline").
///
/// What a signature proves is that the device sent a request with this
/// method, path, query and body digest — not what the server made of it.
/// The path and query bind a pull's project and a revoke's id; `body`
/// binds the rest of a device-management action, whose small body is kept
/// whole. A push's or a delete's body is the file, or names it, and is not
/// kept, so for those the signature vouches for the request and its
/// digest, and `subject` is the server's word.
pub struct SignedRequest<'a> {
    /// `Content-Digest`'s sha-256 value, base64 standard: the hash of the
    /// request body — for a push, the JSON that carried the file, not the
    /// file.
    pub body_sha256: &'a str,
    /// The exact bytes RFC 9421 §2.5 built and the device signed.
    pub signature_base: &'a str,
    /// The signature itself, base64 standard.
    pub signature: &'a str,
    /// The request body itself, for the actions whose body is a few bytes
    /// of JSON with no secret in it: `approve`, `deny`, `revoke`,
    /// `authkey_create` and `authkey_revoke`. [`None`], written `null`, for
    /// a push, a delete and a pull.
    pub body: Option<&'a str>,
}

impl SignedRequest<'_> {
    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("body_sha256".into(), json!(self.body_sha256));
        m.insert("signature_base".into(), json!(self.signature_base));
        m.insert("signature".into(), json!(self.signature));
        m.insert("body".into(), json!(self.body));
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

/// What a push or a delete changed, for [`subject_file`].
pub struct FileChange<'a> {
    /// The project.
    pub project_key: &'a str,
    /// The file, relative to the project's memory directory.
    pub file_path: &'a str,
    /// A delete, rather than a push.
    pub deleted: bool,
    /// `content_sha256` of what is stored now; for a delete, of the empty
    /// string.
    pub stored_sha256: &'a str,
    /// The `base_sha256` the push named, lowercase, if it named one.
    pub base_sha256: Option<&'a str>,
    /// Whether the server merged the push with what it held, so what it
    /// stored is not what was sent, and `stored_sha256` is not the hash of
    /// the pushed content.
    pub merged: bool,
}

/// `subject` for [`action::PUSH`] and [`action::DELETE`]: a file's identity
/// and the hash of what is now stored, never the content.
///
/// `merge_job` is always `null` in this pull request — jobs and the worker
/// are `part5-plan.md`'s PR 2 — and is carried now so its field position is
/// part of the frozen leaf shape rather than an addition later that would
/// need a `v: 2`. `merged` is the other kind of merge, the one done inline
/// before storing.
pub fn subject_file(change: &FileChange<'_>) -> Value {
    let mut m = Map::new();
    m.insert("project_key".into(), json!(change.project_key));
    m.insert("file_path".into(), json!(change.file_path));
    m.insert("deleted".into(), json!(change.deleted));
    m.insert("stored_sha256".into(), json!(change.stored_sha256));
    m.insert("base_sha256".into(), json!(change.base_sha256));
    m.insert("merged".into(), json!(change.merged));
    m.insert("merge_job".into(), Value::Null);
    Value::Object(m)
}

/// `subject` for [`action::PULL`]: which project was fetched.
pub fn subject_pull(project_key: &str) -> Value {
    let mut m = Map::new();
    m.insert("project_key".into(), json!(project_key));
    Value::Object(m)
}

/// `subject` for [`action::APPROVE`] and [`action::ENROLL`], the two ways a
/// device comes to exist: its own identity and public key, so a signature
/// it made can still be checked after the device row itself is gone
/// (revoked, or swept). One shape for both: `authkey_id` names the key an
/// `enroll` came in with and `user_code` the code an `approve` decided,
/// each `null` in the other.
pub fn subject_device(device: &recall_wire::Device, user_code: Option<&str>) -> Value {
    let mut m = Map::new();
    m.insert("device_id".into(), json!(device.id));
    m.insert("name".into(), json!(device.name));
    m.insert("scope".into(), json!(device.scope));
    m.insert("public_key".into(), json!(device.public_key));
    m.insert("fingerprint".into(), json!(device.fingerprint));
    m.insert("ephemeral".into(), json!(device.ephemeral));
    m.insert("authkey_id".into(), json!(device.authkey_id));
    m.insert("user_code".into(), json!(user_code));
    Value::Object(m)
}

/// `subject` for [`action::REVOKE`] and [`action::SWEEP`]: which device,
/// without needing its key again — the `approve` or `enroll` leaf that made
/// it already carries that.
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

/// `subject` for [`action::AUTHKEY_REVOKE`]: the key, whether the request
/// asked for its devices too, and the devices this revoked — the ones it
/// enrolled that were not revoked already — so the log names every device
/// that stopped here, though none of them has a leaf of its own.
pub fn subject_authkey_revoke(id: &str, revoke_devices: bool, revoked: &[String]) -> Value {
    let mut m = Map::new();
    m.insert("authkey_id".into(), json!(id));
    m.insert("revoke_devices".into(), json!(revoke_devices));
    m.insert("revoked_devices".into(), json!(revoked));
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
            subject_file(&FileChange {
                project_key: "acme/app",
                file_path: "topics/auth.md",
                deleted: false,
                stored_sha256: "4b1f",
                base_sha256: Some("9f2c"),
                merged: true,
            }),
            Some(&SignedRequest {
                body_sha256: "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
                signature_base: "\"@method\": POST\n",
                signature: "sig",
                body: None,
            }),
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"v":1,"seq":1001,"at":"2026-10-02T09:14:05.402Z","action":"push","#,
                r#""actor":{"kind":"device","id":"dev_eerivjyffuwecbgzybcesz5hwi","name":"laptop","agent":"recall/0.4.5 (macos-aarch64)"},"#,
                r#""subject":{"project_key":"acme/app","file_path":"topics/auth.md","deleted":false,"stored_sha256":"4b1f","base_sha256":"9f2c","merged":true,"merge_job":null},"#,
                r#""request":{"body_sha256":"47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=","signature_base":"\"@method\": POST\n","signature":"sig","body":null}}"#,
            ),
            "got {text}"
        );
        // Compact, one line: a real newline inside a field is escaped, not
        // left to break the export's one-leaf-per-line shape.
        assert!(!text.contains('\n'), "got {text}");
    }

    /// The two leaves that make a device, `approve` and `enroll`, share one
    /// subject: the public key a later signature is checked with, and
    /// whichever of the code or the authkey it came by.
    #[test]
    fn an_enroll_leaf_names_the_authkey_and_carries_the_key() {
        let device = recall_wire::Device {
            id: "dev_x".into(),
            name: "cloud-k3jz9w2q".into(),
            scope: "sync".into(),
            ephemeral: true,
            public_key: "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs".into(),
            fingerprint: "SHA256:fp".into(),
            authkey_id: Some("ak_1".into()),
            ..Default::default()
        };
        let text = String::from_utf8(encode(
            7,
            "2026-10-02T09:00:00.000Z",
            action::ENROLL,
            &Actor::Authkey {
                id: "ak_1",
                tag: "cloud",
            },
            subject_device(&device, None),
            None,
        ))
        .unwrap();
        assert_eq!(
            text,
            concat!(
                r#"{"v":1,"seq":7,"at":"2026-10-02T09:00:00.000Z","action":"enroll","#,
                r#""actor":{"kind":"authkey","id":"ak_1","tag":"cloud"},"#,
                r#""subject":{"device_id":"dev_x","name":"cloud-k3jz9w2q","scope":"sync","#,
                r#""public_key":"JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs","fingerprint":"SHA256:fp","#,
                r#""ephemeral":true,"authkey_id":"ak_1","user_code":null},"request":null}"#,
            )
        );
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
