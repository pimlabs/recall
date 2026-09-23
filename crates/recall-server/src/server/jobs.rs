//! The job routes: a worker claiming merge jobs and posting their results,
//! and the owner listing and retrying them. The store decides what each
//! one means (`store/jobs.rs`); this module is HTTP and the in-memory
//! state `/health` reports.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use recall_wire::jobs::{self, MAX_LEASE_SECONDS, MAX_WAIT_SECONDS, MIN_LEASE_SECONDS, STATES};
use recall_wire::{
    ClaimRequest, ClaimResponse, ClaudeCliStatus, JobList, MergeError, ResultRequest,
};
use time::OffsetDateTime;

use super::auth::Caller;
use super::devices::new_id;
use super::handlers::not_found;
use super::middleware::{admin_only, guard, worker_only};
use super::respond::{error, internal, json};
use super::AppState;
use crate::store::{Retried, Settlement};
use crate::{format_timestamp, now};

/// How often a waiting claim looks at the queue again even when nothing
/// woke it: a job whose retry delay has passed, or whose lease ran out,
/// becomes claimable without a push to announce it.
const RECHECK: Duration = Duration::from_secs(5);

/// The most jobs one listing answers with.
const LIST_LIMIT: usize = 200;

/// Finished jobs are kept this long, for the listing, then removed.
pub(super) const DONE_JOBS_KEPT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The job routes, each behind [`guard`] and then the scope it needs.
pub(super) fn routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let worker = Router::new()
        .route(jobs::CLAIM_PATH, post(handle_claim).fallback(not_found))
        .route(
            "/v1/jobs/{id}/result",
            post(handle_result).fallback(not_found),
        )
        .route_layer(from_fn(worker_only))
        .route_layer(from_fn_with_state(state.clone(), guard));
    let admin = Router::new()
        .route(jobs::JOBS_PATH, get(handle_list).fallback(not_found))
        .route(
            "/v1/jobs/{id}/retry",
            post(handle_retry).fallback(not_found),
        )
        .route_layer(from_fn(admin_only))
        .route_layer(from_fn_with_state(state, guard));
    worker.merge(admin)
}

/// Releases every lease that ran out, and records in `/health` any job
/// that had no attempt left.
pub(super) fn expire_leases(state: &AppState) -> anyhow::Result<()> {
    let failed = state.store.expire_leases(OffsetDateTime::now_utc())?;
    if let Some(message) = failed.last() {
        record_failure(state, message);
    }
    Ok(())
}

fn record_failure(state: &AppState, message: &str) {
    eprintln!("{message}");
    state.write().last_merge_error = Some(MergeError {
        message: message.to_string(),
        at: now(),
    });
}

fn bad(message: &str) -> Response {
    error(StatusCode::BAD_REQUEST, message)
}

/// `POST /v1/jobs/claim`: waits up to `wait_seconds` for a job of one of
/// `kinds`, and leases it. Wakes as soon as a push queues one.
pub(super) async fn handle_claim(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let Ok(req) = serde_json::from_slice::<ClaimRequest>(&bytes) else {
        return bad("invalid json body");
    };
    if req.wait_seconds > MAX_WAIT_SECONDS {
        return bad("wait_seconds must be 0 to 30");
    }
    if !(MIN_LEASE_SECONDS..=MAX_LEASE_SECONDS).contains(&req.lease_seconds) {
        return bad("lease_seconds must be 30 to 600");
    }

    // What /health says about the worker, and its CLI, from now on.
    {
        let agent = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let mut rt = state.write();
        rt.worker_last_claim_at = Some(now());
        if let Some(agent) = agent {
            rt.worker_agent = agent;
        } else if let Caller::Device { name, .. } = &caller {
            rt.worker_agent = name.clone();
        }
        if let Some(cli) = &req.claude_cli {
            rt.worker_cli = Some(ClaudeCliStatus {
                checked_at: cli.checked_at.clone(),
                available: Some(cli.available),
                logged_in: Some(cli.logged_in),
                error: cli.error.clone(),
            });
        }
    }

    let lease = Duration::from_secs(req.lease_seconds);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(req.wait_seconds);
    loop {
        // Registered before the queue is read, so a job queued between the
        // read and the wait still wakes this claim.
        let woken = state.jobs_ready.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();

        if let Err(e) = expire_leases(&state) {
            return internal(e);
        }
        let lease_id = match new_id("lse_", 16) {
            Ok(id) => id,
            Err(e) => return internal(e),
        };
        match state
            .store
            .claim_job(&req.kinds, &lease_id, lease, OffsetDateTime::now_utc())
        {
            Ok(Some(job)) => return json(StatusCode::OK, &ClaimResponse { job: Some(job) }),
            Ok(None) => {}
            Err(e) => return internal(e),
        }

        let now = tokio::time::Instant::now();
        if now >= deadline || state.closing.load(Ordering::Relaxed) {
            return json(StatusCode::OK, &ClaimResponse { job: None });
        }
        tokio::select! {
            _ = &mut woken => {}
            _ = tokio::time::sleep_until(deadline.min(now + RECHECK)) => {}
        }
    }
}

/// `POST /v1/jobs/{id}/result`.
pub(super) async fn handle_result(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Response {
    let Ok(req) = serde_json::from_slice::<ResultRequest>(&bytes) else {
        return bad("invalid json body");
    };
    if req.merge.is_some() == req.error.is_some() {
        return bad("a result carries exactly one of merge and error");
    }
    if let Err(e) = expire_leases(&state) {
        return internal(e);
    }
    let Caller::Device { name: worker, .. } = &caller else {
        // worker_only let nothing else through.
        return error(StatusCode::FORBIDDEN, "forbidden");
    };
    let follow_up = match new_id("job_", 10) {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    let settled = state
        .store
        .settle_job(&id, &req, worker, &follow_up, OffsetDateTime::now_utc());
    match settled {
        Ok(Settlement::Recorded(s)) => {
            if s.applied {
                let mut rt = state.write();
                rt.last_merge_at = now();
                rt.last_merge_error = None;
            }
            if let Some(message) = &s.failed {
                record_failure(&state, message);
            }
            if s.queued {
                state.jobs_ready.notify_waiters();
            }
            json(StatusCode::OK, &s.response)
        }
        Ok(Settlement::NotFound) => error(StatusCode::NOT_FOUND, "no job has that id"),
        Ok(Settlement::LeaseEnded) => error(
            StatusCode::CONFLICT,
            "this lease has ended; the job was handed out again",
        ),
        Ok(Settlement::WrongKind) => bad("a merge result for a job that is not a merge"),
        Err(e) => internal(e),
    }
}

/// `GET /v1/jobs?state=`: newest first, without file content.
pub(super) async fn handle_list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let wanted = params
        .get("state")
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    if wanted.is_some_and(|s| !STATES.contains(&s)) {
        return bad("state must be queued, leased, done or failed");
    }
    if let Err(e) = expire_leases(&state) {
        return internal(e);
    }
    match state.store.jobs(wanted, LIST_LIMIT) {
        Ok(jobs) => json(StatusCode::OK, &JobList { jobs }),
        Err(e) => internal(e),
    }
}

/// `POST /v1/jobs/{id}/retry`: queues a failed job again.
pub(super) async fn handle_retry(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    match state.store.retry_job(&id, OffsetDateTime::now_utc()) {
        Ok(Retried::Queued(job)) => {
            state.jobs_ready.notify_waiters();
            json(StatusCode::OK, &job)
        }
        Ok(Retried::NotFound) => error(StatusCode::NOT_FOUND, "no job has that id"),
        Ok(Retried::NotFailed(state)) => error(
            StatusCode::CONFLICT,
            &format!("only a failed job can be retried; this one is {state}"),
        ),
        Err(e) => internal(e),
    }
}

/// Removes finished jobs older than [`DONE_JOBS_KEPT`].
pub(super) fn prune(state: &AppState) -> anyhow::Result<usize> {
    state.store.prune_jobs(&format_timestamp(
        OffsetDateTime::now_utc() - DONE_JOBS_KEPT,
    ))
}
