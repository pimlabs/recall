//! One function per route. The admin page itself is in `admin.rs`.
//!
//! Status codes and error wording are part of the frozen API surface —
//! `docs/api.md` describes them and `scripts/api-doc-check.sh` asserts them
//! against a running server.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use axum::Extension;
use recall_wire::discovery::{self, Auth, Build, Protocol, ServerInfo};
use recall_wire::{content_sha256, Discovery, PROTOCOL};
use recall_wire::{
    AdminStats, ClaudeCliStatus, Health, MergeError, MergeSide, MergeStatus, PushRequest,
    PushResponse, SyncResponse, WorkerStatus,
};

use super::auth::{Caller, SignedRequestInfo};
use super::respond::{error, internal, json};
use super::AppState;
use crate::audit::leaf;
use crate::now;
use crate::store::Queued;

const REQUIRED_FIELDS_MSG: &str =
    "project_key, file_path, and content (string) are required, unless deleted is true";

/// How long a worker may go without claiming before its claims count as
/// stopped. A running worker claims at least every half minute (a claim
/// waits 25 seconds), and one busy merging holds a lease, which is looked
/// at separately.
pub(super) const WORKER_STALE: std::time::Duration = std::time::Duration::from_secs(120);

pub(super) async fn handle_push(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    body: Bytes,
) -> Response {
    let mut req: PushRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(_) => {
            // Go's json.Unmarshal tolerates absent fields and reports them
            // through the "required" message below; serde treats them as a
            // parse failure, so the two cases are separated here to keep
            // the frozen wording for each.
            let missing_field = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.as_object()
                        .map(|o| !o.contains_key("project_key") || !o.contains_key("file_path"))
                })
                .unwrap_or(false);
            return error(
                StatusCode::BAD_REQUEST,
                if missing_field {
                    REQUIRED_FIELDS_MSG
                } else {
                    "invalid json body"
                },
            );
        }
    };

    if req.project_key.is_empty() || req.file_path.is_empty() {
        return error(StatusCode::BAD_REQUEST, REQUIRED_FIELDS_MSG);
    }
    match recall_wire::validate_file_path(&req.file_path) {
        Ok(()) => {}
        Err(e @ recall_wire::ValidationError::FilePathTooLong) => {
            return error(StatusCode::BAD_REQUEST, &e.to_string())
        }
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "file_path must be relative, no traversal",
            )
        }
    }
    // Both end up in this push's audit leaf, so both are held to what a
    // real client sends: a key no longer than a path, and a base that is a
    // SHA-256, stored in the leaf as lowercase hex whatever case it came in.
    if let Err(e) = recall_wire::validate_project_key(&req.project_key) {
        return error(StatusCode::BAD_REQUEST, &e.to_string());
    }
    if let Some(base) = &mut req.base_sha256 {
        if let Err(e) = recall_wire::validate_base_sha256(base) {
            return error(StatusCode::BAD_REQUEST, &e.to_string());
        }
        base.make_ascii_lowercase();
    }

    // The name belongs to the key. A push a device signed is recorded under
    // the name that device enrolled as, whatever the body claims, so one
    // machine cannot write as another. A bearer push has no key to go by
    // and keeps the label it sent, exactly as before devices existed.
    if let Some(Extension(Caller::Device { name, .. })) = &caller {
        req.source_env.clone_from(name);
    }
    let caller_ref = caller.as_ref().map(|Extension(c)| c);
    let signed_ref = signed.as_ref().map(|Extension(s)| s);
    let actor = super::audit::actor_for(caller_ref);
    // A push's body is the file, and a delete's names it: neither is kept
    // in the leaf (see `leaf::SignedRequest`).
    let request = super::audit::signed_request_for(signed_ref, None);

    if req.deleted {
        let result = state.store.tombstone_audited(
            &req.project_key,
            &req.file_path,
            &req.source_env,
            |seq, at| {
                leaf::encode(
                    seq,
                    at,
                    leaf::action::DELETE,
                    &actor,
                    leaf::subject_file(&leaf::FileChange {
                        project_key: &req.project_key,
                        file_path: &req.file_path,
                        deleted: true,
                        stored_sha256: &content_sha256(""),
                        base_sha256: req.base_sha256.as_deref(),
                        merged: false,
                        merge_job: None,
                    }),
                    request.as_ref(),
                )
            },
        );
        let updated_at = match result {
            Ok(at) => at,
            Err(e) => return internal(e),
        };
        return json(
            StatusCode::OK,
            &PushResponse {
                ok: true,
                project_key: req.project_key,
                file_path: req.file_path,
                deleted: true,
                merged: false,
                updated_at,
                merge_job: None,
            },
        );
    }

    let existing = match state.store.get(&req.project_key, &req.file_path) {
        Ok(e) => e,
        Err(e) => return internal(e),
    };

    // A non-delete push with no content field at all is malformed — the
    // Node server answers 400 for it, and the wording below matches so a
    // client sees the same message from either implementation.
    let Some(incoming) = req.content.clone() else {
        return error(StatusCode::BAD_REQUEST, REQUIRED_FIELDS_MSG);
    };

    let mut content = incoming.clone();
    let mut merged = false;

    // Merge only when there is genuinely something to reconcile. A
    // brand-new file, a revived tombstone (the delete already expressed
    // intent to discard the old content), or an unchanged re-push all skip
    // straight to a write — cheaper, and it keeps a merge from ever
    // second-guessing content that didn't actually conflict.
    let stale = needs_merge(
        &state,
        existing.as_ref(),
        &incoming,
        req.base_sha256.as_deref(),
    );
    // With a worker enrolled, the merge is its job: the push is stored as
    // sent and answered at once, and the merged file arrives with a later
    // pull. Without one, it runs here, exactly as it did before the queue,
    // and so it does when the worker has stopped taking jobs, or the queue
    // is full, and this server's own CLI can merge.
    if stale.is_some() {
        match state.store.enrolled_worker() {
            Ok(Some(_)) => match merge_here_instead(&state) {
                Ok(None) => return queue_merge(&state, req, incoming, &actor, request.as_ref()),
                Ok(Some(why)) => eprintln!(
                    "{why}, so {}/{} is merged here rather than queued",
                    req.project_key, req.file_path
                ),
                Err(e) => return internal(e),
            },
            Ok(None) => {}
            Err(e) => return internal(e),
        }
    }
    if let Some(stored) = stale.filter(|_| state.read().claude_status.logged_in) {
        match state.merger.merge(&stored.content, &incoming).await {
            Ok(out) => {
                content = out;
                merged = true;
                let mut rt = state.write();
                rt.last_merge_at = now();
                rt.last_merge_error = None;
            }
            Err(e) => {
                // Every merge failure degrades to last-write-wins and still
                // returns 200: a not-yet-configured merge must never be
                // able to take basic syncing down with it.
                eprintln!(
                    "merge failed for {}/{}, falling back to last-write-wins: {e}",
                    req.project_key, req.file_path
                );
                state.write().last_merge_error = Some(MergeError {
                    message: e.to_string(),
                    at: now(),
                });
            }
        }
    }

    let result = state.store.upsert_audited(
        &req.project_key,
        &req.file_path,
        &content,
        &req.source_env,
        |seq, at| {
            leaf::encode(
                seq,
                at,
                leaf::action::PUSH,
                &actor,
                leaf::subject_file(&leaf::FileChange {
                    project_key: &req.project_key,
                    file_path: &req.file_path,
                    deleted: false,
                    stored_sha256: &content_sha256(&content),
                    base_sha256: req.base_sha256.as_deref(),
                    merged,
                    merge_job: None,
                }),
                request.as_ref(),
            )
        },
    );
    let updated_at = match result {
        Ok(at) => at,
        Err(e) => return internal(e),
    };
    json(
        StatusCode::OK,
        &PushResponse {
            ok: true,
            project_key: req.project_key,
            file_path: req.file_path,
            deleted: false,
            merged,
            updated_at,
            merge_job: None,
        },
    )
}

/// Stores a stale push as sent and queues its merge for the worker, in one
/// transaction. Answers as a push always has, `merged: false`, which is
/// exactly what a merge that degraded to last-write-wins looks like, with
/// the job's id beside it.
fn queue_merge(
    state: &AppState,
    req: PushRequest,
    incoming: String,
    actor: &leaf::Actor<'_>,
    request: Option<&leaf::SignedRequest<'_>>,
) -> Response {
    let job_id = match super::devices::new_id("job_", 10) {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    // Stamped with the push's leaf's `at` in the store.
    let side = MergeSide {
        sha256: recall_wire::content_sha256(&incoming),
        content: incoming,
        source_env: req.source_env.clone(),
        updated_at: String::new(),
    };
    // The push's leaf: what it stored is what it sent, and, when it was
    // queued, the job that will merge it.
    let queued = state.store.write_and_queue_merge_audited(
        &req.project_key,
        &req.file_path,
        &side,
        &job_id,
        time::OffsetDateTime::now_utc(),
        |seq, at, queued| {
            leaf::encode(
                seq,
                at,
                leaf::action::PUSH,
                actor,
                leaf::subject_file(&leaf::FileChange {
                    project_key: &req.project_key,
                    file_path: &req.file_path,
                    deleted: false,
                    stored_sha256: &side.sha256,
                    base_sha256: req.base_sha256.as_deref(),
                    merged: false,
                    merge_job: match queued {
                        Queued::Queued(id) => Some(id),
                        Queued::Nothing | Queued::Full => None,
                    },
                }),
                request,
            )
        },
    );
    let (queued, updated_at) = match queued {
        Ok(done) => done,
        Err(e) => return internal(e),
    };
    let merge_job = match queued {
        Queued::Queued(id) => {
            state.jobs_ready.notify_waiters();
            Some(id)
        }
        Queued::Nothing => None,
        Queued::Full => {
            // /health answers anyone, so it names no project and no file;
            // the log does.
            let message = format!(
                "the merge queue is full ({} jobs), so a conflicting push was stored \
                 last-write-wins; is the worker running?",
                crate::store::MAX_OPEN_JOBS,
            );
            eprintln!("{message} ({}/{})", req.project_key, req.file_path);
            state.write().last_merge_error = Some(MergeError { message, at: now() });
            None
        }
    };
    json(
        StatusCode::OK,
        &PushResponse {
            ok: true,
            project_key: req.project_key,
            file_path: req.file_path,
            deleted: false,
            merged: false,
            updated_at,
            merge_job,
        },
    )
}

/// Why a stale push is merged here even though a worker is enrolled, or
/// [`None`] to queue it for the worker as usual.
///
/// Only ever when this server's own CLI can merge: then a worker whose
/// claims have stopped (none for [`WORKER_STALE`], and no job held), or a
/// queue with no room, need not mean last-write-wins. Without the CLI, the
/// push is queued as always, to wait for the worker, or stored
/// last-write-wins when the queue is full.
fn merge_here_instead(state: &AppState) -> anyhow::Result<Option<String>> {
    if !state.read().claude_status.logged_in {
        return Ok(None);
    }
    super::jobs::expire_leases(state)?;
    let queue = state.store.queue_status()?;
    if (queue.queued + queue.leased) as usize >= crate::store::MAX_OPEN_JOBS {
        return Ok(Some(format!(
            "the merge queue is full ({} jobs)",
            crate::store::MAX_OPEN_JOBS
        )));
    }
    let quiet = state.read().worker_last_claim.elapsed();
    if quiet >= WORKER_STALE && queue.leased == 0 {
        return Ok(Some(format!(
            "the merge worker has not asked for work in {}s",
            quiet.as_secs()
        )));
    }
    Ok(None)
}

/// The stored version to merge against, or `None` when this push needs no
/// reconciliation. Whether the merge runs here or is queued for a worker
/// is decided after.
fn needs_merge<'a>(
    state: &AppState,
    existing: Option<&'a crate::store::Existing>,
    incoming: &str,
    base_sha256: Option<&str>,
) -> Option<&'a crate::store::Existing> {
    if !state.cfg.merge_enabled {
        return None;
    }
    let stored = existing.filter(|e| !e.deleted && e.content != incoming)?;
    // The client names the version its edit started from. If that is what
    // is stored, nothing happened in between: this is the next edit, not a
    // concurrent one, and it replaces the stored version outright.
    //
    // Merging it anyway is what this used to do, and it cannot delete: the
    // merge keeps every distinct fact from both versions, so a line removed
    // on purpose is a fact from the stored side and comes back — and so does
    // a CONFLICT marker someone has just resolved. A client that sends no
    // base keeps the old behaviour.
    if base_sha256.is_some_and(|base| {
        base.eq_ignore_ascii_case(&recall_wire::content_sha256(&stored.content))
    }) {
        return None;
    }
    // An inline merge is not even attempted when this server's CLI is not
    // logged in (the caller checks, since a queued merge does not need
    // it): every attempt would burn a subprocess and a timeout before
    // failing to the same place.
    Some(stored)
}

pub(super) async fn handle_pull(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(project_key) = params.get("project_key").filter(|k| !k.is_empty()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "project_key query param is required",
        );
    };
    // It is named in this pull's audit leaf, so it is held to the length a
    // push's is.
    if let Err(e) = recall_wire::validate_project_key(project_key) {
        return error(StatusCode::BAD_REQUEST, &e.to_string());
    }
    let files = match state.store.list(project_key) {
        Ok(files) => files,
        Err(e) => return internal(e),
    };

    // A pull gets a leaf too — the design's own call, so a witness of the
    // log (a checkpoint saved from an earlier pull) has something to check
    // reads against, not only writes.
    let caller_ref = caller.as_ref().map(|Extension(c)| c);
    let signed_ref = signed.as_ref().map(|Extension(s)| s);
    let actor = super::audit::actor_for(caller_ref);
    // A pull has no body; its signature binds the project through @query.
    let request = super::audit::signed_request_for(signed_ref, None);
    if let Err(e) = state.store.audit_append(|seq, at| {
        leaf::encode(
            seq,
            at,
            leaf::action::PULL,
            &actor,
            leaf::subject_pull(project_key),
            request.as_ref(),
        )
    }) {
        return internal(e);
    }

    let mut resp = json(
        StatusCode::OK,
        &SyncResponse {
            project_key: project_key.clone(),
            files,
        },
    );
    // So every pull leaves the client a checkpoint without another
    // request — see docs/design/part5-plan.md's "Who witnesses".
    let (tree_size, root) = state.store.audit_checkpoint();
    let checkpoint = recall_wire::AuditCheckpoint {
        tree_size,
        root_hash: super::audit::base64_hash(&root),
    };
    if let Ok(value) = HeaderValue::from_str(&checkpoint.to_header_value()) {
        resp.headers_mut()
            .insert(recall_wire::audit::CHECKPOINT_HEADER, value);
    }
    resp
}

/// The stamp `deploy/backup-offbox.sh` writes after a verified copy.
///
/// Read per request rather than cached, and on purpose: it is written by a
/// cron job in another process, so a cached value would report a backup that
/// stopped hours ago as current — which is the exact failure this exists to
/// surface. A `/health` call is rare and the file is one line.
///
/// Any failure to read it is an empty string. A missing file is the ordinary
/// case for a deployment with no off-box backup configured, and that is not
/// an error to report.
fn last_offbox_at(backup_dir: &str) -> String {
    if backup_dir.is_empty() {
        return String::new();
    }
    std::fs::read_to_string(std::path::Path::new(backup_dir).join(".last-offbox"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(super) async fn handle_health(State(state): State<Arc<AppState>>) -> Response {
    let last_sync_at = match state.store.last_sync_at() {
        Ok(v) => v,
        Err(e) => return internal(e),
    };

    // With a worker enrolled, the merge is its business: /health shows its
    // CLI rather than this process's. The queue is shown while there is a
    // worker, and whenever the queue holds anything, worker or not: jobs a
    // revoked worker left, waiting to be drained here, or failed ones, are
    // exactly what must not drop out of sight with it.
    let worker = match state.store.enrolled_worker() {
        Ok(w) => w,
        Err(e) => return internal(e),
    };
    if let Err(e) = super::jobs::expire_leases(&state) {
        return internal(e);
    }
    let queue = match state.store.queue_status() {
        Ok(q) if worker.is_some() || q.queued + q.leased + q.failed > 0 => Some(q),
        Ok(_) => None,
        Err(e) => return internal(e),
    };

    let rt = state.read();
    let claude_cli = if worker.is_some() {
        rt.worker_cli.clone().unwrap_or_default()
    } else if rt.claude_status.checked_at.is_empty() {
        ClaudeCliStatus::default()
    } else {
        ClaudeCliStatus {
            checked_at: rt.claude_status.checked_at.clone(),
            available: Some(rt.claude_status.available),
            logged_in: Some(rt.claude_status.logged_in),
            error: rt.claude_status.error.clone(),
        }
    };
    let worker = worker.map(|device| WorkerStatus {
        last_claim_at: rt.worker_last_claim_at.clone(),
        agent: if rt.worker_agent.is_empty() {
            device.agent
        } else {
            rt.worker_agent.clone()
        },
    });
    let body = Health {
        status: "ok".to_string(),
        git_commit: state.cfg.git_commit.clone(),
        started_at: state.started_at.clone(),
        last_sync_at,
        last_backup_at: rt.last_backup_at.clone(),
        last_offbox_at: last_offbox_at(&state.cfg.backup_dir),
        merge: MergeStatus {
            enabled: state.cfg.merge_enabled,
            claude_cli,
            last_merge_at: rt.last_merge_at.clone(),
            last_merge_error: rt.last_merge_error.clone(),
            worker,
            queue,
        },
    };
    drop(rt);
    json(StatusCode::OK, &body)
}

/// The oldest client this server accepts. Every client released so far
/// speaks protocol 1 and is served; this is where that changes when a
/// breaking release stops serving an old one.
const MIN_CLIENT: &str = "0.1.0";

/// `GET /.well-known/recall`: what this server is and what it speaks.
///
/// Unauthenticated and unlimited, like `/health`: it holds nothing a client
/// could not learn by trying, and a client needs it before it knows whether
/// it can authenticate at all.
pub(super) async fn handle_discovery(State(state): State<Arc<AppState>>) -> Response {
    // The commit the container was started with, when the deploy set one;
    // otherwise whatever the build itself recorded.
    let revision = Some(state.cfg.git_commit.clone())
        .filter(|c| !c.is_empty() && c != "unknown")
        .or_else(|| discovery::revision().map(str::to_string));
    let channel = discovery::channel();
    let mut capabilities = std::collections::BTreeMap::new();
    capabilities.insert(
        discovery::CAPABILITY_DEVICES.to_string(),
        serde_json::to_value(super::devices::capability()).unwrap_or_default(),
    );
    capabilities.insert("merge_base".to_string(), serde_json::json!({}));
    // Stale pushes can be queued for a worker, and the job routes exist.
    // Listed whether or not a worker is enrolled now: it says what this
    // server can do, and a worker needs it before it enrols.
    capabilities.insert(
        discovery::CAPABILITY_MERGE_QUEUE.to_string(),
        serde_json::json!({}),
    );
    // Reports on what memory holds, made by a worker: the routes and the
    // `evaluate` job. Listed whether or not a worker is enrolled, like the
    // merge queue.
    capabilities.insert(
        discovery::CAPABILITY_EVALUATION.to_string(),
        serde_json::json!({}),
    );
    capabilities.insert(
        discovery::CAPABILITY_AUDIT.to_string(),
        serde_json::to_value(recall_wire::AuditCapability {
            leaf_version: recall_wire::audit::LEAF_VERSION,
            max_page: recall_wire::audit::MAX_PAGE,
            max_page_bytes: recall_wire::audit::MAX_PAGE_BYTES as u64,
        })
        .unwrap_or_default(),
    );
    capabilities.insert(
        "scopes".to_string(),
        serde_json::json!({ "kinds": ["project", "global", "machine"] }),
    );
    capabilities.insert(
        "limits".to_string(),
        serde_json::json!({
            "max_body_bytes": super::MAX_BODY_BYTES,
            "rate_limit": {
                "max": state.cfg.rate_limit_max,
                "window_seconds": state.cfg.rate_limit_window.as_secs(),
            },
        }),
    );
    json(
        StatusCode::OK,
        &Discovery {
            protocol: Protocol {
                current: PROTOCOL,
                supported: vec![PROTOCOL],
            },
            server: ServerInfo {
                version: discovery::version_for(channel, revision.as_deref()),
                build: Build {
                    channel: channel.to_string(),
                    revision,
                    created: discovery::created().map(str::to_string),
                },
            },
            min_client: MIN_CLIENT.to_string(),
            // Appended, never reordered or removed within a protocol: a
            // client reading this list may be older than any entry in it.
            auth: Auth {
                methods: vec![
                    discovery::AUTH_BEARER.to_string(),
                    discovery::AUTH_DEVICE_SIG.to_string(),
                ],
            },
            capabilities,
        },
    )
}

pub(super) async fn handle_admin_stats(State(state): State<Arc<AppState>>) -> Response {
    let (projects, totals) = match state.store.admin_stats() {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let last_backup_at = state.read().last_backup_at.clone();
    json(
        StatusCode::OK,
        &AdminStats {
            projects,
            totals,
            git_commit: state.cfg.git_commit.clone(),
            last_backup_at,
        },
    )
}

pub(super) async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}
