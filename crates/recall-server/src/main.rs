//! `recall-server`: the sync server, as its own binary.
//!
//! Until 0.4.0 this was `recall serve`, a subcommand of the client, so
//! every laptop and cloud session carried a server it never ran (1.4 MB of
//! a 4.5 MB binary, and SQLite compiled from C). The split is Part 4 of
//! `docs/design/handshake.md`. Both binaries come from one workspace and
//! one version, so the discovery document still compares like with like.
//!
//! Configuration is the environment and nothing else, as it was: see
//! [`recall_server::Config`]. The server is the one program allowed to
//! refuse to start. One reachable from the internet with no token is not a
//! degraded mode worth supporting, so a missing `RECALL_TOKEN` is fatal.

use std::process::ExitCode;
use std::sync::Arc;

use recall_server::{Config, Server, Store};

const USAGE: &str = "\
Recall's sync server. Configured by environment variables; see
https://github.com/pimlabs/recall/blob/main/docs/reference/install.md

Usage: recall-server [version | --version | -V | help | --help | -h]
       recall-server reset-passkeys

With no argument it serves until stopped. RECALL_TOKEN is required.

reset-passkeys removes every passkey registered for /admin, and every
session they signed in, from the database at RECALL_DB_PATH. It is for an
owner who has lost all of them; RECALL_TOKEN can then register a first
passkey again. Run it where the server runs, such as with
docker compose exec.";

/// What `recall-server version` prints: the same shape as `recall
/// version`, so one reading of either tells the same story.
fn version_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("RECALL_GIT_COMMIT").unwrap_or("unknown");
    match recall_wire::discovery::channel() {
        recall_wire::discovery::CHANNEL_RELEASE => {
            format!("recall-server {version} ({commit})")
        }
        _ => format!(
            "recall-server {version} ({commit}, dev build {})",
            recall_wire::discovery::version()
        ),
    }
}

/// Removes every admin passkey, so the bootstrap is open again.
///
/// A command rather than a route on purpose: the one way back in after the
/// last passkey is lost must not be something `RECALL_TOKEN` can do over
/// the network, or a leaked token could replace the owner's passkeys. This
/// needs a shell where the database is, which is more than the token.
fn reset_passkeys() -> ExitCode {
    let db = std::env::var("RECALL_DB_PATH")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "data/recall.db".to_string());
    if !std::path::Path::new(&db).exists() {
        eprintln!("recall-server: no database at {db}; set RECALL_DB_PATH");
        return ExitCode::FAILURE;
    }
    match Store::open(&db).and_then(|store| store.reset_admin_credentials()) {
        Ok(n) => {
            println!(
                "Removed {n} passkey(s) and their sessions from {db}. \
                 Open /admin and register a new one with RECALL_TOKEN."
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("recall-server: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => {}
        ["version" | "--version" | "-V"] => {
            println!("{}", version_line());
            return ExitCode::SUCCESS;
        }
        ["help" | "--help" | "-h"] => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        ["reset-passkeys"] => return reset_passkeys(),
        _ => {
            eprintln!(
                "recall-server: unexpected arguments: {}\n\n{USAGE}",
                args.join(" ")
            );
            return ExitCode::from(2);
        }
    }

    let run = async {
        let cfg = Config::from_env()?;
        // Opening the store before binding means a bad database path fails
        // at once with a clear error, not after the port is taken.
        let store = Arc::new(Store::open(&cfg.db_path)?);
        Server::new(cfg, store).serve().await
    };
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(run));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("recall-server: {err:#}");
            ExitCode::FAILURE
        }
    }
}
