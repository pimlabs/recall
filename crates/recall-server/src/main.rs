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
       recall-server admin <list | rename | remove | restore> ...
       recall-server reset-passkeys

With no argument it serves until stopped. RECALL_TOKEN is required.
`recall-server admin help` describes the admin commands, which change stored
memory from a shell on the host and open no listener.

reset-passkeys removes every passkey registered for /admin, and every
session they signed in, from the database at RECALL_DB_PATH, and prints a
one-time bootstrap code. It is for an owner who has lost all of them:
RECALL_TOKEN and that code can then register a first passkey again. Run it
where the server runs, as the database's owner, such as with
docker compose exec -u node.";

/// What `recall-server version` prints: the same first line as `recall
/// version`, so one reading of either tells the same story, then the
/// optional parts this binary was built with.
///
/// The second line is what lets a release be checked for what it must
/// carry: a server built without `passkeys` starts and syncs as well as
/// one built with it, and only `/admin` would show the difference. The
/// release workflow refuses a binary whose line does not name it.
fn version_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("RECALL_GIT_COMMIT").unwrap_or("unknown");
    let first = match recall_wire::discovery::channel() {
        recall_wire::discovery::CHANNEL_RELEASE => {
            format!("recall-server {version} ({commit})")
        }
        _ => format!(
            "recall-server {version} ({commit}, dev build {})",
            recall_wire::discovery::version()
        ),
    };
    let features: &[&str] = if cfg!(feature = "passkeys") {
        &["passkeys"]
    } else {
        &[]
    };
    let features = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(" ")
    };
    format!("{first}\nfeatures: {features}")
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
        // Handled before anything the server needs is read: an admin
        // command runs beside a server, never as one, so it reads no token
        // and binds nothing.
        ["admin", ..] => return recall_server::admin::main(&args[1..]),
        ["reset-passkeys"] => return recall_server::admin::reset_passkeys(),
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
        let server = Server::new(cfg, store);
        // Printed where only someone on the host reads it: with the token,
        // it registers the first passkey.
        if let Some(code) = server.issue_bootstrap_code()? {
            eprintln!("{}", code.instructions(server.public_url()));
        }
        server.serve().await
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
