//! `recall` — sync Claude Code's auto memory across machines.
//!
//! One binary, both halves: `recall serve` runs the server, everything else
//! runs on a developer machine or inside a Claude Code session as a hook.
//! This file is only the dispatcher; each command lives in its own module,
//! and the reasoning about *how loudly it is allowed to fail* lives with it.
//!
//! # Where the server starts, too
//!
//! This is the entry point for **both** halves — `serve` starts the HTTP
//! server that `recall-server` implements. If you came looking for where the
//! server process begins and expected `recall-server` to hold a `main`, this
//! is the file.
//!
//! It was called `recall-cli` until the v0.1.0 release preflight found that
//! name taken on crates.io by an unrelated project, and `recall` taken too,
//! so it shipped as `recall-sync`. The `recall` name has since been
//! transferred, and this crate took it at 0.2.0 — `0.1.0` was not available
//! to take, because crates.io never lets a version number be reused and an
//! unrelated 2019 crate holds that one under this name for good.
//!
//! `cargo install recall-sync` no longer resolves. Not because that crate
//! was withdrawn — it is still on the index — but because `recall-paths`
//! 0.1.0, which it depends on, is yanked, and 0.1.0 was that crate's only
//! version. A yanked version cannot be picked by a fresh resolution, so the
//! install fails at the dependency rather than at the name. `cargo install
//! recall` is the way in.

mod backfill;
mod hook;
mod init;
mod project;
mod promote;
mod serve;
mod status;

use std::path::PathBuf;

use clap::{CommandFactory, Parser, Subcommand};
use recall_hooks::exit;

/// Set at build time by the release workflow.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: Option<&str> = option_env!("RECALL_GIT_COMMIT");

#[derive(Parser)]
#[command(
    name = "recall",
    about = "Sync Claude Code's auto memory across machines and cloud sessions",
    // clap's own `--version` is replaced rather than merely enabled. Its
    // default prints `recall <version>` and nothing else, while `recall
    // version` prints the commit too — two ways of asking the same question
    // giving different answers is the failure this project keeps finding in
    // itself. `version_line` below is the single source both go through.
    disable_version_flag = true,
    // `recall` alone still prints help and exits 2, which it did before this
    // flag existed. Without this, an optional subcommand would make a bare
    // `recall` a silent success.
    arg_required_else_help = true
)]
struct Cli {
    /// Print the version
    #[arg(short = 'V', long = "version", action = clap::ArgAction::SetTrue)]
    version: bool,
    #[command(subcommand)]
    command: Option<Cmd>,
}

/// What every way of asking for the version prints.
///
/// `recall version`, `recall --version` and `recall -V` all call this, and a
/// test asserts the three are byte-identical. The subcommand came first and
/// cannot be dropped — it has been the CLI surface since v0.1.0 — but a tool
/// where `--version` errors out is a tool people file bugs against, so both
/// exist and neither is allowed to drift from the other.
///
/// The `recall <version>` prefix is load-bearing beyond taste:
/// `scripts/release.sh` matches on it to confirm the binary it just built is
/// the one being tagged.
fn version_line() -> String {
    format!("recall {VERSION} ({})", COMMIT.unwrap_or("unknown"))
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
    /// Move a note out of this project, into the global or machine scope
    Promote {
        /// The note to promote, relative to the memory directory
        file: PathBuf,
        /// Where it should go
        #[arg(long, value_enum, default_value_t = promote::Target::Global)]
        to: promote::Target,
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

    // Before the subcommand match, because there is no subcommand to match:
    // `arg_required_else_help` only covers the no-arguments case, so
    // `recall --version` arrives here with `command` unset.
    if cli.version {
        println!("{}", version_line());
        std::process::exit(exit::OK);
    }
    let Some(command) = cli.command else {
        // Unreachable while `arg_required_else_help` stands: clap prints help
        // and exits before returning. Spelled out rather than unwrapped so
        // that removing that attribute is a behaviour change someone reads,
        // not a panic someone hits.
        Cli::command().print_help().ok();
        std::process::exit(exit::CONFIG);
    };

    // Only `serve` is long-running and genuinely concurrent. The hook
    // commands each make one request and exit, so they get a
    // single-threaded runtime — `recall push` runs on every memory write in
    // a session, and spinning up a thread pool to make one HTTP call is
    // waste the user pays for repeatedly.
    let result = match command {
        Cmd::Version => {
            println!("{}", version_line());
            Ok(exit::OK)
        }
        Cmd::Init { path } => init::run(path.as_deref()),
        Cmd::Backfill => block_on_current(backfill::run()),
        Cmd::Serve => block_on_multi(serve::run()),
        Cmd::Promote { file, to } => block_on_current(promote::run(&file, to)),
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
