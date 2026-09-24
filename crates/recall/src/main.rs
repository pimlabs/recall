//! `recall` — sync Claude Code's auto memory across machines.
//!
//! The client: it runs on a developer machine or inside a Claude Code
//! session as a hook. This file is only the dispatcher; each command lives
//! in its own module, and the reasoning about *how loudly it is allowed to
//! fail* lives with it.
//!
//! The server is not in here. Until 0.4.0 `recall serve` ran it from this
//! binary; it is now `recall-server`, a binary of its own in the
//! `recall-server` crate, and this crate does not depend on it. See Part 4
//! of `docs/design/handshake.md`.
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

mod audit;
mod backfill;
mod connect;
mod devices;
mod doctor;
mod hook;
mod init;
mod project;
mod promote;
mod status;
mod ui;

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
///
/// A build that is not a release says so after the commit, and gives the
/// version it reports to servers, which is a pre-release of the next patch.
fn version_line() -> String {
    let commit = COMMIT.unwrap_or("unknown");
    match recall_wire::discovery::channel() {
        recall_wire::discovery::CHANNEL_RELEASE => format!("recall {VERSION} ({commit})"),
        _ => format!(
            "recall {VERSION} ({commit}, dev build {})",
            recall_wire::discovery::version()
        ),
    }
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
    /// Set this machine up: enrol it with the server (or save its token),
    /// name it, and wire the project you are in
    Connect {
        /// The server's URL, including https://; defaults to the saved one
        url: Option<String>,
        /// This machine's name, instead of being asked
        #[arg(long)]
        name: Option<String>,
        /// Answer yes to every question, and take the suggested name
        #[arg(long, short)]
        yes: bool,
    },
    /// List, approve and revoke the machines enrolled on the server
    #[command(subcommand)]
    Devices(devices::Cmd),
    /// Make, list and revoke authkeys, with which cloud sessions enrol
    /// themselves
    #[command(subcommand)]
    Authkey(devices::KeyCmd),
    /// Hold the server to its history: export its audit log, verify an
    /// export offline, or check the log still extends what was saved here
    #[command(subcommand)]
    Audit(audit::Cmd),
    /// Remove a saved token, and this machine's device key
    Disconnect {
        /// The server; defaults to the one most recently connected
        url: Option<String>,
    },
    /// Show whether sync is configured and reachable, here
    Status {
        /// Machine-readable output, for scripts and CI
        #[arg(long)]
        json: bool,
    },
    /// Check everything sync needs, and exit non-zero if any of it is broken
    Doctor {
        /// Machine-readable output, for scripts and CI
        #[arg(long)]
        json: bool,
    },
    /// Moved to its own binary, `recall-server`, in 0.4.0. Kept hidden so
    /// that typing it explains where the server went instead of failing
    /// with "unrecognized subcommand".
    #[command(hide = true)]
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

    // The commands each make a few requests and exit, so they get a
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
        Cmd::Serve => {
            eprintln!(
                "recall: the server is its own binary since 0.4.0. Run recall-server \
                 instead, with the same environment. See \
                 https://github.com/pimlabs/recall/blob/main/docs/reference/install.md"
            );
            Ok(exit::CONFIG)
        }
        Cmd::Promote { file, to } => block_on_current(promote::run(&file, to)),
        Cmd::Connect { url, name, yes } => {
            block_on_current(connect::connect(connect::Args { url, name, yes }))
        }
        Cmd::Disconnect { url } => connect::disconnect(url.as_deref()),
        Cmd::Devices(cmd) => block_on_current(devices::run(cmd)),
        Cmd::Authkey(cmd) => block_on_current(devices::run_authkey(cmd)),
        Cmd::Audit(cmd) => block_on_current(audit::run(cmd)),
        Cmd::Status { json } => block_on_current(status::run(json)),
        Cmd::Doctor { json } => block_on_current(doctor::run(json)),
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
