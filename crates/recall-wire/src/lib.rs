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
//! is in `docs/api.md`.

#![deny(missing_docs)]

pub mod admin;
pub mod health;
pub mod sync;
pub mod validate;

pub use admin::{AdminStats, AdminTotals, ProjectStats};
pub use health::{ClaudeCliStatus, Health, MergeError, MergeStatus};
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
