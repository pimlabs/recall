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
//! The security rule this exists to uphold lives in `config.rs`, not here:
//! by the time a [`TlsMode`] reaches [`serve`], `RECALL_TRUSTED_IP_HEADER`
//! has already been forced empty (or refused the server outright, if it was
//! set explicitly), so the client IP the rate limiter sees is always the
//! raw TCP peer address `axum-server` hands to `ConnectInfo`, the same as
//! it would be with no header configured at all. Nothing in this module
//! reads a header for that purpose.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use futures_util::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::AcmeConfig;

use crate::config::TlsMode;

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

/// Serves `router` over TLS on an already-bound listener, until `shutdown`
/// resolves.
///
/// `tls` must not be [`TlsMode::Off`]; `mod.rs` only calls this module when
/// it isn't.
pub(super) async fn serve<F>(
    router: Router,
    listener: std::net::TcpListener,
    tls: &TlsMode,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    install_ring_provider();
    let app = router.into_make_service_with_connect_info::<SocketAddr>();
    let handle = Handle::new();
    spawn_shutdown(handle.clone(), shutdown);

    match tls {
        TlsMode::Off => unreachable!("mod.rs only reaches this module when TLS is configured"),
        TlsMode::Files {
            cert_path,
            key_path,
        } => {
            let config = RustlsConfig::from_pem_file(cert_path, key_path)
                .await
                .with_context(|| {
                    format!("loading TLS certificate {cert_path} and key {key_path}")
                })?;
            axum_server::from_tcp_rustls(listener, config)?
                .handle(handle)
                .serve(app)
                .await
                .map_err(Into::into)
        }
        TlsMode::Acme {
            domains,
            email,
            cache_dir,
            staging,
        } => {
            // The named volume already exists (it holds the database); the
            // subdirectory under it, the default cache location, does not.
            std::fs::create_dir_all(cache_dir)
                .with_context(|| format!("creating ACME cache directory {cache_dir}"))?;
            let mut state = AcmeConfig::new(domains.clone())
                .contact([format!("mailto:{email}")])
                .cache(DirCache::new(PathBuf::from(cache_dir)))
                .directory_lets_encrypt(!staging)
                .state();
            // Built from the same state before it moves into the
            // background task below; this is what actually terminates
            // TLS-ALPN-01 challenges and, for every other handshake, the
            // real connection.
            let acceptor = state.axum_acceptor(state.default_rustls_config());

            // Drives certificate issuance and renewal for as long as the
            // server runs. A failure here (the ACME directory unreachable,
            // a rate limit, DNS not pointed here yet) is logged and
            // retried, never fatal: the next handshake fails closed on its
            // own rather than one bad renewal taking the whole process
            // down.
            tokio::spawn(async move {
                while let Some(event) = state.next().await {
                    match event {
                        Ok(ok) => eprintln!("acme: {ok:?}"),
                        Err(err) => eprintln!("acme: {err}"),
                    }
                }
            });

            axum_server::from_tcp(listener)?
                .acceptor(acceptor)
                .handle(handle)
                .serve(app)
                .await
                .map_err(Into::into)
        }
    }
}
