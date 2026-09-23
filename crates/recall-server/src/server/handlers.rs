//! One function per route, plus the two constants the admin page needs.
//!
//! Status codes and error wording are part of the frozen API surface —
//! `docs/api.md` describes them and `scripts/api-doc-check.sh` asserts them
//! against a running server.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use recall_wire::discovery::{self, Auth, Build, Protocol, ServerInfo};
use recall_wire::{
    AdminStats, ClaudeCliStatus, Health, MergeError, MergeStatus, PushRequest, PushResponse,
    SyncResponse,
};
use recall_wire::{Discovery, PROTOCOL};

use super::respond::{error, internal, json};
use super::AppState;
use crate::now;

/// The admin page is embedded so the binary stays self-contained — there is
/// no asset directory to forget to ship.
const ADMIN_HTML: &str = include_str!("../../assets/admin.html");

/// The token the page holds lives in sessionStorage on this origin;
/// `default-src 'none'` with `connect-src 'self'` means even a future
/// injection bug there would have nowhere to send it.
const ADMIN_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'";

const REQUIRED_FIELDS_MSG: &str =
    "project_key, file_path, and content (string) are required, unless deleted is true";

pub(super) async fn handle_push(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let req: PushRequest = match serde_json::from_slice(&body) {
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
    if recall_wire::validate_file_path(&req.file_path).is_err() {
        return error(
            StatusCode::BAD_REQUEST,
            "file_path must be relative, no traversal",
        );
    }

    let updated_at = now();

    if req.deleted {
        if let Err(e) = state.store.tombstone(
            &req.project_key,
            &req.file_path,
            &req.source_env,
            &updated_at,
        ) {
            return internal(e);
        }
        return json(
            StatusCode::OK,
            &PushResponse {
                ok: true,
                project_key: req.project_key,
                file_path: req.file_path,
                deleted: true,
                merged: false,
                updated_at,
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
    if let Some(stored) = should_merge(
        &state,
        existing.as_ref(),
        &incoming,
        req.base_sha256.as_deref(),
    ) {
        match state.merger.merge(stored, &incoming).await {
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

    if let Err(e) = state.store.upsert(
        &req.project_key,
        &req.file_path,
        &content,
        &req.source_env,
        &updated_at,
    ) {
        return internal(e);
    }
    json(
        StatusCode::OK,
        &PushResponse {
            ok: true,
            project_key: req.project_key,
            file_path: req.file_path,
            deleted: false,
            merged,
            updated_at,
        },
    )
}

/// Returns the stored content to merge against, or `None` when this push
/// needs no reconciliation.
fn should_merge<'a>(
    state: &AppState,
    existing: Option<&'a crate::store::Existing>,
    incoming: &str,
    base_sha256: Option<&str>,
) -> Option<&'a str> {
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
    // Don't even attempt it when the CLI isn't logged in: every attempt
    // would burn a subprocess and a timeout before failing to the same
    // place.
    state
        .read()
        .claude_status
        .logged_in
        .then_some(stored.content.as_str())
}

pub(super) async fn handle_pull(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(project_key) = params.get("project_key").filter(|k| !k.is_empty()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "project_key query param is required",
        );
    };
    match state.store.list(project_key) {
        Ok(files) => json(
            StatusCode::OK,
            &SyncResponse {
                project_key: project_key.clone(),
                files,
            },
        ),
        Err(e) => internal(e),
    }
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

    let rt = state.read();
    let claude_cli = if rt.claude_status.checked_at.is_empty() {
        ClaudeCliStatus::default()
    } else {
        ClaudeCliStatus {
            checked_at: rt.claude_status.checked_at.clone(),
            available: Some(rt.claude_status.available),
            logged_in: Some(rt.claude_status.logged_in),
            error: rt.claude_status.error.clone(),
        }
    };
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

/// Static markup only: the page holds no data, it asks the viewer for a
/// token and fetches `/admin/stats` itself.
pub(super) async fn handle_admin_page() -> Response {
    (
        StatusCode::OK,
        [
            ("content-type", "text/html; charset=utf-8"),
            ("x-content-type-options", "nosniff"),
            ("content-security-policy", ADMIN_CSP),
        ],
        ADMIN_HTML,
    )
        .into_response()
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
