//! Serving the HTTP API over TLS instead of behind an ingress: a static
//! certificate/key pair, or one issued and renewed automatically over
//! TLS-ALPN-01.
//!
//! Kept separate from `mod.rs` on purpose, so the plain-HTTP path every
//! existing deployment (`deploy/docker-compose.yml`,
//! `docker-compose.traefik.yml`) runs through is untouched by this: `mod.rs`
//! only reaches this module at all when [`Config::tls`](crate::Config::tls)
//! is on, and the two are otherwise independent accept loops.
//!
//! The client-IP rule this exists to uphold lives in `config.rs`, not here:
//! by the time a [`TlsMode`] reaches [`prepare`], `RECALL_TRUSTED_IP_HEADER`
//! has already been forced empty (or refused the server outright, if it
//! named a header), so the client IP the rate limiter sees is always the
//! raw TCP peer address `axum-server` hands to `ConnectInfo`, counted the
//! same way as it would be with no header configured at all (through
//! `middleware.rs`'s one `bucket` function, so an IPv6 peer is its /64 and
//! an IPv4-mapped one its IPv4 address). Nothing in this module reads a
//! header for that purpose, and [`serve`] serves the same router, every
//! route group and auth layer included, that plain HTTP does.
//!
//! What this module does own is the connection hardening an ingress would
//! otherwise have provided, since here the socket is the internet's to open:
//!
//! - a cap on open connections, handshakes included ([`Limits::max_connections`]);
//! - a TLS handshake deadline, in both modes ([`Limits::handshake_timeout`]);
//! - hyper's own HTTP/1 header deadline, which also closes an idle
//!   keep-alive connection, and HTTP/2 keep-alive pings, both of which need
//!   a timer axum-server never gives hyper by default;
//! - an idle deadline for everything hyper has no timer for: a connection
//!   that completes the handshake and then sends nothing, or an HTTP/2
//!   connection with no stream open ([`Limits::idle_timeout`]), plus a
//!   ceiling on how long a request (response body included) keeps its
//!   connection counted as busy ([`Limits::request_timeout`]).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::body::{Body, Bytes, HttpBody};
use axum::http::Response;
use axum::Router;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use axum_server::Handle;
use futures_util::StreamExt;
use http_body::{Frame, SizeHint};
use hyper_util::rt::{TokioExecutor, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use rustls_acme::axum::AxumAcceptor;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, AcmeState, EventError, EventOk};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{Instant, Sleep};

use crate::config::TlsMode;
use crate::Config;

/// How often files mode re-reads its certificate and key even without a
/// SIGHUP, so a renewal nobody signalled still lands well before the old
/// certificate expires (Let's Encrypt renews with 30 days to spare).
const RELOAD_EVERY: Duration = Duration::from_secs(12 * 60 * 60);

/// Everything that bounds what one connection may cost this process.
///
/// Fixed rather than configurable, apart from the connection cap: these are
/// the ordinary values for a server nobody sits in front of, and a knob for
/// each would be a knob to get wrong.
#[derive(Debug, Clone)]
pub(super) struct Limits {
    /// Open connections, counting ones still in their TLS handshake. The
    /// one beyond this is closed as soon as it is accepted.
    pub max_connections: usize,
    /// How long a client gets to finish the TLS handshake.
    pub handshake_timeout: Duration,
    /// How long an HTTP/1 client gets to send a request's headers, timed
    /// from when the server starts waiting for them, so it also closes a
    /// keep-alive connection that sits idle this long between requests.
    pub header_read_timeout: Duration,
    /// How long a connection may go with no request in flight before it
    /// is closed: the backstop for what `header_read_timeout` cannot see
    /// (silence straight after the handshake, before hyper knows whether
    /// the client speaks HTTP/1 or HTTP/2, and an HTTP/2 connection with no
    /// open stream).
    pub idle_timeout: Duration,
    /// How long a request, from its headers to the last byte of its
    /// response, keeps its connection counted as busy. Past this the idle
    /// deadline applies again, so a client that stops reading a response
    /// (a zero HTTP/2 flow-control window, or a full TCP buffer) cannot
    /// hold its connection open forever. Derived from the merge timeout, a
    /// request's longest legitimate wait.
    pub request_timeout: Duration,
    /// How often an HTTP/2 connection is pinged, and how long the peer has
    /// to answer before the connection is dropped as dead.
    pub h2_keep_alive_interval: Duration,
    /// See [`h2_keep_alive_interval`](Self::h2_keep_alive_interval).
    pub h2_keep_alive_timeout: Duration,
}

impl Limits {
    pub(super) fn from_config(cfg: &Config) -> Self {
        Self {
            max_connections: cfg.tls_max_connections,
            handshake_timeout: Duration::from_secs(10),
            header_read_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(30),
            request_timeout: cfg.merge_timeout + Duration::from_secs(60),
            h2_keep_alive_interval: Duration::from_secs(20),
            h2_keep_alive_timeout: Duration::from_secs(10),
        }
    }
}

/// `rustls` needs one crypto provider installed as the process default
/// before it will build a `ServerConfig`, and this server never wants the
/// default (`aws-lc-rs`): see the workspace `Cargo.toml` for why. Called
/// once, right before the first TLS config is built, so a plain-HTTP
/// deployment never touches `rustls` at all.
fn install_ring_provider() {
    // A second install (two TLS servers in one process, which the test
    // suite does) returns an error rather than a working no-op; ignored
    // rather than unwrapped, since the first install already did the job.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A TLS mode with its certificate already loaded (files) or its ACME
/// state already built, and the background work that keeps it current.
///
/// Split from [`serve`] so `mod.rs` can report "listening" only once there
/// is actually a certificate to serve with: a bad path or an unreadable
/// key fails here, before anything claims the server is up.
pub(super) struct Prepared {
    acceptor: TlsAcceptor,
    tasks: Vec<JoinHandle<()>>,
    description: String,
}

enum TlsAcceptor {
    Files(RustlsConfig),
    Acme(AxumAcceptor),
}

impl Prepared {
    /// How `mod.rs`'s startup line describes the transport.
    pub(super) fn description(&self) -> &str {
        &self.description
    }
}

/// Loads the certificate (files mode) or builds the ACME state (ACME mode)
/// for `tls`, and starts what keeps it current: the reload loop, or
/// certificate issuance and renewal.
///
/// `tls` must not be [`TlsMode::Off`]; `mod.rs` only calls this module when
/// it isn't.
pub(super) async fn prepare(tls: &TlsMode) -> Result<Prepared> {
    install_ring_provider();
    match tls {
        TlsMode::Off => unreachable!("mod.rs only reaches this module when TLS is configured"),
        TlsMode::Files {
            cert_path,
            key_path,
        } => {
            warn_if_key_exposed(key_path);
            let loaded = CertFiles::read(cert_path, key_path)?;
            let config = RustlsConfig::from_pem(loaded.cert.clone(), loaded.key.clone())
                .await
                .with_context(|| {
                    format!("loading TLS certificate {cert_path} and key {key_path}")
                })?;
            let reloader = spawn_reloader(
                config.clone(),
                cert_path.clone(),
                key_path.clone(),
                loaded,
                RELOAD_EVERY,
            )?;
            Ok(Prepared {
                acceptor: TlsAcceptor::Files(config),
                tasks: vec![reloader],
                description: format!("tls, certificate {cert_path}"),
            })
        }
        TlsMode::Acme {
            domains,
            email,
            cache_dir,
            staging,
        } => {
            let directory = if *staging {
                rustls_acme::acme::LETS_ENCRYPT_STAGING_DIRECTORY
            } else {
                rustls_acme::acme::LETS_ENCRYPT_PRODUCTION_DIRECTORY
            };
            let (acceptor, state) = acme(domains, email, cache_dir, directory)?;
            Ok(Prepared {
                acceptor: TlsAcceptor::Acme(acceptor),
                tasks: vec![spawn_acme_events(state, cache_dir.clone())],
                description: format!(
                    "tls, acme for {}{}",
                    domains.join(","),
                    if *staging { " (staging)" } else { "" }
                ),
            })
        }
    }
}

/// Serves `router` over TLS on an already-bound listener, until `shutdown`
/// resolves.
pub(super) async fn serve<F>(
    router: Router,
    listener: std::net::TcpListener,
    prepared: Prepared,
    limits: Limits,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let Prepared {
        acceptor, tasks, ..
    } = prepared;
    let app = router.into_make_service_with_connect_info::<SocketAddr>();
    let handle = Handle::new();
    spawn_shutdown(handle.clone(), shutdown);
    let limits = Arc::new(limits);
    let permits = Arc::new(Semaphore::new(limits.max_connections));
    let refusals = Arc::new(Refusals::default());

    // One arm per acceptor type rather than one generic function: the
    // bounds axum-server puts on an acceptor are long, and each arm below
    // is checked against them with its concrete type instead.
    let result = match acceptor {
        TlsAcceptor::Files(config) => {
            let tls = RustlsAcceptor::new(config).handshake_timeout(limits.handshake_timeout);
            let mut server = axum_server::from_tcp(listener)?.acceptor(Hardened {
                tls,
                limits: limits.clone(),
                permits,
                refusals,
            });
            configure_http(server.http_builder(), &limits);
            server.handle(handle).serve(app).await
        }
        TlsAcceptor::Acme(tls) => {
            let mut server = axum_server::from_tcp(listener)?.acceptor(Hardened {
                tls,
                limits: limits.clone(),
                permits,
                refusals,
            });
            configure_http(server.http_builder(), &limits);
            server.handle(handle).serve(app).await
        }
    };
    for task in tasks {
        task.abort();
    }
    result.map_err(Into::into)
}

/// axum-server builds hyper's connection builder with no timer at all, and
/// without one hyper silently disables every timeout it has, header read
/// included. Setting the timer is what makes the two deadlines here real.
fn configure_http(builder: &mut Builder<TokioExecutor>, limits: &Limits) {
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(limits.h2_keep_alive_interval)
        .keep_alive_timeout(limits.h2_keep_alive_timeout);
}

/// Ties graceful shutdown to `axum-server`'s own `Handle`, the equivalent of
/// `axum::serve(..).with_graceful_shutdown(shutdown)` on the plain-HTTP
/// path in `mod.rs`.
fn spawn_shutdown<F>(handle: Handle<SocketAddr>, shutdown: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        shutdown.await;
        // Same grace period the plain-HTTP path gives in-flight requests.
        handle.graceful_shutdown(Some(Duration::from_secs(30)));
    });
}

// ---------------------------------------------------------------------------
// Files mode: loading, and reloading on SIGHUP or a timer.
// ---------------------------------------------------------------------------

/// The certificate and key exactly as last read from disk, so a reload can
/// tell whether anything changed and so the bytes it loads are the bytes it
/// compared (a renewal landing between a compare and a second read cannot
/// slip past).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CertFiles {
    cert: Vec<u8>,
    key: Vec<u8>,
}

impl CertFiles {
    fn read(cert_path: &str, key_path: &str) -> Result<Self> {
        Ok(Self {
            cert: std::fs::read(cert_path)
                .with_context(|| format!("reading TLS certificate {cert_path}"))?,
            key: std::fs::read(key_path).with_context(|| format!("reading TLS key {key_path}"))?,
        })
    }
}

/// What one reload attempt did, for the log and for tests.
#[derive(Debug, PartialEq, Eq)]
enum Reload {
    Unchanged,
    Reloaded,
    /// The files could not be read or parsed; the previous certificate is
    /// still the one being served.
    Failed,
}

/// Re-reads the certificate and key, and swaps them in if they changed.
///
/// A failure never takes the server down or clears the certificate it is
/// serving: a renewal tool caught halfway through writing, or a key that
/// does not match its certificate, leaves the old pair in place until the
/// next attempt finds a good one.
async fn reload(
    config: &RustlsConfig,
    cert_path: &str,
    key_path: &str,
    last: &mut CertFiles,
    why: &str,
) -> Reload {
    let current = match CertFiles::read(cert_path, key_path) {
        Ok(current) => current,
        Err(err) => {
            eprintln!(
                "tls: reload ({why}) failed: {err:#}; still serving the previous certificate"
            );
            return Reload::Failed;
        }
    };
    if current == *last {
        return Reload::Unchanged;
    }
    match config
        .reload_from_pem(current.cert.clone(), current.key.clone())
        .await
    {
        Ok(()) => {
            eprintln!("tls: reloaded certificate {cert_path} ({why})");
            warn_if_key_exposed(key_path);
            *last = current;
            Reload::Reloaded
        }
        Err(err) => {
            eprintln!(
                "tls: reload ({why}) failed: {cert_path} or {key_path} changed but could not be \
                 loaded: {err}; still serving the previous certificate"
            );
            Reload::Failed
        }
    }
}

/// Reloads on SIGHUP (what a certbot deploy hook sends, via `docker kill
/// -s HUP`) and every `every` regardless, so a renewal with no hook still
/// lands in time.
///
/// The signal handler is installed here, synchronously, rather than inside
/// the task: until it exists a SIGHUP would still get its default action,
/// which ends the process.
fn spawn_reloader(
    config: RustlsConfig,
    cert_path: String,
    key_path: String,
    mut last: CertFiles,
    every: Duration,
) -> Result<JoinHandle<()>> {
    #[cfg(unix)]
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("installing the SIGHUP handler that reloads the TLS certificate")?;
    Ok(tokio::spawn(async move {
        let mut tick = tokio::time::interval_at(Instant::now() + every, every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            #[cfg(unix)]
            let why = tokio::select! {
                _ = tick.tick() => "timer",
                Some(()) = hangup.recv() => "SIGHUP",
            };
            #[cfg(not(unix))]
            let why = {
                tick.tick().await;
                "timer"
            };
            let outcome = reload(&config, &cert_path, &key_path, &mut last, why).await;
            // A timer finding nothing new is the normal case and stays
            // quiet; someone who sent a SIGHUP is waiting to hear back.
            if outcome == Reload::Unchanged && why == "SIGHUP" {
                eprintln!("tls: SIGHUP: {cert_path} and {key_path} unchanged");
            }
        }
    }))
}

/// A private key anyone else on the machine can read is a private key
/// anyone else on the machine has. Warned about, not refused: the fix is
/// one chmod, and refusing would turn a permissions slip into an outage.
#[cfg(unix)]
fn warn_if_key_exposed(key_path: &str) {
    use std::os::unix::fs::PermissionsExt;
    // metadata() follows symlinks, so certbot's live/ links are judged by
    // the archive/ file they point at, the one actually read.
    if let Ok(meta) = std::fs::metadata(key_path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "tls: warning: {key_path} is readable by users other than its owner (mode \
                 {mode:03o}); chmod 600 it, owned by the user recall-server runs as"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_key_exposed(_key_path: &str) {}

// ---------------------------------------------------------------------------
// ACME mode.
// ---------------------------------------------------------------------------

/// The acceptor that answers TLS-ALPN-01 challenges and serves every other
/// handshake with the issued certificate, and the state that issues and
/// renews it. `directory` is the ACME directory URL, a parameter so tests
/// can point it somewhere that never answers.
fn acme(
    domains: &[String],
    email: &str,
    cache_dir: &str,
    directory: &str,
) -> Result<(AxumAcceptor, AcmeState<io::Error>)> {
    prepare_cache_dir(cache_dir)?;
    let state = AcmeConfig::new(domains)
        .contact([format!("mailto:{email}")])
        .cache(DirCache::new(PathBuf::from(cache_dir)))
        .directory(directory)
        .state();
    let acceptor = state.axum_acceptor(acme_server_config(&state));
    Ok((acceptor, state))
}

/// rustls-acme's default server config offers no ALPN protocol at all, so
/// a client could never negotiate HTTP/2 even though hyper would serve it.
/// Offered here the way files mode's config (built by axum-server) already
/// does. Challenge connections are unaffected: the acceptor recognises
/// `acme-tls/1` itself and answers those with a config of its own.
fn acme_server_config<EC, EA>(state: &AcmeState<EC, EA>) -> Arc<rustls::ServerConfig>
where
    EC: std::fmt::Debug + 'static,
    EA: std::fmt::Debug + 'static,
{
    let mut config = (*state.default_rustls_config()).clone();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
}

/// The cache holds the ACME account key and the certificate's private key,
/// so it is made private to the server's own user: the directory 0700, and
/// every file in it 0600. rustls-acme writes its files with the default
/// umask (0644); the directory's mode is what actually keeps other users
/// out, and the file modes are tightened after each write as well, so a
/// copy of the directory taken elsewhere keeps them.
fn prepare_cache_dir(dir: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating ACME cache directory {dir}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making ACME cache directory {dir} private (0700)"))?;
    }
    tighten_cache_files(dir);
    Ok(())
}

fn tighten_cache_files(dir: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Err(err) =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                {
                    eprintln!("acme: could not make {} private: {err}", path.display());
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Drives certificate issuance and renewal for as long as the server runs.
///
/// A failure (the ACME directory unreachable, a rate limit, DNS not pointed
/// here yet) is logged and retried, never fatal: a certificate already
/// issued keeps serving until it expires, and with none yet every normal
/// handshake fails closed, rather than one bad renewal taking the whole
/// process down. What the log owes the owner is when that retry is.
fn spawn_acme_events(mut state: AcmeState<io::Error>, cache_dir: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut failures: u32 = 0;
        while let Some(event) = state.next().await {
            match event {
                Ok(EventOk::DeployedCachedCert) => {
                    eprintln!("acme: serving the cached certificate from {cache_dir}");
                }
                Ok(EventOk::DeployedNewCert) => {
                    failures = 0;
                    eprintln!("acme: issued a new certificate and switched to it");
                }
                Ok(stored @ (EventOk::CertCacheStore | EventOk::AccountCacheStore)) => {
                    tighten_cache_files(&cache_dir);
                    eprintln!("acme: {stored:?} in {cache_dir}");
                }
                Err(EventError::Order(err)) => {
                    failures += 1;
                    eprintln!(
                        "acme: certificate order failed (attempt {failures}): {err}; retrying in \
                         {}s. Any certificate already issued keeps serving until it expires; \
                         until one is, every handshake fails",
                        acme_retry_delay(failures).as_secs()
                    );
                }
                Err(err) => eprintln!("acme: {err}"),
            }
        }
    })
}

/// When rustls-acme (0.15) retries after the `failures`th consecutive
/// failed order: one second, doubling each time, capped at 2^16 s (about
/// 18 hours). It does not report this itself, so it is mirrored here only
/// to say it in the log; keep it in step on an upgrade.
fn acme_retry_delay(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.saturating_sub(1).min(16))
}

// ---------------------------------------------------------------------------
// Connection hardening: the cap, the handshake deadline, and the idle one.
// ---------------------------------------------------------------------------

/// Wraps the TLS acceptor of either mode with everything in [`Limits`] that
/// hyper does not do itself.
#[derive(Clone)]
struct Hardened<A> {
    tls: A,
    limits: Arc<Limits>,
    permits: Arc<Semaphore>,
    refusals: Arc<Refusals>,
}

impl<A, S> Accept<TcpStream, S> for Hardened<A>
where
    A: Accept<TcpStream, S>,
    A::Future: Send + 'static,
    A::Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    A::Service: Send + 'static,
{
    type Stream = Guarded<A::Stream>;
    type Service = Tracked<A::Service>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        // Taken before the handshake, not after: a socket that never
        // finishes one is the cheapest thing an attacker can open, so it
        // has to count.
        let Ok(permit) = self.permits.clone().try_acquire_owned() else {
            self.refusals.record(self.limits.max_connections);
            // Dropping the stream here closes it at once, freeing its
            // descriptor, rather than queueing it behind the ones already
            // open.
            drop(stream);
            return Box::pin(std::future::ready(Err(io::Error::other(
                "connection limit reached",
            ))));
        };
        let limits = self.limits.clone();
        let handshake = self.tls.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = tokio::time::timeout(limits.handshake_timeout, handshake)
                .await
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out")
                })??;
            let conn = Arc::new(ConnState::new());
            Ok((
                Guarded {
                    timer: Box::pin(tokio::time::sleep_until(conn.deadline(&limits))),
                    inner: stream,
                    conn: conn.clone(),
                    limits,
                    _permit: permit,
                },
                Tracked {
                    inner: service,
                    conn,
                },
            ))
        })
    }
}

/// Reports refused connections at most once a minute, with a count, so a
/// flood against the cap leaves a trace in the log without becoming one.
#[derive(Default)]
struct Refusals {
    since_report: AtomicU64,
    last_report: Mutex<Option<Instant>>,
}

impl Refusals {
    fn record(&self, cap: usize) {
        let refused = self.since_report.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self
            .last_report
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if last.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
            *last = Some(Instant::now());
            self.since_report.store(0, Ordering::Relaxed);
            eprintln!(
                "tls: {cap} connections open (RECALL_TLS_MAX_CONNECTIONS); refused {refused} new \
                 connection(s) since the last report"
            );
        }
    }
}

/// Whether a connection has a request in flight, and since when it has not.
///
/// Shared between the connection's I/O ([`Guarded`], which enforces the
/// deadline) and its service ([`Tracked`], which is the only thing that
/// knows when a request starts and when its response has been sent). Bytes
/// on the wire deliberately do not count as activity: HTTP/2 keep-alive
/// pings, which the server itself sends, would otherwise keep an idle
/// connection alive forever.
struct ConnState {
    activity: Mutex<Activity>,
}

struct Activity {
    in_flight: usize,
    idle_since: Instant,
    newest_request: Instant,
}

impl ConnState {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            activity: Mutex::new(Activity {
                in_flight: 0,
                idle_since: now,
                newest_request: now,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn begin(self: &Arc<Self>) -> Busy {
        let mut activity = self.lock();
        activity.in_flight += 1;
        activity.newest_request = Instant::now();
        Busy(self.clone())
    }

    /// When this connection should be closed if nothing changes before
    /// then.
    fn deadline(&self, limits: &Limits) -> Instant {
        let activity = self.lock();
        if activity.in_flight == 0 {
            activity.idle_since + limits.idle_timeout
        } else {
            activity.newest_request + limits.request_timeout
        }
    }
}

/// One request in flight, from its headers until its response body is
/// dropped (sent, or abandoned).
struct Busy(Arc<ConnState>);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut activity = self.0.lock();
        activity.in_flight -= 1;
        if activity.in_flight == 0 {
            activity.idle_since = Instant::now();
        }
    }
}

/// A connection's I/O, holding its slot under the cap for as long as it is
/// open and failing it once [`ConnState::deadline`] passes with the
/// connection still waiting on the client.
///
/// The deadline is checked only when a read or write would block, which is
/// exactly when a connection is waiting on its peer; it is recomputed on
/// every such check, so a request starting or finishing moves it.
struct Guarded<S> {
    inner: S,
    conn: Arc<ConnState>,
    limits: Arc<Limits>,
    timer: Pin<Box<Sleep>>,
    _permit: OwnedSemaphorePermit,
}

impl<S> Guarded<S> {
    fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<io::Error> {
        let deadline = self.conn.deadline(&self.limits);
        if self.timer.deadline() != deadline {
            self.timer.as_mut().reset(deadline);
        }
        match self.timer.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection idle past its deadline",
            )),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Guarded<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Pending => this.poll_expired(cx).map(Err),
            ready => ready,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Guarded<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Pending => this.poll_expired(cx).map(Err),
            ready => ready,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending => this.poll_expired(cx).map(Err),
            ready => ready,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The per-connection service, marking each request busy on its
/// connection's [`ConnState`] until its response body is done.
#[derive(Clone)]
struct Tracked<S> {
    inner: S,
    conn: Arc<ConnState>,
}

impl<S, R> tower_service::Service<R> for Tracked<S>
where
    S: tower_service::Service<R, Response = Response<Body>>,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Body>, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let busy = self.conn.begin();
        let response = self.inner.call(req);
        Box::pin(async move {
            let response = response.await?;
            // The response body carries the marker, not this future: the
            // request is not done until its last byte is sent, and a
            // client reading slowly is still a request in flight (bounded
            // by Limits::request_timeout).
            Ok(response.map(|body| {
                Body::new(BusyBody {
                    inner: body,
                    _busy: busy,
                })
            }))
        })
    }
}

struct BusyBody {
    inner: Body,
    _busy: Busy,
}

impl HttpBody for BusyBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    // Passed through so a fixed-size body keeps its Content-Length.
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::routing::get;
    use rustls::pki_types::{CertificateDer, ServerName};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;
    use tokio_rustls::client::TlsStream;

    /// A self-signed certificate for `localhost`, as PEM for the server
    /// and DER for a client to trust.
    struct TestCert {
        cert_pem: String,
        key_pem: String,
        der: CertificateDer<'static>,
    }

    fn test_cert() -> TestCert {
        let rcgen::CertifiedKey { cert, key_pair } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        TestCert {
            cert_pem: cert.pem(),
            key_pem: key_pair.serialize_pem(),
            der: cert.der().clone(),
        }
    }

    /// Short enough that a test waiting one out stays fast, long enough
    /// that a loaded CI machine still finishes a handshake inside them.
    fn short_limits() -> Limits {
        Limits {
            max_connections: 16,
            handshake_timeout: Duration::from_millis(500),
            header_read_timeout: Duration::from_millis(500),
            idle_timeout: Duration::from_millis(700),
            request_timeout: Duration::from_secs(10),
            h2_keep_alive_interval: Duration::from_secs(30),
            h2_keep_alive_timeout: Duration::from_secs(30),
        }
    }

    fn router() -> Router {
        Router::new().route("/", get(|| async { "ok" })).route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                "done"
            }),
        )
    }

    struct Running {
        addr: SocketAddr,
        _stop: oneshot::Sender<()>,
    }

    async fn files_prepared(cert: &TestCert) -> Prepared {
        install_ring_provider();
        let config = RustlsConfig::from_pem(
            cert.cert_pem.clone().into_bytes(),
            cert.key_pem.clone().into_bytes(),
        )
        .await
        .unwrap();
        Prepared {
            acceptor: TlsAcceptor::Files(config),
            tasks: vec![],
            description: String::new(),
        }
    }

    /// ACME mode against a directory that never answers: nothing is ever
    /// issued, and the state is not even driven, which is all a test of
    /// the connection handling around its acceptor needs.
    fn acme_prepared(cache: &std::path::Path) -> Prepared {
        install_ring_provider();
        let (acceptor, _state) = acme(
            &["recall.invalid".to_string()],
            "me@example.com",
            cache.to_str().unwrap(),
            "https://127.0.0.1:9/directory",
        )
        .unwrap();
        Prepared {
            acceptor: TlsAcceptor::Acme(acceptor),
            tasks: vec![],
            description: String::new(),
        }
    }

    async fn start(prepared: Prepared, limits: Limits) -> Running {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = listener.into_std().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(serve(router(), listener, prepared, limits, async {
            let _ = stopped.await;
        }));
        Running { addr, _stop: stop }
    }

    async fn tls_connect(
        addr: SocketAddr,
        cert: &TestCert,
        alpn: &[&[u8]],
    ) -> TlsStream<TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der.clone()).unwrap();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let tcp = TcpStream::connect(addr).await.unwrap();
        tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), tcp)
            .await
            .unwrap()
    }

    /// Reads until the server closes the connection (EOF or an error),
    /// failing the test if it is still open after `within`. Returns what
    /// was read and how long the close took.
    async fn read_until_closed<R: AsyncRead + Unpin>(
        stream: &mut R,
        within: Duration,
    ) -> (Vec<u8>, Duration) {
        let started = Instant::now();
        let mut seen = Vec::new();
        let read_all = async {
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
            }
        };
        tokio::time::timeout(within, read_all)
            .await
            .expect("the server should have closed the connection by now");
        (seen, started.elapsed())
    }

    /// The ACME-mode gap the review found: its acceptor had no handshake
    /// deadline at all, so a socket that connected and never spoke held a
    /// task and a descriptor forever.
    #[tokio::test]
    async fn a_connection_that_never_starts_a_handshake_is_closed_in_acme_mode() {
        let cache = tempfile::tempdir().unwrap();
        let server = start(acme_prepared(cache.path()), short_limits()).await;
        let mut tcp = TcpStream::connect(server.addr).await.unwrap();
        let (_, took) = read_until_closed(&mut tcp, Duration::from_secs(5)).await;
        assert!(
            took >= Duration::from_millis(400),
            "closed after {took:?}: that is not the handshake deadline"
        );
    }

    #[tokio::test]
    async fn a_connection_that_never_starts_a_handshake_is_closed_in_files_mode() {
        let cert = test_cert();
        let server = start(files_prepared(&cert).await, short_limits()).await;
        let mut tcp = TcpStream::connect(server.addr).await.unwrap();
        let (_, took) = read_until_closed(&mut tcp, Duration::from_secs(5)).await;
        assert!(took >= Duration::from_millis(400), "closed after {took:?}");
    }

    /// Silence after a completed handshake is invisible to hyper's header
    /// timeout: until the first bytes arrive, hyper does not yet know
    /// whether the client speaks HTTP/1 or HTTP/2, and has no timer
    /// running. The idle deadline is what closes it.
    #[tokio::test]
    async fn a_connection_that_sends_nothing_after_the_handshake_is_closed() {
        let cert = test_cert();
        let server = start(files_prepared(&cert).await, short_limits()).await;
        let mut tls = tls_connect(server.addr, &cert, &[b"http/1.1"]).await;
        let (_, took) = read_until_closed(&mut tls, Duration::from_secs(5)).await;
        assert!(took >= Duration::from_millis(500), "closed after {took:?}");
    }

    /// Slowloris: a request whose headers never finish. Proves hyper's own
    /// header timeout is live, which it is not unless it is given a timer;
    /// the idle deadline is pushed out of the way so it cannot be what
    /// closes the connection instead.
    #[tokio::test]
    async fn a_request_whose_headers_never_finish_is_closed() {
        let cert = test_cert();
        let limits = Limits {
            idle_timeout: Duration::from_secs(30),
            ..short_limits()
        };
        let server = start(files_prepared(&cert).await, limits).await;
        let mut tls = tls_connect(server.addr, &cert, &[b"http/1.1"]).await;
        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nX-Slow: ")
            .await
            .unwrap();
        tls.flush().await.unwrap();
        let (_, took) = read_until_closed(&mut tls, Duration::from_secs(5)).await;
        assert!(took >= Duration::from_millis(400), "closed after {took:?}");
    }

    /// An HTTP/1 connection kept alive after a response is closed once it
    /// sits idle, rather than holding its slot for as long as the client
    /// likes.
    #[tokio::test]
    async fn an_idle_keep_alive_connection_is_closed_after_its_response() {
        let cert = test_cert();
        let server = start(files_prepared(&cert).await, short_limits()).await;
        let mut tls = tls_connect(server.addr, &cert, &[b"http/1.1"]).await;
        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let (seen, _) = read_until_closed(&mut tls, Duration::from_secs(5)).await;
        let seen = String::from_utf8_lossy(&seen);
        assert!(seen.starts_with("HTTP/1.1 200"), "{seen}");
        assert!(seen.ends_with("ok"), "{seen}");
    }

    /// The same for HTTP/2, which hyper has no idle timeout for at all: a
    /// connection that opens with the preface and settings and then no
    /// stream is closed by the idle deadline. The keep-alive pings are set
    /// far out, so they are not what ends it.
    #[tokio::test]
    async fn an_http2_connection_with_no_stream_open_is_closed() {
        let cert = test_cert();
        let server = start(files_prepared(&cert).await, short_limits()).await;
        let mut tls = tls_connect(server.addr, &cert, &[b"h2"]).await;
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
        tls.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        // An empty SETTINGS frame: length 0, type 4, no flags, stream 0.
        tls.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();
        tls.flush().await.unwrap();
        let (seen, took) = read_until_closed(&mut tls, Duration::from_secs(5)).await;
        assert!(!seen.is_empty(), "the server should have sent its SETTINGS");
        assert!(took >= Duration::from_millis(500), "closed after {took:?}");
    }

    /// The idle deadline must never cut off a request that is simply slow
    /// to answer: a merge can take most of a minute. Here the handler takes
    /// twice the idle timeout and its response still arrives whole.
    #[tokio::test]
    async fn a_request_slower_than_the_idle_timeout_still_completes() {
        let cert = test_cert();
        let server = start(files_prepared(&cert).await, short_limits()).await;
        let mut tls = tls_connect(server.addr, &cert, &[b"http/1.1"]).await;
        tls.write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let (seen, _) = read_until_closed(&mut tls, Duration::from_secs(10)).await;
        let seen = String::from_utf8_lossy(&seen);
        assert!(seen.starts_with("HTTP/1.1 200"), "{seen}");
        assert!(seen.ends_with("done"), "{seen}");
    }

    /// Past the cap, a new connection is closed straight away, before any
    /// handshake; once a slot frees up, the next one is served normally.
    #[tokio::test]
    async fn connections_past_the_cap_are_closed_until_a_slot_frees() {
        let cert = test_cert();
        let limits = Limits {
            max_connections: 2,
            handshake_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(30),
            ..short_limits()
        };
        let server = start(files_prepared(&cert).await, limits).await;
        let first = TcpStream::connect(server.addr).await.unwrap();
        let _second = TcpStream::connect(server.addr).await.unwrap();
        // Let the accept loop take both before the third arrives.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut third = TcpStream::connect(server.addr).await.unwrap();
        let (_, took) = read_until_closed(&mut third, Duration::from_secs(2)).await;
        assert!(took < Duration::from_secs(1), "closed after {took:?}");

        drop(first);
        let mut served = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let tcp = TcpStream::connect(server.addr).await.unwrap();
            let mut roots = rustls::RootCertStore::empty();
            roots.add(cert.der.clone()).unwrap();
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
            if let Ok(tls) = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(ServerName::try_from("localhost").unwrap(), tcp)
                .await
            {
                served = Some(tls);
                break;
            }
        }
        let mut tls = served.expect("a slot should have freed once the first connection closed");
        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let (seen, _) = read_until_closed(&mut tls, Duration::from_secs(5)).await;
        assert!(String::from_utf8_lossy(&seen).starts_with("HTTP/1.1 200"));
    }

    /// Files mode picks up a renewed certificate without a restart, and a
    /// broken one on disk never replaces a working one in memory.
    #[tokio::test]
    async fn a_changed_certificate_is_reloaded_and_a_broken_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("fullchain.pem");
        let key_path = dir.path().join("privkey.pem");
        let (cert_path, key_path) = (cert_path.to_str().unwrap(), key_path.to_str().unwrap());
        let old = test_cert();
        std::fs::write(cert_path, &old.cert_pem).unwrap();
        std::fs::write(key_path, &old.key_pem).unwrap();

        install_ring_provider();
        let mut last = CertFiles::read(cert_path, key_path).unwrap();
        let config = RustlsConfig::from_pem(last.cert.clone(), last.key.clone())
            .await
            .unwrap();
        let prepared = Prepared {
            acceptor: TlsAcceptor::Files(config.clone()),
            tasks: vec![],
            description: String::new(),
        };
        let server = start(prepared, short_limits()).await;

        assert_eq!(
            reload(&config, cert_path, key_path, &mut last, "test").await,
            Reload::Unchanged
        );

        let new = test_cert();
        std::fs::write(cert_path, &new.cert_pem).unwrap();
        std::fs::write(key_path, &new.key_pem).unwrap();
        assert_eq!(
            reload(&config, cert_path, key_path, &mut last, "test").await,
            Reload::Reloaded
        );
        // A client trusting only the new certificate now gets through.
        tls_connect(server.addr, &new, &[b"http/1.1"]).await;

        // Half-written: a key that does not parse.
        std::fs::write(key_path, "-----BEGIN PRIVATE KEY-----\ntruncated").unwrap();
        assert_eq!(
            reload(&config, cert_path, key_path, &mut last, "test").await,
            Reload::Failed
        );
        tls_connect(server.addr, &new, &[b"http/1.1"]).await;
    }

    #[test]
    fn the_acme_server_config_offers_http2_and_http1() {
        install_ring_provider();
        let cache = tempfile::tempdir().unwrap();
        let state = AcmeConfig::new(["recall.invalid"])
            .cache(DirCache::new(cache.path().to_path_buf()))
            .directory("https://127.0.0.1:9/directory")
            .state();
        assert_eq!(
            acme_server_config(&state).alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    /// The cache holds two private keys; nobody but the server's own user
    /// should be able to read either, including a cache left behind by an
    /// earlier version with the default 0755/0644 modes.
    #[cfg(unix)]
    #[test]
    fn the_acme_cache_is_private_to_the_server_user() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("acme");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = dir.join("cached_cert_x");
        std::fs::write(&file, "key").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

        prepare_cache_dir(dir.to_str().unwrap()).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&file), 0o600);

        // And a directory that does not exist yet is created private.
        let fresh = root.path().join("fresh/acme");
        prepare_cache_dir(fresh.to_str().unwrap()).unwrap();
        assert_eq!(mode(&fresh), 0o700);
    }

    #[test]
    fn the_logged_acme_retry_delay_doubles_and_caps() {
        assert_eq!(acme_retry_delay(1), Duration::from_secs(1));
        assert_eq!(acme_retry_delay(2), Duration::from_secs(2));
        assert_eq!(acme_retry_delay(5), Duration::from_secs(16));
        assert_eq!(acme_retry_delay(17), Duration::from_secs(1 << 16));
        assert_eq!(acme_retry_delay(400), Duration::from_secs(1 << 16));
    }
}
