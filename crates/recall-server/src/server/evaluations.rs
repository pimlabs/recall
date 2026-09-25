//! The evaluation routes: the owner asking for a report on what memory
//! holds, and reading the reports. The worker makes each one as an
//! `evaluate` job (see `jobs.rs` and `store/evaluations.rs`); this module
//! is HTTP, and the schedule `RECALL_EVAL_INTERVAL_HOURS` turns on.
//!
//! All three routes are admin: the operator's token, an admin device, or
//! the admin page's passkey session. The session is served everything but
//! `details`, which quotes notes and which the page never shows.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use recall_wire::evaluations::{EvaluationRequest, MAX_PROJECTS, STATE_QUEUED};
use recall_wire::{EvaluationCreated, EvaluationList};
use time::OffsetDateTime;

use super::audit::{actor_for, signed_request_for};
use super::auth::{Caller, SignedRequestInfo};
use super::devices::{body, body_text, new_id, small_body};
use super::respond::{error, internal, json};
use super::AppState;
use crate::audit::leaf;
use crate::store::Requested;

/// The most runs one listing answers with.
const LIST_LIMIT: usize = 200;

/// What a request is refused with when no worker could take it.
pub(super) const NO_WORKER: &str = "no worker is enrolled to make an evaluation: run \
     recall-worker and approve it (recall devices approve <code> --worker)";

/// `POST /v1/evaluations`: queues a run. `{}`, or no body, asks for every
/// project without the contradiction check.
pub(super) async fn handle_request(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let bytes = match small_body(bytes) {
        Ok(bytes) => bytes,
        Err(refused) => return refused.into_response(),
    };
    let text = match body_text(&bytes) {
        Ok(text) => text,
        Err(refused) => return refused.into_response(),
    };
    let mut req: EvaluationRequest = if text.trim().is_empty() {
        EvaluationRequest::default()
    } else {
        match body(&bytes) {
            Ok(req) => req,
            Err(refused) => return refused.into_response(),
        }
    };
    if req.projects.len() > MAX_PROJECTS {
        return error(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_PROJECTS} projects"),
        );
    }
    let mut seen = std::collections::HashSet::new();
    req.projects.retain(|p| seen.insert(p.clone()));
    for project in &req.projects {
        if let Err(e) = recall_wire::validate_project_key(project) {
            return error(StatusCode::BAD_REQUEST, &e.to_string());
        }
        match state.store.has_project(project) {
            Ok(true) => {}
            Ok(false) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    &format!("no project has the key {project:?}"),
                )
            }
            Err(e) => return internal(e),
        }
    }
    let actor = actor_for(caller.as_ref().map(|Extension(c)| c));
    let request = signed_request_for(signed.as_ref().map(|Extension(s)| s), Some(text));
    match request_evaluation(&state, &req, &actor, request.as_ref()) {
        Ok(Some(created)) => json(StatusCode::OK, &created),
        Ok(None) => error(StatusCode::CONFLICT, NO_WORKER),
        Err(e) => internal(e),
    }
}

/// Queues a run asked for by `actor`, and wakes the worker's claim.
/// [`None`] when no worker is enrolled to make it, or the queue is full.
fn request_evaluation(
    state: &AppState,
    req: &EvaluationRequest,
    actor: &leaf::Actor<'_>,
    request: Option<&leaf::SignedRequest<'_>>,
) -> anyhow::Result<Option<EvaluationCreated>> {
    // The server does not evaluate, so a run nothing will take is refused
    // rather than queued to wait for a worker nobody has set up.
    if state.store.enrolled_worker()?.is_none() {
        return Ok(None);
    }
    let id = new_id("eval_", 10)?;
    let job = new_id("job_", 10)?;
    let requested = state.store.request_evaluation_audited(
        &id,
        &job,
        req,
        OffsetDateTime::now_utc(),
        |seq, at| {
            leaf::encode(
                seq,
                at,
                leaf::action::EVALUATE,
                actor,
                leaf::subject_evaluate(&id, &job, &req.projects, req.contradictions),
                request,
            )
        },
    )?;
    match requested {
        Requested::Queued(job) => {
            state.jobs_ready.notify_waiters();
            Ok(Some(EvaluationCreated {
                id,
                state: STATE_QUEUED.to_string(),
                job,
            }))
        }
        Requested::Full => Ok(None),
    }
}

/// A scheduled run, for [`super::Server::start_background`]: every project,
/// and never the contradiction check, which spends the owner's Claude
/// usage and runs only when someone asks for it. Skipped while no worker
/// is enrolled, and while another run is waiting or being made. Answers
/// the run it queued.
pub(super) fn run_scheduled(state: &AppState) -> anyhow::Result<Option<String>> {
    if state.store.evaluation_open()? {
        return Ok(None);
    }
    let req = EvaluationRequest::default();
    Ok(request_evaluation(state, &req, &leaf::Actor::Server, None)?.map(|c| c.id))
}

/// `GET /v1/evaluations`: newest first, with counts; no findings and no
/// details.
pub(super) async fn handle_list(State(state): State<Arc<AppState>>) -> Response {
    if let Err(e) = super::jobs::expire_leases(&state) {
        return internal(e);
    }
    match state.store.evaluations(LIST_LIMIT) {
        Ok(evaluations) => json(StatusCode::OK, &EvaluationList { evaluations }),
        Err(e) => internal(e),
    }
}

/// `GET /v1/evaluations/{id}`: one run with its findings, and its details
/// for anyone but the admin page's passkey session.
pub(super) async fn handle_get(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = super::jobs::expire_leases(&state) {
        return internal(e);
    }
    // The page shows kinds, files and lines, and says to run
    // `recall eval show` for the rest: a browser session never holds
    // note text.
    let session = matches!(
        caller.as_ref().map(|Extension(c)| c),
        Some(Caller::Owner { .. })
    );
    match state.store.evaluation(&id, !session) {
        Ok(Some(evaluation)) => json(StatusCode::OK, &evaluation),
        Ok(None) => error(StatusCode::NOT_FOUND, "no evaluation has that id"),
        Err(e) => internal(e),
    }
}
