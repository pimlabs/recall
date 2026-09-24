//! The audit log: a Merkle tree over every authenticated action, following
//! [`docs/design/part5-plan.md`](../../../../docs/design/part5-plan.md)'s
//! "PR 1: audit" and "The audit chain".
//!
//! [`merkle`] is the pure hash tree (RFC 9162 §2.1), with no knowledge of
//! Recall's own shapes. [`leaf`] is the canonical, versioned encoding of one
//! entry — what gets hashed and stored. Persistence — the `audit_log`
//! table, the in-memory [`merkle::Tree`] it is rebuilt into at start,
//! and the single transaction a leaf commits in alongside the state change
//! it records — lives in [`crate::store`], which is where the two meet.
//!
//! [`merkle`] itself lives in `recall-wire` and is re-exported here, since
//! the client checks what this server proves with the same code: a
//! consistency proof built from this tree is verified by `recall doctor`
//! with [`merkle::verify_consistency`], and an export's root rebuilt by
//! `recall audit verify` with [`merkle::Tree`].

pub mod leaf;
pub use recall_wire::audit::merkle;
