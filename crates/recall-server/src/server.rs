//! The HTTP surface.
//!
//! Routes and their auth posture are frozen:
//!
//! | route | auth |
//! |---|---|
//! | `GET /health` | none — uptime tooling holds no secret |
//! | `GET /admin` | none — static markup, no data |
//! | `POST /sync`, `GET /sync`, `GET /admin/stats` | bearer token |
//! | anything else | 404 JSON |

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use recall_wire::{
    AdminStats, ClaudeCliStatus, ErrorResponse, Health, MergeError, MergeStatus, PushRequest,
    PushResponse, SyncResponse,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::merge::{Merger, Status};
use crate::{now, Config, Store};

/// The admin page is embedded so the binary stays self-contained — there is
/// no asset directory to forget to ship.
const ADMIN_HTML: &str = include_str!("../admin.html");

/// The token the page holds lives in sessionStorage on this origin;
/// `default-src 'none'` with `connect-src 'self'` means even a future
/// injection bug there would have nowhere to send it.
const ADMIN_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'";

/// Bounds a single push. Memory files are prose; anything this large is a
/// bug or an attack, not a note.
const MAX_BODY_BYTES: usize = 5 << 20;

const REQUIRED_FIELDS_MSG: &str =
    "project_key, file_path, and content (string) are required, unless deleted is true";

struct Runtime {
    last_backup_at: String,
    last_merge_at: String,
    last_merge_error: Option<MergeError>,
    claude_status: Status,
}

struct AppState {
    cfg: Config,
    store: Arc<Store>,
    merger: Merger,
    started_at: String,
    runtime: RwLock<Runtime>,
    limiter: RateLimiter,
}

impl AppState {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Runtime> {
        self.runtime.read().unwrap_or_else(PoisonError::into_inner)
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Runtime> {
        self.runtime.write().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Server {
    state: Arc<AppState>,
}

impl Server {
    /// Builds a server around an already-open store.
    pub fn new(cfg: Config, store: Arc<Store>) -> Self {
        let limiter = RateLimiter::new(cfg.rate_limit_window, cfg.rate_limit_max);
        let merger = Merger::new(cfg.claude_bin.clone(), cfg.merge_timeout);
        Self {
            state: Arc::new(AppState {
                cfg,
                store,
                merger,
                started_at: now(),
                runtime: RwLock::new(Runtime {
                    last_backup_at: String::new(),
                    last_merge_at: String::new(),
                    last_merge_error: None,
                    claude_status: Status::default(),
                }),
                limiter,
            }),
        }
    }

    /// The router, built separately from binding a port so tests can drive
    /// it without real sockets.
    pub fn router(&self) -> Router {
        let state = self.state.clone();
        Router::new()
            // Go's mux dispatched every method through one guarded handler
            // and 404'd the ones it didn't implement; the method fallbacks
            // keep that shape (and its JSON body) instead of axum's bare
            // 405.
            .route(
                "/sync",
                get(handle_pull).post(handle_push).fallback(not_found),
            )
            .route("/admin/stats", get(handle_admin_stats).fallback(not_found))
            // Registered before the layer, so only these two routes are
            // rate limited and authenticated.
            .route_layer(middleware::from_fn_with_state(state.clone(), guard))
            .route("/health", get(handle_health).fallback(not_found))
            .route("/admin", get(handle_admin_page).fallback(not_found))
            .fallback(not_found)
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .with_state(state)
    }

    /// Re-runs the local `claude auth status` probe.
    pub async fn refresh_claude_status(&self) {
        let status = self.state.merger.check_status().await;
        self.state.write().claude_status = status;
    }

    /// The last known state of the `claude` CLI.
    pub fn claude_status(&self) -> Status {
        self.state.read().claude_status.clone()
    }

    /// Overrides the cached CLI status.
    ///
    /// Exposed so tests can exercise the merge-failure path — reaching it
    /// otherwise needs a real, logged-in CLI on the machine running them.
    pub fn set_claude_status(&self, status: Status) {
        self.state.write().claude_status = status;
    }

    /// Writes a backup now. Failure is logged, never propagated: it becomes
    /// visible through `/health`'s `last_backup_at` going stale.
    pub fn run_backup(&self) {
        run_backup(&self.state);
    }

    /// Starts background work: the first Claude CLI status check, its
    /// refresh loop, and backups. All of it is best-effort — none of it may
    /// take the sync API down.
    pub fn start_background(&self) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        if self.state.cfg.merge_enabled {
            let state = self.state.clone();
            tasks.push(tokio::spawn(async move {
                let every = state.cfg.claude_status_interval;
                loop {
                    let status = state.merger.check_status().await;
                    state.write().claude_status = status;
                    tokio::time::sleep(every).await;
                }
            }));
        }
        if !self.state.cfg.backup_dir.is_empty() {
            let state = self.state.clone();
            tasks.push(tokio::spawn(async move {
                let every = state.cfg.backup_interval;
                loop {
                    // VACUUM INTO can take a while on a large database and
                    // holds the store lock, so it stays off the async
                    // worker threads.
                    let s = state.clone();
                    let _ = tokio::task::spawn_blocking(move || run_backup(&s)).await;
                    tokio::time::sleep(every).await;
                }
            }));
        }
        tasks
    }

    /// Binds `cfg.addr` and serves until SIGTERM or ctrl-c, then shuts down
    /// gracefully so an in-flight merge isn't cut off mid-write.
    pub async fn serve(&self) -> Result<()> {
        let listener = TcpListener::bind(&self.state.cfg.addr)
            .await
            .with_context(|| format!("binding {}", self.state.cfg.addr))?;
        eprintln!(
            "recall server listening on {} (db: {})",
            self.state.cfg.addr, self.state.cfg.db_path
        );
        self.serve_with_shutdown(listener, shutdown_signal()).await
    }

    /// Serves on an already-bound listener until `shutdown` resolves.
    pub async fn serve_with_shutdown<F>(&self, listener: TcpListener, shutdown: F) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let tasks = self.start_background();
        let result = axum::serve(
            listener,
            self.router()
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await;
        for task in tasks {
            task.abort();
        }
        result.map_err(Into::into)
    }
}

fn run_backup(state: &AppState) {
    if state.cfg.backup_dir.is_empty() {
        return;
    }
    match state
        .store
        .backup(&state.cfg.backup_dir, state.cfg.backup_keep)
    {
        Ok(dest) => {
            state.write().last_backup_at = now();
            eprintln!("backup written: {}", dest.display());
        }
        Err(e) => eprintln!("backup failed: {e:#}"),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

// ---------------------------------------------------------------- middleware

/// Rate limiting runs *before* auth, so a flood of invalid tokens is
/// limited too rather than escaping the limiter by never reaching the auth
/// check.
async fn guard(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if state.limiter.limited(&client_ip(&req)) {
        let mut resp = error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded, try again later",
        );
        if let Ok(v) = state
            .cfg
            .rate_limit_window
            .as_secs()
            .to_string()
            .parse::<axum::http::HeaderValue>()
        {
            resp.headers_mut().insert("retry-after", v);
        }
        return resp;
    }
    if !authorized(&state.cfg.token, req.headers()) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}

fn authorized(token: &str, headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    !value.is_empty() && constant_time_eq(value.as_bytes(), token.as_bytes())
}

/// Compared without an early exit so the time taken doesn't reveal how much
/// of a guessed token was right. Lengths are allowed to short-circuit —
/// they leak only the length, as `crypto/subtle` does.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Trusts Cloudflare's header because every request that reaches this
/// process arrives through the tunnel — the origin port is never published
/// (compose uses `expose`, not `ports`), so the header can't be spoofed by
/// hitting the origin directly.
fn client_ip(req: &Request) -> String {
    if let Some(ip) = header_str(req.headers(), "cf-connecting-ip") {
        return ip.to_string();
    }
    if let Some(xff) = header_str(req.headers(), "x-forwarded-for") {
        let first = xff.split(',').next().unwrap_or("").trim();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

// ------------------------------------------------------------------ handlers

async fn handle_push(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
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

    let mut content = req.content.clone();
    let mut merged = false;

    // Merge only when there is genuinely something to reconcile. A
    // brand-new file, a revived tombstone (the delete already expressed
    // intent to discard the old content), or an unchanged re-push all skip
    // straight to a write — cheaper, and it keeps a merge from ever
    // second-guessing content that didn't actually conflict.
    if let Some(stored) = should_merge(&state, existing.as_ref(), &req.content) {
        match state.merger.merge(stored, &req.content).await {
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
) -> Option<&'a str> {
    if !state.cfg.merge_enabled {
        return None;
    }
    let stored = existing.filter(|e| !e.deleted && e.content != incoming)?;
    // Don't even attempt it when the CLI isn't logged in: every attempt
    // would burn a subprocess and a timeout before failing to the same
    // place.
    state
        .read()
        .claude_status
        .logged_in
        .then_some(stored.content.as_str())
}

async fn handle_pull(
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

async fn handle_health(State(state): State<Arc<AppState>>) -> Response {
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

/// Static markup only: the page holds no data, it asks the viewer for a
/// token and fetches `/admin/stats` itself.
async fn handle_admin_page() -> Response {
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

async fn handle_admin_stats(State(state): State<Arc<AppState>>) -> Response {
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

async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, Json(body)).into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    json(
        status,
        &ErrorResponse {
            error: message.to_string(),
        },
    )
}

fn internal(e: anyhow::Error) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

// -------------------------------------------------------------- rate limiter

/// A per-IP fixed window, in memory — enough for a single-owner server, and
/// it needs no external store.
struct RateLimiter {
    window: Duration,
    max: u32,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    buckets: HashMap<String, Bucket>,
    last_sweep: Instant,
}

struct Bucket {
    count: u32,
    window_start: Instant,
}

impl RateLimiter {
    fn new(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            state: Mutex::new(LimiterState {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }

    fn limited(&self, ip: &str) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        // Sweep on the way through rather than from a background task:
        // without it every IP that ever connected would stay in memory for
        // the life of the process, and doing it here keeps the limiter
        // usable with no runtime around it.
        if now.duration_since(state.last_sweep) >= self.window {
            state.last_sweep = now;
            let window = self.window;
            state
                .buckets
                .retain(|_, b| now.duration_since(b.window_start) < 2 * window);
        }

        let bucket = state.buckets.entry(ip.to_string()).or_insert(Bucket {
            count: 0,
            window_start: now,
        });
        if now.duration_since(bucket.window_start) >= self.window {
            bucket.count = 0;
            bucket.window_start = now;
        }
        bucket.count += 1;
        bucket.count > self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_counts_per_ip_and_resets_after_the_window() {
        let rl = RateLimiter::new(Duration::from_millis(40), 2);
        assert!(!rl.limited("a"));
        assert!(!rl.limited("a"));
        assert!(rl.limited("a"), "third request in the window is limited");
        assert!(!rl.limited("b"), "a different IP has its own bucket");

        std::thread::sleep(Duration::from_millis(60));
        assert!(!rl.limited("a"), "the window resets");
    }

    #[test]
    fn rate_limiter_sweeps_stale_buckets() {
        let rl = RateLimiter::new(Duration::from_millis(10), 100);
        for i in 0..50 {
            rl.limited(&format!("10.0.0.{i}"));
        }
        std::thread::sleep(Duration::from_millis(30));
        rl.limited("10.0.1.1");
        let n = rl
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .buckets
            .len();
        assert_eq!(n, 1, "stale buckets should have been swept, got {n}");
    }

    #[test]
    fn bearer_comparison_rejects_everything_but_the_exact_token() {
        let mut h = HeaderMap::new();
        assert!(!authorized("secret", &h), "no header");
        h.insert("authorization", "secret".parse().unwrap());
        assert!(!authorized("secret", &h), "missing Bearer scheme");
        h.insert("authorization", "Bearer ".parse().unwrap());
        assert!(!authorized("secret", &h), "empty token");
        h.insert("authorization", "Bearer secre".parse().unwrap());
        assert!(!authorized("secret", &h), "prefix of the token");
        h.insert("authorization", "Basic secret".parse().unwrap());
        assert!(!authorized("secret", &h), "wrong scheme");
        h.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(authorized("secret", &h));
    }

    #[test]
    fn client_ip_prefers_cloudflare_then_forwarded_then_socket() {
        let build = |headers: Vec<(&str, &str)>| {
            let mut req = Request::new(axum::body::Body::empty());
            req.extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
            for (k, v) in headers {
                let name = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
                req.headers_mut().insert(name, v.parse().unwrap());
            }
            req
        };
        assert_eq!(client_ip(&build(vec![])), "127.0.0.1");
        assert_eq!(
            client_ip(&build(vec![("x-forwarded-for", "203.0.113.9, 10.0.0.1")])),
            "203.0.113.9"
        );
        assert_eq!(
            client_ip(&build(vec![
                ("x-forwarded-for", "203.0.113.9"),
                ("cf-connecting-ip", "198.51.100.4"),
            ])),
            "198.51.100.4"
        );
    }
}
