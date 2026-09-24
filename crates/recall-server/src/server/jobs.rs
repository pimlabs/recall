//! The job routes: a worker claiming merge jobs and posting their results,
//! and the owner listing and retrying them. The store decides what each
//! one means (`store/jobs.rs`); this module is HTTP, the in-memory state
//! `/health` reports, and the drain that merges what is left in the queue
//! here once no worker is.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Router};
use recall_wire::jobs::{
    self, KIND_MERGE, MAX_LEASE_SECONDS, MAX_WAIT_SECONDS, MIN_LEASE_SECONDS, STATES,
};
use recall_wire::{
    ClaimRequest, ClaimResponse, ClaudeCliStatus, JobList, MergeError, MergeResult, ResultRequest,
};
use time::OffsetDateTime;

use super::auth::Caller;
use super::devices::new_id;
use super::handlers::not_found;
use super::middleware::{admin_only, guard, worker_only};
use super::respond::{error, internal, json};
use super::AppState;
use crate::store::{clip, Failure, Retried, Settled, Settlement, MAX_ERROR_BYTES};
use crate::{format_timestamp, now};

/// How often a waiting claim looks at the queue again even when nothing
/// woke it: a job whose retry delay has passed, or whose lease ran out,
/// becomes claimable without a push to announce it.
const RECHECK: Duration = Duration::from_secs(5);

/// The most jobs one listing answers with.
const LIST_LIMIT: usize = 200;

/// Finished jobs are kept this long, for the listing, then removed.
pub(super) const DONE_JOBS_KEPT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The most of a claim's `claude_cli.checked_at` kept: a timestamp is 24
/// characters.
const MAX_CHECKED_AT_BYTES: usize = 64;

/// The most of a worker's `User-Agent` kept for `/health`.
const MAX_AGENT_BYTES: usize = 200;

/// How long the server's own merge of a drained job may hold it, beyond
/// the merge timeout: time to write the result.
const DRAIN_LEASE_MARGIN: Duration = Duration::from_secs(60);

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
    for failure in state.store.expire_leases(OffsetDateTime::now_utc())? {
        record_failure(state, &failure);
    }
    Ok(())
}

/// Logs a failed job with its file, and shows it in `/health` without:
/// `/health` answers anyone, and names a job, never a project or a path.
fn record_failure(state: &AppState, failure: &Failure) {
    eprintln!("{}", failure.logged());
    record_error(state, failure.public());
}

fn record_error(state: &AppState, message: String) {
    state.write().last_merge_error = Some(MergeError { message, at: now() });
}

/// What a recorded result changes in `/health`, and who is woken for it.
fn settled(state: &AppState, s: &Settled) {
    if s.applied {
        let mut rt = state.write();
        rt.last_merge_at = now();
        rt.last_merge_error = None;
    }
    if let Some(failure) = &s.failed {
        record_failure(state, failure);
    }
    if s.queued {
        state.jobs_ready.notify_waiters();
    }
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

    // What /health says about the worker, and its CLI, from now on. Each
    // is the worker's to write and /health shows it to anyone, so each is
    // kept short.
    {
        let agent = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|a| clip(a, MAX_AGENT_BYTES).to_string());
        let mut rt = state.write();
        rt.worker_last_claim_at = Some(now());
        rt.worker_last_claim = Instant::now();
        if let Some(agent) = agent {
            rt.worker_agent = agent;
        } else if let Caller::Device { name, .. } = &caller {
            rt.worker_agent = name.clone();
        }
        if let Some(cli) = &req.claude_cli {
            rt.worker_cli = Some(ClaudeCliStatus {
                checked_at: clip(&cli.checked_at, MAX_CHECKED_AT_BYTES).to_string(),
                available: Some(cli.available),
                logged_in: Some(cli.logged_in),
                error: clip(&cli.error, MAX_ERROR_BYTES).to_string(),
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
            self::settled(&state, &s);
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

/// Merges what is left in the queue here, once no worker is left to.
///
/// Run when the last worker is revoked, and on every sweep after (the first
/// at start), so jobs a revoked worker leaves behind are neither stranded
/// in the queue nor lost. Each goes through the server's own merge and then
/// the same compare-and-swap as a worker's result, attributed to the push
/// that queued it, as an inline merge is. When this server's CLI cannot
/// merge, the jobs are marked failed instead, which `/health` shows, and a
/// retry takes them up once something can.
///
/// Does nothing while a worker is enrolled, or before the first check of
/// this server's CLI has run: a drain at start must not fail every job
/// only because the CLI has not been asked yet.
pub(super) async fn drain_without_worker(state: &Arc<AppState>) -> anyhow::Result<()> {
    // One drain at a time: a revocation and a sweep may both start one.
    if state.draining.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    struct Done<'a>(&'a AppState);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.draining.store(false, Ordering::Release);
        }
    }
    let _done = Done(state);

    if state.store.enrolled_worker()?.is_some() {
        return Ok(());
    }
    let status = state.read().claude_status.clone();
    if state.cfg.merge_enabled && status.checked_at.is_empty() {
        return Ok(());
    }
    let open = state.store.release_open_jobs(OffsetDateTime::now_utc())?;
    if open == 0 {
        return Ok(());
    }
    if !state.cfg.merge_enabled || !status.logged_in {
        let why = if state.cfg.merge_enabled {
            "no worker is enrolled to merge this, and this server's claude CLI cannot \
             (not logged in); retry it once one can"
        } else {
            "no worker is enrolled to merge this, and merging is turned off on this server \
             (RECALL_MERGE_ENABLED); retry it once a worker is enrolled"
        };
        let ids = state.store.fail_open_jobs(why, OffsetDateTime::now_utc())?;
        eprintln!(
            "no worker is enrolled and this server cannot merge: marked {} waiting merge \
             jobs failed: {}",
            ids.len(),
            ids.join(", ")
        );
        record_error(
            state,
            format!(
                "{} merge job{} waiting with no worker enrolled, and this server cannot merge \
                 {}, so {} marked failed; see GET /v1/jobs?state=failed",
                ids.len(),
                if ids.len() == 1 { " was" } else { "s were" },
                if ids.len() == 1 { "it" } else { "them" },
                if ids.len() == 1 { "it is" } else { "they are" },
            ),
        );
        return Ok(());
    }

    eprintln!("no worker is enrolled: merging the {open} jobs left in the queue here");
    let kinds = [KIND_MERGE.to_string()];
    let lease = state.cfg.merge_timeout + DRAIN_LEASE_MARGIN;
    loop {
        // A worker enrolled meanwhile takes what is left.
        if state.store.enrolled_worker()?.is_some() {
            return Ok(());
        }
        let lease_id = new_id("lse_", 16)?;
        let Some(job) =
            state
                .store
                .claim_job(&kinds, &lease_id, lease, OffsetDateTime::now_utc())?
        else {
            return Ok(());
        };
        let Some(m) = job.merge else {
            continue;
        };
        let merged = if m.stored.content == m.incoming.content {
            Ok(m.incoming.content.clone())
        } else {
            state
                .merger
                .merge(&m.stored.content, &m.incoming.content)
                .await
                .map_err(|e| e.to_string())
        };
        let result = match merged {
            Ok(content) => ResultRequest {
                lease_id,
                merge: Some(MergeResult { content }),
                error: None,
            },
            Err(error) => {
                eprintln!(
                    "merge job {} for {}/{} failed here: {error}",
                    job.id, m.project_key, m.file_path
                );
                ResultRequest {
                    lease_id,
                    merge: None,
                    error: Some(error),
                }
            }
        };
        let follow_up = new_id("job_", 10)?;
        match state.store.settle_job(
            &job.id,
            &result,
            &m.incoming.source_env,
            &follow_up,
            OffsetDateTime::now_utc(),
        )? {
            Settlement::Recorded(s) => settled(state, &s),
            other => eprintln!("merge job {} was not settled here: {other:?}", job.id),
        }
    }
}
