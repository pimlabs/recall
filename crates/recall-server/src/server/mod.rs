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
//!
//! This module owns the shared state, the router, and the background jobs.
//! The things it wires together are private submodules, each living next to
//! its own tests: `middleware.rs` (rate limiting, then auth), `handlers.rs`
//! (one function per route), `respond.rs` (the JSON shape of every reply,
//! errors included), `limit.rs` (the per-IP window the middleware
//! consults), and `tls.rs` (the direct-TLS accept loop, used only when
//! `Config::tls` is on; plain HTTP, the default, never touches it).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
// Imported by name because this module has a `middleware` of its own, and
// an unqualified `middleware::` would resolve to that one.
use axum::middleware::from_fn_with_state;
use axum::routing::get;
use axum::Router;
use recall_wire::MergeError;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::TlsMode;
use crate::merge::{Merger, Status};
use crate::{now, Config, Store};

mod handlers;
mod limit;
mod middleware;
mod respond;
mod tls;

use handlers::{
    handle_admin_page, handle_admin_stats, handle_discovery, handle_health, handle_pull,
    handle_push, not_found,
};
use limit::RateLimiter;
use middleware::guard;

/// Bounds a single push. Memory files are prose; anything this large is a
/// bug or an attack, not a note.
const MAX_BODY_BYTES: usize = 5 << 20;

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

/// The HTTP API, its background jobs, and the state they share.
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
            .route_layer(from_fn_with_state(state.clone(), guard))
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
        self.serve_with_shutdown(listener, shutdown_signal()).await
    }

    /// Serves on an already-bound listener until `shutdown` resolves, in
    /// whichever transport `cfg.tls` names (see `server/tls.rs`; plain HTTP,
    /// the default, still goes through `axum::serve` directly, unchanged).
    pub async fn serve_with_shutdown<F>(&self, listener: TcpListener, shutdown: F) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // The certificate is loaded (or the ACME state built) before
        // anything claims the server is up, so a bad path or an unreadable
        // key is the last line in the log rather than one after
        // "listening".
        let transport = match &self.state.cfg.tls {
            TlsMode::Off => None,
            mode => Some(tls::prepare(mode).await?),
        };
        eprintln!(
            "recall server listening on {} ({}, db: {})",
            listener
                .local_addr()
                .map_or_else(|_| self.state.cfg.addr.clone(), |a| a.to_string()),
            transport
                .as_ref()
                .map_or("plain http", tls::Prepared::description),
            self.state.cfg.db_path
        );
        let tasks = self.start_background();
        let result = match transport {
            None => axum::serve(
                listener,
                self.router()
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(Into::into),
            Some(prepared) => {
                // axum-server runs its own accept loop rather than
                // axum::serve's, so the listener crosses over to std here.
                // It is already non-blocking (tokio bound it), which is
                // exactly what tokio::net::TcpListener::from_std, which
                // axum-server calls internally, requires.
                let listener = listener.into_std().context("preparing the TLS listener")?;
                let limits = tls::Limits::from_config(&self.state.cfg);
                tls::serve(self.router(), listener, prepared, limits, shutdown).await
            }
        };
        for task in tasks {
            task.abort();
        }
        result
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
