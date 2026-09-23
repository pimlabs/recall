//! The audit log: a Merkle tree over every authenticated action, following
//! [`docs/design/part5-plan.md`](../../../../docs/design/part5-plan.md)'s
//! "PR 1: audit" and "The audit chain".
//!
//! [`merkle`] is the pure hash tree (RFC 9162 §2.1), with no knowledge of
//! Recall's own shapes. [`leaf`] is the canonical, versioned encoding of one
//! entry — what gets hashed and stored. Persistence — the `audit_log`
//! table, the in-memory [`merkle::Frontier`] it is rebuilt into at start,
//! and the single transaction a leaf commits in alongside the state change
//! it records — lives in [`crate::store`], which is where the two meet.

pub mod leaf;
pub mod merkle;
