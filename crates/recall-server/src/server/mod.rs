//! The HTTP surface.
//!
//! Routes and their auth posture are frozen:
//!
//! | route | auth |
//! |---|---|
//! | `GET /health` | none — uptime tooling holds no secret |
//! | `GET /admin` | none — static markup, no data |
//! | `GET /.well-known/recall` | none — a client asks before it can authenticate |
//! | `POST /sync`, `GET /sync`, `GET /v1/devices/me` | bearer token, or any device's signature |
//! | `POST /v1/devices/enroll`, `POST /v1/devices/enroll/poll` | none, but rate limited, and small bodies only |
//! | `GET /admin/stats`, the rest of `/v1/devices`, and `/v1/authkeys` | bearer token, or an admin device's signature |
//! | anything else | 404 JSON |
//!
//! This module owns the shared state, the router, and the background jobs.
//! What it wires together are private submodules, each living next to its
//! own tests: `middleware.rs` (rate limiting, then the protocol check, then
//! auth), `auth.rs` (device signatures and the replay cache),
//! `handlers.rs` (one function per route), `devices.rs` (the device
//! routes), `respond.rs` (the JSON shape of every reply, errors included)
//! and `limit.rs` (the per-IP window the middleware consults).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
// Imported by name because this module has a `middleware` of its own, and
// an unqualified `middleware::` would resolve to that one.
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};
use axum::Router;
use recall_wire::devices as paths;
use recall_wire::MergeError;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::merge::{Merger, Status};
use crate::{format_timestamp, now, Config, Store};

mod auth;
mod devices;
mod handlers;
mod limit;
mod middleware;
mod respond;

use auth::ReplayCache;
use devices::{
    handle_approve, handle_create_authkey, handle_deny, handle_enroll, handle_list_authkeys,
    handle_list_devices, handle_me, handle_pending, handle_poll, handle_revoke_authkey,
    handle_revoke_device,
};
use handlers::{
    handle_admin_page, handle_admin_stats, handle_discovery, handle_health, handle_pull,
    handle_push, not_found,
};
use limit::RateLimiter;
use middleware::{admin_only, guard, limited};

/// How often idle ephemeral devices and long-expired enrolments are swept
/// away. Removal is at most this late, which against a TTL counted in
/// hours is nothing.
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Bounds a single push. Memory files are prose; anything this large is a
/// bug or an attack, not a note.
const MAX_BODY_BYTES: usize = 5 << 20;

/// Bounds a request to the routes anyone may call. An enrolment is a name,
/// a key and an agent, and a poll is an id; nobody who has not proved
/// anything gets to make the server hold megabytes.
const ENROLL_BODY_BYTES: usize = 8 << 10;

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
    /// When this process started, as a UNIX time: signatures the process
    /// before could have accepted are refused, since the nonces that would
    /// catch their replay were in its memory (see `auth.rs`). Moved only
    /// by tests, through [`Server::backdate_start`].
    started_unix: AtomicI64,
    /// Added to the clock signatures are judged by. Zero except in tests.
    clock_offset: AtomicI64,
    runtime: RwLock<Runtime>,
    limiter: RateLimiter,
    replay: ReplayCache,
}

impl AppState {
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Runtime> {
        self.runtime.read().unwrap_or_else(PoisonError::into_inner)
    }
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Runtime> {
        self.runtime.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The UNIX time signatures are judged by.
    fn now(&self) -> i64 {
        unix_now() + self.clock_offset.load(Ordering::Relaxed)
    }

    /// When this process started, as a UNIX time.
    fn started(&self) -> i64 {
        self.started_unix.load(Ordering::Relaxed)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// How many live nonces one device may have: as many requests as one
/// address may make while a nonce stays live, which is up to the window
/// and the few seconds `created` may be ahead of the clock.
fn nonces_per_device(cfg: &Config) -> usize {
    let live_ms = auth::NONCE_LIFETIME as u128 * 1000;
    let window_ms = cfg.rate_limit_window.as_millis().max(1);
    let windows = live_ms.div_ceil(window_ms);
    (cfg.rate_limit_max as u128 * windows).min(usize::MAX as u128) as usize
}

/// The HTTP API, its background jobs, and the state they share.
pub struct Server {
    state: Arc<AppState>,
}

impl Server {
    /// Builds a server around an already-open store.
    pub fn new(cfg: Config, store: Arc<Store>) -> Self {
        let limiter = RateLimiter::new(cfg.rate_limit_window, cfg.rate_limit_max);
        let merger = Merger::new(cfg.claude_bin.clone(), cfg.merge_timeout);
        let replay = ReplayCache::new(auth::WINDOW, nonces_per_device(&cfg));
        Self {
            state: Arc::new(AppState {
                cfg,
                store,
                merger,
                started_at: now(),
                started_unix: AtomicI64::new(unix_now()),
                clock_offset: AtomicI64::new(0),
                runtime: RwLock::new(Runtime {
                    last_backup_at: String::new(),
                    last_merge_at: String::new(),
                    last_merge_error: None,
                    claude_status: Status::default(),
                }),
                limiter,
                replay,
            }),
        }
    }

    /// The router, built separately from binding a port so tests can drive
    /// it without real sockets.
    pub fn router(&self) -> Router {
        let state = self.state.clone();
        // Managing devices: authenticated, then held to the admin scope.
        // The last layer added runs first, so `guard` has put the caller
        // in place by the time `admin_only` looks for it.
        let admin = Router::new()
            .route("/admin/stats", get(handle_admin_stats).fallback(not_found))
            .route(
                paths::DEVICES_PATH,
                get(handle_list_devices).fallback(not_found),
            )
            .route(
                paths::APPROVE_PATH,
                post(handle_approve).fallback(not_found),
            )
            .route(paths::DENY_PATH, post(handle_deny).fallback(not_found))
            .route(
                "/v1/devices/pending/{user_code}",
                get(handle_pending).fallback(not_found),
            )
            .route(
                "/v1/devices/{id}/revoke",
                post(handle_revoke_device).fallback(not_found),
            )
            .route(
                paths::AUTHKEYS_PATH,
                get(handle_list_authkeys)
                    .post(handle_create_authkey)
                    .fallback(not_found),
            )
            .route(
                "/v1/authkeys/{id}/revoke",
                post(handle_revoke_authkey).fallback(not_found),
            )
            .route_layer(from_fn(admin_only))
            .route_layer(from_fn_with_state(state.clone(), guard));
        // Enrolling: a machine has no credential yet, so no auth, but the
        // same rate limit and protocol check as everything else, and a
        // body limit sized for what an enrolment is. The inner limit wins
        // over the router-wide one below.
        let enrolment = Router::new()
            .route(paths::ENROLL_PATH, post(handle_enroll).fallback(not_found))
            .route(
                paths::ENROLL_POLL_PATH,
                post(handle_poll).fallback(not_found),
            )
            .route_layer(DefaultBodyLimit::max(ENROLL_BODY_BYTES))
            .route_layer(from_fn_with_state(state.clone(), limited));
        Router::new()
            // Go's mux dispatched every method through one guarded handler
            // and 404'd the ones it didn't implement; the method fallbacks
            // keep that shape (and its JSON body) instead of axum's bare
            // 405.
            .route(
                "/sync",
                get(handle_pull).post(handle_push).fallback(not_found),
            )
            .route(paths::DEVICES_ME_PATH, get(handle_me).fallback(not_found))
            // Registered before the layer, so only these routes are rate
            // limited and authenticated here.
            .route_layer(from_fn_with_state(state.clone(), guard))
            .merge(admin)
            .merge(enrolment)
            .route("/health", get(handle_health).fallback(not_found))
            .route(
                recall_wire::DISCOVERY_PATH,
                get(handle_discovery).fallback(not_found),
            )
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

    /// Moves the clock device signatures are judged by, in seconds.
    ///
    /// Exposed so tests can reach the edges of the signature window, and
    /// what happens after it, without waiting a minute for each.
    pub fn set_clock_offset(&self, seconds: i64) {
        self.state.clock_offset.store(seconds, Ordering::Relaxed);
    }

    /// Makes this server act as if it had started `seconds` earlier than
    /// it did.
    ///
    /// Exposed so tests can sign requests at once, rather than waiting out
    /// the few seconds after a start in which every signature is refused.
    pub fn backdate_start(&self, seconds: i64) {
        self.state
            .started_unix
            .fetch_sub(seconds, Ordering::Relaxed);
    }

    /// Writes a backup now. Failure is logged, never propagated: it becomes
    /// visible through `/health`'s `last_backup_at` going stale.
    pub fn run_backup(&self) {
        run_backup(&self.state);
    }

    /// Removes ephemeral devices idle for longer than
    /// [`Config::ephemeral_device_ttl`], and enrolments that expired over
    /// an hour ago. Answers how many of each went.
    pub fn sweep_devices(&self) -> Result<(usize, usize)> {
        sweep_devices(&self.state)
    }

    /// Starts background work: the first Claude CLI status check, its
    /// refresh loop, backups, and the device sweep. All of it is
    /// best-effort — none of it may take the sync API down.
    pub fn start_background(&self) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        {
            let state = self.state.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let s = state.clone();
                    match tokio::task::spawn_blocking(move || sweep_devices(&s)).await {
                        Ok(Ok((0, 0))) => {}
                        Ok(Ok((devices, enrollments))) => eprintln!(
                            "removed {devices} idle ephemeral devices and {enrollments} expired enrolments"
                        ),
                        Ok(Err(e)) => eprintln!("device sweep failed: {e:#}"),
                        Err(_) => {}
                    }
                    tokio::time::sleep(SWEEP_EVERY).await;
                }
            }));
        }
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

fn sweep_devices(state: &AppState) -> Result<(usize, usize)> {
    let now = time::OffsetDateTime::now_utc();
    state.store.sweep_devices(
        &format_timestamp(now - state.cfg.ephemeral_device_ttl),
        &format_timestamp(now - devices::EXPIRED_ENROLLMENT_KEPT),
    )
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
