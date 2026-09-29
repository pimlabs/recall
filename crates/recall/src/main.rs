//! `recall` — sync Claude Code's auto memory across machines.
//!
//! The client: it runs on a developer machine or inside a Claude Code
//! session as a hook. This file is the dispatcher and the help; each command
//! lives in its own module, and the reasoning about *how loudly it is allowed
//! to fail* lives with it.
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
mod edit;
mod eval;
mod hook;
mod init;
mod project;
mod promote;
mod review;
mod status;
mod ui;

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::builder::styling::{AnsiColor, Style, Styles};
use clap::builder::StyledStr;
use clap::{Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use recall_hooks::exit;

/// Set at build time by the release workflow.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COMMIT: Option<&str> = option_env!("RECALL_GIT_COMMIT");

// The help's styling, the version line above every help, the grouped command
// list and the footer are not attributes here: they are added by `cli()`
// below, because the version line is not a constant and clap has no way to
// hand a `before_help` down to every command.
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
///
/// Every help prints it too, as its first line (see [`cli`]): the version is
/// the first thing a bug report needs, and help is where people look.
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

// Each command's `///` is its help. The first line is the one-line summary
// the command lists and `-h` show: imperative, at most 60 characters, and
// no full stop. A paragraph after it is what `--help` and `recall help
// <command>` add. clap is built without its `wrap_help` feature, so that
// paragraph is wrapped by hand, and `verbatim_doc_comment` keeps the line
// breaks. Tests below hold both the length and the 100-column width.
#[derive(Subcommand)]
enum Cmd {
    /// Wire this project's .claude/settings.json for sync
    ///
    /// Adds the push and pull hooks to the project's own settings file.
    /// Commit it, so fresh clones and cloud sessions sync too.
    #[command(verbatim_doc_comment)]
    Init {
        /// Project to wire; defaults to the git root of the working directory
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Send the memory the server does not have yet
    ///
    /// The first sync, for a project whose memory predates Recall. A file
    /// the server holds a different version of is held back, not
    /// overwritten.
    #[command(verbatim_doc_comment)]
    Backfill,
    /// Move a note into the global or machine scope
    Promote {
        /// The note to promote, relative to the memory directory
        file: PathBuf,
        /// Where it should go
        #[arg(long, value_enum, default_value_t = promote::Target::Global)]
        to: promote::Target,
    },
    /// Set this machine up to sync
    ///
    /// Enrols it with the server (or saves its token), names it, wires the
    /// project you are in, and offers its first sync. A step with nothing
    /// to do is skipped, so running it again only does what is missing.
    #[command(verbatim_doc_comment)]
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
    /// List, approve and revoke enrolled machines
    ///
    /// Each needs an admin device or the server's RECALL_TOKEN.
    #[command(subcommand)]
    Devices(devices::Cmd),
    /// Make, list and revoke authkeys for cloud sessions
    ///
    /// A cloud session enrols itself with an authkey. Each of these needs
    /// an admin device or the server's RECALL_TOKEN.
    #[command(subcommand, verbatim_doc_comment)]
    Authkey(devices::KeyCmd),
    /// Export and verify the server's audit log
    ///
    /// Holds the server to its history: export its whole audit log, verify an
    /// export offline, or check the log still extends the checkpoints saved here.
    #[command(subcommand, verbatim_doc_comment)]
    Audit(audit::Cmd),
    /// Ask the server's worker for a report on memory
    ///
    /// Ask for a report on what memory holds, read the reports, and make a
    /// finding's suggested edit.
    #[command(subcommand, verbatim_doc_comment)]
    Eval(eval::Cmd),
    /// Check which claims in memory still hold
    ///
    /// Checks what memory claims against what this checkout and its git
    /// history can see, and makes a stale claim's suggested edit when asked.
    #[command(subcommand, verbatim_doc_comment)]
    Review(review::Cmd),
    /// Remove a saved token, and this machine's device key
    Disconnect {
        /// The server; defaults to the one most recently connected
        url: Option<String>,
    },
    /// Send memory changed here, and fetch the rest, mid-session
    ///
    /// The session-start pull, run by hand: first it sends what changed here
    /// since the last sync and no hook saw (a note edited, created or removed
    /// through the shell), then it fetches what other machines sent. Run it
    /// after changing memory outside Claude Code's Edit and Write, or to pick
    /// up another machine's notes without starting a new session.
    #[command(verbatim_doc_comment)]
    Sync,
    /// Show whether sync is configured and reachable here
    ///
    /// Reports what the hooks would see in this project, and never fails
    /// over what it finds: recall doctor is the one with the exit code.
    #[command(verbatim_doc_comment)]
    Status {
        /// Machine-readable output, for scripts and CI
        #[arg(long)]
        json: bool,
    },
    /// Check everything sync needs, and say what is broken
    ///
    /// Exits non-zero when any of it is broken, so a script or a CI job can
    /// stop on it. recall status only reports.
    #[command(verbatim_doc_comment)]
    Doctor {
        /// Machine-readable output, for scripts and CI
        #[arg(long)]
        json: bool,
    },
    // Moved to its own binary, `recall-server`, in 0.4.0. Kept, hidden, so
    // that typing it explains where the server went instead of failing with
    // "unrecognized subcommand"; the `///` is what its own help says.
    /// Moved: run recall-server instead, since 0.4.0
    #[command(hide = true)]
    Serve,
    /// Send a memory edit to the server
    ///
    /// Claude Code runs this after it edits or writes a file, through the
    /// PostToolUse hook recall init wires. It reads the hook's payload on
    /// standard input; there is no need to type it.
    #[command(verbatim_doc_comment)]
    Push,
    /// Fetch this project's memory from the server
    ///
    /// Claude Code runs this when a session starts, through the SessionStart
    /// hook recall init wires. An unreachable server is a warning, never a
    /// failed session.
    #[command(verbatim_doc_comment)]
    Pull,
    /// Print the version
    Version,
}

/// The top-level help's command list, by what each command is for.
///
/// Only names live here. Each line's text is the command's own `about`, read
/// back from clap, so a summary is written once, on its [`Cmd`] variant. A
/// command in no group is listed under "Other" rather than left out, so
/// forgetting to add one here costs it a heading, never its place in the
/// help. The hidden `serve` stays out through clap's own `hide`.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "Get started",
        &["connect", "init", "backfill", "disconnect"],
    ),
    ("Every day", &["status", "doctor", "sync", "promote"]),
    ("Memory quality", &["review", "eval"]),
    ("Your server", &["devices", "authkey", "audit"]),
    ("Run by Claude Code (hooks)", &["push", "pull"]),
];

/// Help in the CLI's visual language (`ui.rs`): headings bold, and what is
/// typed, commands and flags, in cyan, the colour `ui` gives a fix to run.
/// clap drops the colour by itself when output is not a terminal or
/// `NO_COLOR` is set, as `anstream` does for `ui`.
const STYLES: Styles = Styles::styled()
    .header(Style::new().bold())
    .usage(Style::new().bold())
    .literal(AnsiColor::Cyan.on_default());

/// The command line clap parses: [`Cli`]'s, with [`version_line`] above every
/// help it prints, the top-level commands in [`GROUPS`], and a footer on
/// where to start.
fn cli() -> Command {
    let mut cmd = Cli::command().styles(STYLES);
    // Built before it is changed, so the tree is whole: clap adds its own
    // `help` commands while building, and those get the version line and a
    // place in the list like the rest. Parsing would build it anyway, more
    // lazily; the one difference that shows is that `recall help help` now
    // lists the commands it can be asked about.
    cmd.build();
    let bold = Style::new().bold();
    let version = StyledStr::from(format!("{bold}{}{bold:#}", version_line()));
    let cmd = with_version(cmd, &version);
    let template = help_template(&cmd);
    cmd.help_template(template).after_help(footer())
}

/// `cmd` and every command beneath it, with `line` above its help.
///
/// clap passes styles down the tree but not a `before_help`, so the walk is
/// done here, once, before parsing.
fn with_version(cmd: Command, line: &StyledStr) -> Command {
    cmd.before_help(line.clone())
        .mut_subcommands(|sub| with_version(sub, line))
}

/// The top-level help: clap's own layout, with its one "Commands" list
/// replaced by the groups in [`GROUPS`], and "Other" after them.
fn help_template(cmd: &Command) -> StyledStr {
    let styles = cmd.get_styles();
    let (header, literal) = (styles.get_header(), styles.get_literal());
    let visible: Vec<&Command> = cmd.get_subcommands().filter(|c| !c.is_hide_set()).collect();
    let width = visible
        .iter()
        .map(|c| c.get_name().len())
        .max()
        .unwrap_or(0);

    let grouped = |name: &str| GROUPS.iter().any(|(_, names)| names.contains(&name));
    let mut groups: Vec<(&str, Vec<&Command>)> = GROUPS
        .iter()
        .map(|(heading, names)| {
            let members = names
                .iter()
                .filter_map(|n| visible.iter().copied().find(|c| c.get_name() == *n))
                .collect();
            (*heading, members)
        })
        .collect();
    let other = visible
        .iter()
        .copied()
        .filter(|c| !grouped(c.get_name()))
        .collect();
    groups.push(("Other", other));

    // Literal text in a template is printed as it is; only the `{…}` names
    // are clap's. None of the summaries has a brace in it.
    let mut out = StyledStr::new();
    out.push_str("{before-help}{about-with-newline}\n{usage-heading} {usage}\n");
    for (heading, commands) in groups.iter().filter(|(_, c)| !c.is_empty()) {
        let _ = write!(out, "\n{header}{heading}:{header:#}\n");
        for c in commands {
            let name = c.get_name();
            let about = c.get_about().map(ToString::to_string).unwrap_or_default();
            let pad = width - name.len();
            let _ = writeln!(out, "  {literal}{name}{literal:#}{:pad$}  {about}", "");
        }
    }
    let _ = write!(
        out,
        "\n{header}Options:{header:#}\n{{options}}{{after-help}}"
    );
    out
}

/// Where to start, under the top-level help.
fn footer() -> StyledStr {
    let literal = STYLES.get_literal();
    StyledStr::from(format!(
        "New here? Run '{literal}recall connect https://your-recall-host{literal:#}', \
         then '{literal}recall doctor{literal:#}'.\n\
         See '{literal}recall help <command>{literal:#}' for more on a command."
    ))
}

fn main() {
    // What `Cli::parse()` does, with [`cli`]'s command in place of the bare
    // derived one, so that every help clap prints on the way is the finished
    // one.
    let mut matches = cli().get_matches();
    let args =
        Cli::from_arg_matches_mut(&mut matches).unwrap_or_else(|e| e.format(&mut cli()).exit());

    // Before the subcommand match, because there is no subcommand to match:
    // `arg_required_else_help` only covers the no-arguments case, so
    // `recall --version` arrives here with `command` unset.
    if args.version {
        println!("{}", version_line());
        std::process::exit(exit::OK);
    }
    let Some(command) = args.command else {
        // Unreachable while `arg_required_else_help` stands: clap prints help
        // and exits before returning. Spelled out rather than unwrapped so
        // that removing that attribute is a behaviour change someone reads,
        // not a panic someone hits.
        cli().print_help().ok();
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
        Cmd::Eval(cmd) => block_on_current(eval::run(cmd)),
        Cmd::Review(cmd) => block_on_current(review::run(cmd)),
        Cmd::Status { json } => block_on_current(status::run(json)),
        Cmd::Doctor { json } => block_on_current(doctor::run(json)),
        Cmd::Push => block_on_current(hook::push()),
        Cmd::Pull => block_on_current(hook::pull()),
        Cmd::Sync => block_on_current(hook::sync()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command a help can be asked about, as the words that name it:
    /// empty for `recall` itself, `["devices", "approve"]` for one beneath.
    ///
    /// clap's own `help` commands are left out below the top: `recall help
    /// devices` is asked through `recall help`, not of a command of its own.
    fn paths() -> Vec<Vec<String>> {
        fn walk(cmd: &Command, path: Vec<String>, out: &mut Vec<Vec<String>>) {
            out.push(path.clone());
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let mut next = path.clone();
                next.push(sub.get_name().to_string());
                walk(sub, next, out);
            }
        }
        let mut out = Vec::new();
        walk(&cli(), Vec::new(), &mut out);
        out
    }

    /// What the binary would print for `recall <args>`, without colour, and
    /// the error kind clap stopped with.
    fn render(args: &[&str]) -> (clap::error::ErrorKind, String) {
        let argv = std::iter::once("recall").chain(args.iter().copied());
        match cli().try_get_matches_from(argv) {
            Ok(_) => panic!("recall {args:?} parsed instead of printing help"),
            Err(e) => (e.kind(), e.render().to_string()),
        }
    }

    /// Every way of asking for every help: `-h`, `--help`, `help <command>`,
    /// and for a command with commands beneath it, nothing at all.
    fn every_help() -> Vec<(Vec<String>, String)> {
        let mut out = Vec::new();
        for path in paths() {
            let words: Vec<&str> = path.iter().map(String::as_str).collect();
            let then = |flag| words.iter().copied().chain([flag]).collect::<Vec<_>>();
            let mut forms = vec![
                then("-h"),
                then("--help"),
                ["help"].into_iter().chain(words.iter().copied()).collect(),
            ];
            if cli_at(&words).has_subcommands() {
                forms.push(words.clone());
            }
            for form in forms {
                let (_, text) = render(&form);
                out.push((form.iter().map(|s| s.to_string()).collect(), text));
            }
        }
        out
    }

    /// The command `words` names.
    fn cli_at(words: &[&str]) -> Command {
        let mut cmd = cli();
        for word in words {
            cmd = cmd.find_subcommand(word).expect("a real command").clone();
        }
        cmd
    }

    /// The version comes first in every help, and it is [`version_line`]'s,
    /// not a second formatting of it that could drift.
    #[test]
    fn every_help_starts_with_the_version_line() {
        for (form, text) in every_help() {
            assert_eq!(
                text.lines().next(),
                Some(version_line().as_str()),
                "recall {form:?}:\n{text}"
            );
        }
    }

    /// clap is built without wrapping, so a long line in a help is printed
    /// as it is written. This is what keeps them readable in a terminal.
    #[test]
    fn no_help_line_is_wider_than_100_columns() {
        for (form, text) in every_help() {
            for line in text.lines() {
                assert!(
                    line.chars().count() <= 100,
                    "recall {form:?} has a line of {} columns:\n{line}",
                    line.chars().count()
                );
            }
        }
    }

    /// The one line each command is listed with: short enough to read in a
    /// list, one line, and a summary rather than a sentence.
    #[test]
    fn every_summary_is_one_short_line() {
        fn walk(cmd: &Command, name: String) {
            for sub in cmd.get_subcommands() {
                let name = format!("{name} {}", sub.get_name());
                let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
                assert!(!about.is_empty(), "{name} has no summary");
                assert!(!about.contains('\n'), "{name}'s summary is two lines");
                assert!(
                    !about.ends_with('.'),
                    "{name}'s summary ends in a full stop"
                );
                assert!(
                    about.chars().count() <= 60,
                    "{name}'s summary is {} characters: {about}",
                    about.chars().count()
                );
                walk(sub, name);
            }
        }
        walk(&cli(), "recall".to_string());
    }

    /// A name in [`GROUPS`] that is not a command would leave a heading
    /// short of the command it meant, quietly.
    #[test]
    fn every_grouped_name_is_a_visible_command() {
        let cmd = cli();
        for (heading, names) in GROUPS {
            for name in *names {
                let sub = cmd.find_subcommand(name);
                assert!(
                    sub.is_some_and(|s| !s.is_hide_set()),
                    "{heading} names {name}, which is not a visible command"
                );
            }
        }
    }

    /// Every command the top level offers is listed exactly once, under a
    /// heading, and the hidden one is not.
    #[test]
    fn the_top_level_help_lists_every_command_once() {
        let (_, text) = render(&["--help"]);
        let cmd = cli();
        for sub in cmd.get_subcommands() {
            let name = sub.get_name();
            let listed = text
                .lines()
                .filter(|l| l.split_whitespace().next() == Some(name) && l.starts_with("  "))
                .count();
            let want = usize::from(!sub.is_hide_set());
            assert_eq!(listed, want, "{name} is listed {listed} times:\n{text}");
        }
        for (heading, _) in GROUPS {
            assert!(
                text.contains(&format!("\n{heading}:\n")),
                "no {heading}:\n{text}"
            );
        }
    }

    /// A command added without a place in [`GROUPS`] still appears, under
    /// "Other", rather than vanishing from the help.
    #[test]
    fn a_command_in_no_group_is_listed_under_other() {
        let cmd = Command::new("recall")
            .subcommand(Command::new("status").about("Show whether sync is set up"))
            .subcommand(Command::new("brand-new").about("Do something new"));
        let template = help_template(&cmd).to_string();
        let other = template.split("Other:").nth(1).expect("an Other heading");
        assert!(
            other.contains("  brand-new  Do something new"),
            "{template}"
        );
        assert!(!other.contains("status"), "{template}");
    }

    /// `recall` alone is a question: it answers with the help, as an error,
    /// so that it exits non-zero.
    #[test]
    fn a_bare_recall_prints_the_help_as_an_error() {
        let (kind, text) = render(&[]);
        assert_eq!(
            kind,
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(text, render(&["-h"]).1);
    }
}
