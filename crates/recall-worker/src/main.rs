//! `recall-worker`: drains a Recall server's merge queue.
//!
//! Configured by the environment, as `recall-server` is; see
//! [`recall_worker::config::Config`]. It opens no port. On first start it
//! makes a key in its data directory, enrols, and prints a code for the
//! owner to approve with the `worker` scope; from then on it claims jobs.

use std::process::ExitCode;

use recall_worker::config::Config;
use recall_worker::worker::Worker;

const USAGE: &str = "\
Recall's merge worker. Configured by environment variables; see
https://github.com/pimlabs/recall/blob/main/deploy/README.md

Usage: recall-worker [version | --version | -V | help | --help | -h]

With no argument it enrols (the first time) and then works until stopped.
RECALL_URL is required.";

fn version_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = option_env!("RECALL_GIT_COMMIT").unwrap_or("unknown");
    match recall_wire::discovery::channel() {
        recall_wire::discovery::CHANNEL_RELEASE => {
            format!("recall-worker {version} ({commit})")
        }
        _ => format!(
            "recall-worker {version} ({commit}, dev build {})",
            recall_wire::discovery::version()
        ),
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
        _ => {
            eprintln!(
                "recall-worker: unexpected arguments: {}\n\n{USAGE}",
                args.join(" ")
            );
            return ExitCode::from(2);
        }
    }

    let run = async {
        let cfg = Config::from_env().map_err(|e| e.to_string())?;
        let worker = Worker::new(cfg).map_err(|e| e.to_string())?;
        worker
            .run(shutdown_signal())
            .await
            .map_err(|e| e.to_string())
    };
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
        .and_then(|rt| rt.block_on(run));
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("recall-worker: {err}");
            ExitCode::FAILURE
        }
    }
}

/// SIGTERM, which `docker compose stop` sends, or ctrl-c. A merge cut off
/// here is not lost: its lease runs out and the job is claimed again.
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
