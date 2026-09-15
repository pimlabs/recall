//! `recall` — sync Claude Code's auto memory across machines.
//!
//! One binary, both halves: `recall serve` runs the server, everything else
//! runs on a developer machine or inside a Claude Code session as a hook.
//! This file is only the dispatcher; each command lives in its own module,
//! and the reasoning about *how loudly it is allowed to fail* lives with it.
//!
//! # The name is narrower than the crate
//!
//! This crate is named for the client, but it is the entry point for
//! **both** halves — `serve` starts the HTTP server that `recall-server`
//! implements. If you came looking for where the server process begins and
//! expected `recall-server` to hold a `main`, this is the file.
//!
//! It was called `recall-cli` until the release preflight found that name
//! already taken on crates.io by an unrelated project, and `recall` taken
//! too. The name is published as of v0.1.0, so it is now fixed: renaming
//! would strand everyone who installed with `cargo install recall-sync`.
//! The binary it produces is still `recall`.

mod backfill;
mod hook;
mod init;
mod project;
mod promote;
mod serve;
mod status;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use recall_hooks::exit;

/// Set at build time by the release workflow.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: Option<&str> = option_env!("RECALL_GIT_COMMIT");

#[derive(Parser)]
#[command(
    name = "recall",
    about = "Sync Claude Code's auto memory across machines and cloud sessions",
    disable_version_flag = true
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wire the current project's .claude/settings.json for sync
    Init {
        /// Project to wire; defaults to the git root of the working directory
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Send memory files the server does not have yet — the first sync
    Backfill,
    /// Move a note into the global scope, so it follows you into every project
    Promote {
        /// The note to promote, relative to the memory directory
        file: PathBuf,
    },
    /// Show whether sync is configured and reachable, here
    Status {
        /// Machine-readable output, for scripts and CI
        #[arg(long)]
        json: bool,
    },
    /// Run the sync server
    Serve,
    /// Hook entry point — called by PostToolUse
    Push,
    /// Hook entry point — called by SessionStart
    Pull,
    /// Print the version
    Version,
}

fn main() {
    let cli = Cli::parse();

    // Only `serve` is long-running and genuinely concurrent. The hook
    // commands each make one request and exit, so they get a
    // single-threaded runtime — `recall push` runs on every memory write in
    // a session, and spinning up a thread pool to make one HTTP call is
    // waste the user pays for repeatedly.
    let result = match cli.command {
        Cmd::Version => {
            println!("recall {VERSION} ({})", COMMIT.unwrap_or("unknown"));
            Ok(exit::OK)
        }
        Cmd::Init { path } => init::run(path.as_deref()),
        Cmd::Backfill => block_on_current(backfill::run()),
        Cmd::Serve => block_on_multi(serve::run()),
        Cmd::Promote { file } => block_on_current(promote::run(&file)),
        Cmd::Status { json } => block_on_current(status::run(json)),
        Cmd::Push => block_on_current(hook::push()),
        Cmd::Pull => block_on_current(hook::pull()),
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("recall: {err:#}");
            std::process::exit(exit::CONFIG)
        }
    }
}

fn block_on_current<F: std::future::Future<Output = anyhow::Result<i32>>>(
    fut: F,
) -> anyhow::Result<i32> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}

fn block_on_multi<F: std::future::Future<Output = anyhow::Result<i32>>>(
    fut: F,
) -> anyhow::Result<i32> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(fut)
}
