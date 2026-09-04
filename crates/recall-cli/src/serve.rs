//! `recall serve` — the other half of the binary.
//!
//! The one command that is allowed to refuse to start. A server reachable
//! from the internet with no token is not a degraded mode worth supporting,
//! so a missing `RECALL_TOKEN` is fatal here where it is merely reported
//! everywhere else.

use std::sync::Arc;

use recall_hooks::exit;
use recall_server::{Config, Server, Store};

/// Loads configuration, opens the database, and serves until shut down.
pub async fn run() -> anyhow::Result<i32> {
    let cfg = Config::from_env()?;
    // Opening the store before constructing the server means a bad database
    // path fails immediately with a clear error, rather than after the port
    // is already bound.
    let store = Arc::new(Store::open(&cfg.db_path)?);
    Server::new(cfg, store).serve().await?;
    Ok(exit::OK)
}
