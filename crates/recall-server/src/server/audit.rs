//! `GET /v1/audit/checkpoint`, `GET /v1/audit/entries` and
//! `GET /v1/audit/consistency` — any credential, like `GET /sync` — plus the
//! small helpers every other handler uses to say who acted and how, for the
//! leaf it appends.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use recall_wire::audit::{
    AuditCheckpoint, AuditConsistencyResponse, AuditEntriesResponse, MAX_PAGE,
};

use super::auth::{Caller, SignedRequestInfo};
use super::respond::{error, internal, json};
use super::AppState;
use crate::audit::leaf;
use crate::store::ConsistencyError;

/// The actor an audit leaf records for whoever called the route it is
/// appended from: the device that signed the request, or the operator when
/// it carried `RECALL_TOKEN` instead (or, defensively, carried no
/// credential the auth layer recognised at all — which `guard` never lets
/// reach a handler, but this stays total rather than assuming it).
pub(super) fn actor_for(caller: Option<&Caller>) -> leaf::Actor<'_> {
    match caller {
        Some(Caller::Device {
            id, name, agent, ..
        }) => leaf::Actor::Device { id, name, agent },
        Some(Caller::Operator) | None => leaf::Actor::Operator,
    }
}

/// The `request` an audit leaf records, when the caller signed it — `None`
/// for the operator, who has no signature to keep.
pub(super) fn signed_request_for(
    signed: Option<&SignedRequestInfo>,
) -> Option<leaf::SignedRequest<'_>> {
    signed.map(|s| leaf::SignedRequest {
        body_sha256: &s.body_sha256,
        signature_base: &s.signature_base,
        signature: &s.signature,
    })
}

/// Standard base64, as the wire contract carries tree hashes — never the
/// lowercase hex `base_sha256` and the other file hashes use.
pub(super) fn base64_hash(h: &crate::audit::merkle::Hash) -> String {
    BASE64_STANDARD.encode(h)
}

/// `GET /v1/audit/checkpoint`.
pub(super) async fn handle_checkpoint(State(state): State<Arc<AppState>>) -> Response {
    let (tree_size, root) = state.store.audit_checkpoint();
    json(
        StatusCode::OK,
        &AuditCheckpoint {
            tree_size,
            root_hash: base64_hash(&root),
        },
    )
}

/// `GET /v1/audit/entries?start=&end=`.
pub(super) async fn handle_entries(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    const BAD_RANGE: &str =
        "start and end are required, end must be at least start, at most 1000 apart, \
         and at most the log's size";
    let parse = |k: &str| params.get(k).and_then(|v| v.parse::<u64>().ok());
    let (Some(start), Some(end)) = (parse("start"), parse("end")) else {
        return error(StatusCode::BAD_REQUEST, BAD_RANGE);
    };
    let (tree_size, _) = state.store.audit_checkpoint();
    if end < start || end - start > u64::from(MAX_PAGE) || end > tree_size {
        return error(StatusCode::BAD_REQUEST, BAD_RANGE);
    }
    match state.store.audit_entries(start, end) {
        Ok(entries) => json(
            StatusCode::OK,
            &AuditEntriesResponse {
                start,
                end,
                tree_size,
                entries: entries
                    .into_iter()
                    .map(|e| String::from_utf8(e.leaf).expect("a stored leaf is valid UTF-8"))
                    .collect(),
            },
        ),
        Err(e) => internal(e),
    }
}

/// `GET /v1/audit/consistency?first=&second=`.
pub(super) async fn handle_consistency(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let parse = |k: &str| params.get(k).and_then(|v| v.parse::<u64>().ok());
    let (Some(first), Some(second)) = (parse("first"), parse("second")) else {
        return error(
            StatusCode::BAD_REQUEST,
            "first and second query params are required",
        );
    };
    match state.store.audit_consistency(first, second) {
        Ok(Ok(proof)) => json(
            StatusCode::OK,
            &AuditConsistencyResponse {
                first,
                second,
                proof: proof.iter().map(base64_hash).collect(),
            },
        ),
        Ok(Err(ConsistencyError::BadRange)) => error(
            StatusCode::BAD_REQUEST,
            "first must be at least 1 and at most second",
        ),
        Ok(Err(ConsistencyError::SecondBeyondTreeSize)) => {
            error(StatusCode::BAD_REQUEST, "second is past the end of the log")
        }
        Err(e) => internal(e),
    }
}
