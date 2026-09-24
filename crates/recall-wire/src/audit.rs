//! `GET /v1/audit/checkpoint`, `GET /v1/audit/entries` and
//! `GET /v1/audit/consistency` — the Merkle tree over every authenticated
//! action. See `docs/design/part5-plan.md`'s "PR 1: audit" for the design,
//! and `docs/reference/api.md` for the authoritative shape of what shipped.
//!
//! A leaf's own JSON — `v`, `seq`, `at`, `action`, `actor`, `subject`,
//! `request` — is not typed here: it varies by `action`, it is written once
//! by `recall_server::audit::leaf` and never re-serialized (a verifier
//! hashes the exact bytes it was given), and every consumer of it — this
//! crate's fixtures, the entries route, an export — treats it as an opaque
//! string. [`AuditEntriesResponse::entries`] is `Vec<String>` for exactly
//! that reason.

use serde::{Deserialize, Serialize};

/// `GET`: the tree's current size and root.
pub const CHECKPOINT_PATH: &str = "/v1/audit/checkpoint";

/// `GET ?start=&end=`: leaves `start` to `end - 1`.
pub const ENTRIES_PATH: &str = "/v1/audit/entries";

/// `GET ?first=&second=`: the RFC 9162 §2.1.4 proof that `second` extends
/// `first`.
pub const CONSISTENCY_PATH: &str = "/v1/audit/consistency";

/// The most leaves [`ENTRIES_PATH`] answers with in one page.
pub const MAX_PAGE: u32 = 1000;

/// The most bytes of leaves [`ENTRIES_PATH`] answers with in one page, 2
/// MiB. A page whose leaves would come to more stops before the one that
/// would cross it — never before its first — and its `end` says where it
/// stopped. A typical leaf is under a kilobyte, so a full page of 1,000
/// seldom meets it.
pub const MAX_PAGE_BYTES: usize = 2 << 20;

/// The leaf format this build writes and reads. Carried in the discovery
/// document's `audit` capability so a client — or a future server version —
/// knows which rules a leaf without its own `v` field long gone would have
/// followed; every leaf this version writes carries `v` itself regardless.
pub const LEAF_VERSION: u32 = 1;

/// The response header every `GET /sync` answer also carries: `<tree_size>
/// <root_hash>`, e.g. `1042 CsUYapGGPo4dkMgIAUqom/Xajj7h2fB2MPA3j2jxq2I=` —
/// the same two fields as [`AuditCheckpoint`], so a pull leaves the client a
/// checkpoint without another request.
pub const CHECKPOINT_HEADER: &str = "recall-audit-checkpoint";

/// `GET /v1/audit/checkpoint`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditCheckpoint {
    /// How many leaves the tree has.
    pub tree_size: u64,
    /// The tree's root hash, standard base64 (as in a C2SP checkpoint —
    /// unlike `base_sha256` and the other file hashes on this API, which
    /// stay lowercase hex).
    pub root_hash: String,
}

impl AuditCheckpoint {
    /// The [`CHECKPOINT_HEADER`] value for this checkpoint.
    pub fn to_header_value(&self) -> String {
        format!("{} {}", self.tree_size, self.root_hash)
    }

    /// Reads a [`CHECKPOINT_HEADER`] value back. [`None`] if it is not
    /// `<tree_size> <root_hash>`.
    pub fn parse_header_value(value: &str) -> Option<Self> {
        let (size, root) = value.trim().split_once(' ')?;
        Some(Self {
            tree_size: size.parse().ok()?,
            root_hash: root.to_string(),
        })
    }
}

/// `GET /v1/audit/entries`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntriesResponse {
    /// Echoed from the query.
    pub start: u64,
    /// One past the last leaf in `entries`: the query's `end`, unless the
    /// page stopped early at [`MAX_PAGE_BYTES`], when it is where to ask
    /// from next.
    pub end: u64,
    /// The tree's size when this was answered, so a caller paging through
    /// knows where the end really is without a second request.
    pub tree_size: u64,
    /// Leaves `start` to `end - 1`, each exactly as stored: compact JSON,
    /// never re-serialized.
    pub entries: Vec<String>,
}

/// `GET /v1/audit/consistency`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditConsistencyResponse {
    /// Echoed from the query.
    pub first: u64,
    /// Echoed from the query.
    pub second: u64,
    /// The proof nodes, standard base64, in the order RFC 9162 §2.1.4
    /// builds them.
    pub proof: Vec<String>,
}

/// The `audit` capability in the discovery document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditCapability {
    /// The leaf format this server writes: [`LEAF_VERSION`].
    pub leaf_version: u32,
    /// The most entries one page of [`ENTRIES_PATH`] holds: [`MAX_PAGE`].
    pub max_page: u32,
    /// The most bytes of leaves one page holds: [`MAX_PAGE_BYTES`].
    pub max_page_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checkpoint_header_round_trips() {
        let cp = AuditCheckpoint {
            tree_size: 1042,
            root_hash: "CsUYapGGPo4dkMgIAUqom/Xajj7h2fB2MPA3j2jxq2I=".to_string(),
        };
        assert_eq!(
            cp.to_header_value(),
            "1042 CsUYapGGPo4dkMgIAUqom/Xajj7h2fB2MPA3j2jxq2I="
        );
        assert_eq!(
            AuditCheckpoint::parse_header_value(&cp.to_header_value()),
            Some(cp)
        );
        assert_eq!(
            AuditCheckpoint::parse_header_value("not a checkpoint"),
            None
        );
        assert_eq!(AuditCheckpoint::parse_header_value("abc CsUY"), None);
    }
}
