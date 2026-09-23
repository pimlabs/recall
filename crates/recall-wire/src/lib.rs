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
//! # Layout
//!
//! One module per endpoint, since that is the unit that has to stay
//! compatible. Every type is also re-exported at the crate root, so callers
//! can write `recall_wire::PushRequest` and never think about which endpoint
//! a type belongs to.
//!
//! | Module | Endpoint |
//! |---|---|
//! | [`sync`] | `POST /sync`, `GET /sync` — memory files in both directions |
//! | [`health`] | `GET /health` — unauthenticated liveness and merge status |
//! | [`admin`] | `GET /admin/stats` — what is stored, per project |
//! | [`discovery`] | `GET /.well-known/recall` — what the server is and speaks |
//! | [`devices`] | `/v1/devices`, `/v1/enroll-keys` — enrolling and managing devices |
//! | [`signature`] | not an endpoint: how a device signs every request |
//! | [`validate`] | the rules both halves enforce |
//!
//! # Frozen surface
//!
//! **These JSON shapes are frozen.** The deployed Node server speaks them,
//! its SQLite rows were written against them, and during any migration a
//! machine on the old client and one on the new binary talk to the same
//! deployment. Field names and ordering here are compatibility surface, not
//! style.
//!
//! That includes timestamps, which every `updated_at`, `checked_at` and
//! `last_*_at` field carries as JavaScript's `Date.toISOString()` —
//! millisecond precision with a `Z` suffix, e.g. `2026-09-03T21:49:55.191Z`.
//! Rows already in the database are in that shape. `recall_server::now`
//! produces it.
//!
//! The full reference, including status codes and worked `curl` examples,
//! is in `docs/reference/api.md`.

#![deny(missing_docs)]

pub mod admin;
pub mod audit;
pub mod devices;
pub mod discovery;
pub mod health;
pub mod signature;
pub mod sync;
pub mod validate;

pub use admin::{AdminStats, AdminTotals, ProjectStats};
pub use audit::{AuditCapability, AuditCheckpoint, AuditConsistencyResponse, AuditEntriesResponse};
pub use devices::{
    ApproveRequest, DenyRequest, DenyResponse, Device, DeviceIdentity, DeviceList,
    DevicesCapability, EnrollApproved, EnrollKey, EnrollKeyCreated, EnrollKeyList,
    EnrollKeyRequest, EnrollKeyRevokeRequest, EnrollPending, EnrollPollRequest, EnrollPollResponse,
    EnrollRequest, PendingEnrollment,
};
pub use discovery::{Discovery, DISCOVERY_PATH, PROTOCOL, PROTOCOL_HEADER};
pub use health::{ClaudeCliStatus, Health, MergeError, MergeStatus};
/// The hash a push names its base by: SHA-256 of the file's exact bytes, as
/// lowercase hex.
///
/// Computed on both sides of the wire — by the client over what it last
/// synced, by the server over what it has stored — so it has to be one
/// function in one place.
pub fn content_sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod hash_tests {
    use super::content_sha256;

    /// Known SHA-256 vectors, so the two sides of the wire cannot drift into
    /// computing something else under the same name.
    #[test]
    fn content_sha256_is_plain_sha256_as_lowercase_hex() {
        assert_eq!(
            content_sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            content_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Exact bytes: a trailing newline is part of the content, and so part of
    /// the hash.
    #[test]
    fn a_trailing_newline_changes_the_hash() {
        assert_ne!(content_sha256("a"), content_sha256("a\n"));
    }
}

pub use sync::{File, PushRequest, PushResponse, SyncResponse};
pub use validate::{validate_file_path, ValidationError};

use serde::{Deserialize, Serialize};

/// Body returned for any non-2xx, on every endpoint.
///
/// It lives at the root rather than in an endpoint module because it belongs
/// to all of them: whatever a request was trying to do, this is the shape of
/// being told no.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// A human-readable reason, safe to show a user.
    pub error: String,
}
