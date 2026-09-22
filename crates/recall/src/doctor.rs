//! `recall doctor` — the command that answers "why isn't this working",
//! and is allowed to say so with an exit code.
//!
//! `recall status` reports. This judges. The distinction earned itself: in
//! this repository's own cloud environment, the hooks were wired and the
//! binary installed, but `RECALL_URL` and `RECALL_TOKEN` were never set on
//! the environment. `recall pull` ran at every session start for a full day,
//! synced nothing, and exited `0` every time — correctly, because a hook
//! that fails a session is worse than one that does nothing. `recall status`
//! printed `(unset)` and also exited `0`. So nothing anywhere — no exit
//! code, no log line, no failing check — distinguished "never configured"
//! from "nothing new to sync", and a day's work went unsynced unnoticed.
//!
//! That is the gap this fills, and it is why the exit code is the feature
//! rather than the formatting. `status` keeps exiting `0`; it is
//! informational and scripts read its `--json`. Doctor carries the bad news.
//!
//! Every finding below is a reading of [`status::Report`], never a second
//! collection. A question worth asking is worth both commands knowing about.

use recall_hooks::config::Source;
use recall_hooks::{exit, ClientConfig};

use crate::project as proj;
use crate::status::{self, Report};

/// How much a finding matters, which is also the exit code's input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Working.
    Ok,
    /// Recall runs, but something is not doing what it looks like it does.
    /// Never fails the command: a laptop with no machine scope is not
    /// broken, and a doctor that exits non-zero on taste would stop being
    /// read.
    Warn,
    /// Memory is not syncing, or is syncing somewhere you did not ask for.
    Fail,
}

impl Level {
    fn mark(self) -> &'static str {
        match self {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

/// One answered question.
#[derive(Debug, serde::Serialize)]
pub struct Finding {
    /// How much it matters.
    pub level: Level,
    /// The short name of what was checked, e.g. `RECALL_URL`.
    pub check: &'static str,
    /// What was found.
    pub detail: String,
    /// What to do about it. Present on anything that is not `Ok`, because a
    /// finding a reader cannot act on is a finding that trains them to skip
    /// the output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn ok(check: &'static str, detail: impl Into<String>) -> Finding {
    Finding {
        level: Level::Ok,
        check,
        detail: detail.into(),
        fix: None,
    }
}

fn warn(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding {
        level: Level::Warn,
        check,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

fn fail(check: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Finding {
    Finding {
        level: Level::Fail,
        check,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

/// Where a variable has to be set, which differs by environment in a way
/// that is not guessable and cost this project a day.
const WHERE_TO_SET: &str =
    "cloud environment: the \"Add/Edit cloud environment\" dialog; laptop: recall connect <url>";

/// Reads the report into findings, most fundamental first.
///
/// Ordered so the first `FAIL` is the one worth fixing first: there is no
/// point reporting an unreachable server above an unset URL, because the
/// second explains the first.
pub(crate) fn findings(rep: &Report) -> Vec<Finding> {
    let mut out = Vec::new();

    // ---- can it reach a server at all
    if rep.url_set {
        out.push(ok("RECALL_URL", source_detail(rep, rep.url_source)));
    } else {
        out.push(fail("RECALL_URL", "not set anywhere", WHERE_TO_SET));
    }

    if rep.token_set {
        out.push(ok("RECALL_TOKEN", source_detail(rep, rep.token_source)));
    } else {
        out.push(fail("RECALL_TOKEN", "not set anywhere", WHERE_TO_SET));
    }

    credentials_findings(rep, &mut out);

    // Only asked once there is somewhere to ask. Reporting "unreachable"
    // when no URL is set would be true and useless.
    if rep.url_set {
        match (&rep.server_error, &rep.git_commit) {
            (Some(err), _) => out.push(fail(
                "server",
                format!("unreachable — {err}"),
                "check RECALL_URL, and that this environment is allowed to reach it \
                 (a cloud environment needs the domain under Allowed domains)",
            )),
            (None, Some(commit)) => out.push(ok("server", format!("answered, commit {commit}"))),
            (None, None) => out.push(ok("server", "answered")),
        }
    }

    if rep.server_ok && !rep.merge_ready {
        out.push(warn(
            "merge",
            "server is up but not logged in to the Claude CLI, so conflicting \
             edits fall back to last-write-wins",
            "on the server: docker compose exec -it -u node recall-server claude setup-token",
        ));
    }

    // ---- can Claude Code see the memory at all
    //
    // Unwired hooks are a problem in a project and nothing at all outside
    // one. Checking your connection from a home directory is an ordinary
    // thing to do, and the first version of this failed for it — which is
    // how a command teaches people to stop reading its output.
    match (rep.hooks_wired, rep.in_git_repo) {
        (true, _) => out.push(ok("hooks", "wired in .claude/settings.json")),
        (false, true) => out.push(fail(
            "hooks",
            "this project's .claude/settings.json has no Recall hooks",
            "recall init",
        )),
        (false, false) => out.push(ok(
            "hooks",
            "not in a git repository — nothing here to wire",
        )),
    }

    // `CLAUDE_CODE_REMOTE` answers the question that made this a permanent
    // warning in both directions: unset is correct on a laptop and means
    // Claude Code's auto-memory is off entirely in a remote session, where
    // Recall then has nothing to sync no matter what else is right.
    match (rep.remote_session, rep.remote_memory_dir_set) {
        (true, false) => out.push(fail(
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "not set in a remote session, so Claude Code's auto-memory is off \
             entirely and there is nothing for Recall to sync",
            // The one value in the whole setup that cannot be reasoned out.
            "set it on this cloud environment, and note it is not $HOME: /home/user/.claude",
        )),
        (true, true) => out.push(ok("CLAUDE_CODE_REMOTE_MEMORY_DIR", "set")),
        (false, true) => out.push(ok(
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "set, so the memory root is this rather than ~/.claude",
        )),
        (false, false) => out.push(ok(
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "not needed outside a remote session",
        )),
    }

    out.push(ok(
        "memory dir",
        format!("{} ({} files)", rep.memory_dir, rep.memory_files),
    ));

    offbox_finding(rep, &mut out);

    // ---- is anything quietly going nowhere
    reserved_findings(rep, &mut out);

    if let Some(mis) = &rep.miscased_dir {
        out.push(fail(
            "reserved directory",
            format!(
                "{}/ is spelled differently from {}/, so nothing under it syncs — \
                 and on a case-insensitive filesystem it looks identical",
                mis.found, mis.reserved
            ),
            format!("rename {}/ to {}/", mis.found, mis.reserved),
        ));
    }

    for var in &rep.rejected_vars {
        out.push(fail(
            "rejected value",
            format!("{var} is set to a value Recall refused, so it did nothing"),
            format!("recall status names the file behind {var}"),
        ));
    }

    for file in &rep.unreadable_settings {
        out.push(fail(
            "settings file",
            format!("{file} is not readable JSON, so nothing it declares is in effect"),
            "Claude Code cannot read it either — fix the JSON".to_string(),
        ));
    }

    for ig in &rep.ignored_env {
        out.push(warn(
            "ignored value",
            format!(
                "{} is declared in {} but its value is not a string, so it was skipped",
                ig.name, ig.file
            ),
            "quote the value".to_string(),
        ));
    }

    out
}

/// Where a value came from, in the words a finding uses.
fn source_detail(rep: &Report, source: Source) -> String {
    match source {
        Source::CredentialsFile => format!(
            "saved in {}",
            rep.credentials_file
                .as_deref()
                .unwrap_or("the credentials file")
        ),
        _ => "set".to_string(),
    }
}

/// Where the token lives, which matters as much as whether it is set.
///
/// The warning speaks on a laptop and never in a remote session. A cloud
/// environment's variables are a secret store and the right place for the
/// token there; a shell profile is a dotfile that every subprocess inherits
/// from, that gets committed to dotfiles repositories, and whose `export`
/// line sits in shell history. The same `CLAUDE_CODE_REMOTE` signal decides
/// the memory-dir check below.
fn credentials_findings(rep: &Report, out: &mut Vec<Finding>) {
    if rep.token_source == Source::Environment && !rep.remote_session {
        // A settings file can be named because it was read. The shell
        // cannot: by the time a process sees a variable, which profile
        // exported it is gone, and naming a guess would send someone to
        // edit the wrong file and believe they were done.
        let (detail, remove_from) = match rep.declared_env.iter().find(|d| d.name == "RECALL_TOKEN")
        {
            Some(d) => (
                format!("RECALL_TOKEN is set in {}, in plain text", d.file),
                d.file.clone(),
            ),
            None => (
                "RECALL_TOKEN comes from your shell, so every process started from it \
                 inherits the token"
                    .to_string(),
                "your shell profile".to_string(),
            ),
        };
        out.push(warn(
            "token storage",
            detail,
            format!(
                "recall connect <url> saves it to ~/.recall/credentials.json, readable by \
                 you only; then remove RECALL_TOKEN from {remove_from}"
            ),
        ));
    }

    if let Some(err) = &rep.credentials_error {
        out.push(warn(
            "credentials file",
            format!("{err} — so nothing in it is in effect"),
            "move it aside and run recall connect again",
        ));
    }

    if rep.credentials_exposed {
        let file = rep
            .credentials_file
            .as_deref()
            .unwrap_or("~/.recall/credentials.json");
        out.push(warn(
            "credentials file",
            format!("{file} is readable by other users"),
            format!("chmod 600 {file}"),
        ));
    }
}

/// How long an off-box copy may be missing before it is worth saying so.
///
/// Generous against the cadence `deploy/README.md` recommends — six-hourly,
/// so eight runs would have to fail before this speaks. That asymmetry is
/// deliberate: a false alarm here costs the credibility of every other line
/// in the report, and a real stoppage stays true for days.
const OFFBOX_STALE_AFTER: time::Duration = time::Duration::days(2);

/// Whether a copy has recently reached somewhere the loss of the server does
/// not reach.
///
/// Nothing watched this before. The server's own snapshots surface as
/// `last_backup_at` and go stale visibly, but they sit on the disk they
/// protect; the copy that survives losing the machine ran from cron, and a
/// cron job that dies mails its error to a mailbox nobody reads. A transient
/// 403 from the bucket looked exactly like a quiet night.
///
/// Silence when no stamp exists at all, rather than a standing complaint: a
/// deployment with no off-box backup configured is not broken, and warning
/// it forever is how a report teaches people to skip it.
fn offbox_finding(rep: &Report, out: &mut Vec<Finding>) {
    let Some(stamp) = rep.last_offbox_at.as_deref() else {
        return;
    };

    let Some(age) = age_of(stamp) else {
        out.push(warn(
            "off-box backup",
            format!("the server reported a stamp this cannot read: {stamp}"),
            "expected the API's timestamp format, e.g. 2026-09-22T00:17:03.000Z",
        ));
        return;
    };

    if age > OFFBOX_STALE_AFTER {
        out.push(warn(
            "off-box backup",
            format!(
                "last verified copy was {} days ago ({stamp}) — the snapshots on \
                 the server are the only copies of anything newer",
                age.whole_days()
            ),
            "on the server: check the cron job's mail, then run \
             RECALL_BACKUP_REMOTE=... ./deploy/backup-offbox.sh by hand",
        ));
    } else {
        out.push(ok("off-box backup", format!("verified {stamp}")));
    }
}

/// How long ago a timestamp in the API's format was.
///
/// [`None`] rather than a guess when it cannot be parsed — a report that
/// invents an age is worse than one that admits it cannot read the value.
fn age_of(stamp: &str) -> Option<time::Duration> {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let at = time::PrimitiveDateTime::parse(stamp, &fmt)
        .ok()?
        .assume_utc();
    Some(time::OffsetDateTime::now_utc() - at)
}

/// The two reserved scopes, asked the same two questions each.
///
/// Both questions exist because both have already been shipped wrong: files
/// under a scope that is switched off go nowhere, and files under one that
/// is on are still never read unless `MEMORY.md` links them.
fn reserved_findings(rep: &Report, out: &mut Vec<Finding>) {
    let scopes: [(&'static str, &'static str, &Option<String>, usize, bool); 2] = [
        (
            "global scope",
            "RECALL_GLOBAL_KEY",
            &rep.global_key,
            rep.global_files,
            rep.global_linked,
        ),
        (
            "machine scope",
            "RECALL_MACHINE_KEY",
            &rep.machine_key,
            rep.machine_files,
            rep.machine_linked,
        ),
    ];

    for (name, var, key, files, linked) in scopes {
        match key {
            // Off with files sitting under it: they are not filed under the
            // project either, so they sync nowhere at all, and silently.
            None if files > 0 => out.push(warn(
                name,
                format!("off, but {files} file(s) are on disk under it and sync nowhere"),
                format!("set {var}, or move the files out"),
            )),
            None => out.push(ok(name, "off")),
            Some(k) if files > 0 && !linked => out.push(fail(
                name,
                format!(
                    "{k}: {files} file(s) synced, but MEMORY.md links none of them — \
                     Claude Code opens what MEMORY.md links and nothing else"
                ),
                "recall pull".to_string(),
            )),
            Some(k) => out.push(ok(name, format!("{k} ({files} files)"))),
        }
    }
}

/// Collects, judges, prints, and exits accordingly.
pub async fn run(as_json: bool) -> anyhow::Result<i32> {
    let here = proj::resolve();
    let cfg = here.config();
    let rep = status::collect(&here, &cfg).await;
    let found = findings(&rep);

    if as_json {
        println!("{}", serde_json::to_string_pretty(&found)?);
    } else {
        print_text(&cfg, &found);
    }
    Ok(verdict(&found))
}

/// [`exit::CONFIG`] on any failure — "a misconfiguration only the user can
/// fix", which is exactly what every `Fail` above is.
///
/// Warnings never reach this. A command that exits non-zero over a scope
/// someone deliberately left off is a command that gets `|| true` appended
/// to it, and then it is not checking anything.
pub(crate) fn verdict(found: &[Finding]) -> i32 {
    if found.iter().any(|f| f.level == Level::Fail) {
        exit::CONFIG
    } else {
        exit::OK
    }
}

fn print_text(cfg: &ClientConfig, found: &[Finding]) {
    for f in found {
        println!("  {} {:<30} {}", f.level.mark(), f.check, f.detail);
        if let Some(fix) = &f.fix {
            println!("       {:<30} → {}", "", fix);
        }
    }

    let fails = found.iter().filter(|f| f.level == Level::Fail).count();
    let warns = found.iter().filter(|f| f.level == Level::Warn).count();

    println!();
    match (fails, warns) {
        (0, 0) => println!("  Everything checks out."),
        // Named plainly, because the whole point is that this state used to
        // look identical to a healthy one.
        (0, w) => println!("  Nothing broken. {w} thing(s) worth a look."),
        (f, _) => println!(
            "  {f} problem(s). Recall is not syncing {}.",
            if cfg.url.is_empty() {
                "anything here"
            } else {
                "everything it looks like it is"
            }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{KeySource, MiscasedDir};

    /// Everything configured and working, for a test to break one thing in.
    ///
    /// Built healthy rather than empty on purpose: a fixture that starts
    /// broken makes it far too easy to write a test that passes because of
    /// a failure it was not looking at.
    fn healthy() -> Report {
        Report {
            project: "/w/app".into(),
            project_key: "acme/app".into(),
            project_key_source: KeySource::Remote,
            memory_dir: "/w/memory".into(),
            memory_files: 4,
            hooks_wired: true,
            // A wired project on a laptop — the ordinary case, and the one
            // where both of the checks below used to be wrong.
            in_git_repo: true,
            remote_session: false,
            declared_env: Vec::new(),
            ignored_env: Vec::new(),
            unreadable_settings: Vec::new(),
            global_key: None,
            rejected_vars: Vec::new(),
            global_files: 0,
            machine_key: None,
            machine_files: 0,
            machine_linked: false,
            miscased_dir: None,
            global_linked: false,
            remote_memory_dir_set: true,
            url_set: true,
            token_set: true,
            // Saved by `recall connect` — the setup that should raise
            // nothing at all.
            url_source: Source::CredentialsFile,
            token_source: Source::CredentialsFile,
            credentials_file: Some("/h/.recall/credentials.json".into()),
            credentials_error: None,
            credentials_exposed: false,
            server_ok: true,
            server_error: None,
            git_commit: Some("a1b2c3d".into()),
            merge_ready: true,
            synced_files: 4,
            last_synced_at: None,
            // No stamp: the ordinary case for a deployment with no off-box
            // backup, and the one that must stay silent.
            last_offbox_at: None,
        }
    }

    fn find<'a>(found: &'a [Finding], check: &str) -> Option<&'a Finding> {
        found.iter().find(|f| f.check == check)
    }

    #[test]
    fn a_healthy_setup_exits_zero_and_flags_nothing() {
        let found = findings(&healthy());
        assert_eq!(verdict(&found), exit::OK);
        assert!(
            found.iter().all(|f| f.level == Level::Ok),
            "{:?}",
            found
                .iter()
                .filter(|f| f.level != Level::Ok)
                .collect::<Vec<_>>()
        );
    }

    /// The case that cost a day: hooks wired, binary installed, nothing set.
    #[test]
    fn an_unconfigured_environment_fails_rather_than_looking_idle() {
        let mut rep = healthy();
        rep.url_set = false;
        rep.token_set = false;
        rep.server_ok = false;
        rep.git_commit = None;

        let found = findings(&rep);

        assert_eq!(find(&found, "RECALL_URL").unwrap().level, Level::Fail);
        assert_eq!(find(&found, "RECALL_TOKEN").unwrap().level, Level::Fail);
        assert_eq!(verdict(&found), exit::CONFIG);
    }

    /// With no URL there is nothing to be unreachable, and saying so anyway
    /// buries the finding that explains it.
    #[test]
    fn the_server_is_not_reported_on_when_there_is_no_url() {
        let mut rep = healthy();
        rep.url_set = false;
        rep.server_ok = false;

        assert!(find(&findings(&rep), "server").is_none());
    }

    /// A token in the environment on a laptop is the thing `recall connect`
    /// exists to replace, so doctor says so — once, as a warning, and with
    /// the command that fixes it.
    #[test]
    fn a_shell_token_on_a_laptop_is_a_warning_that_names_the_fix() {
        let mut rep = healthy();
        rep.token_source = Source::Environment;

        let found = findings(&rep);
        let f = find(&found, "token storage").expect("a shell token is worth a word");

        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("shell"), "{}", f.detail);
        assert!(f.fix.as_deref().unwrap().contains("recall connect"));
        assert_eq!(
            verdict(&found),
            exit::OK,
            "a warning never fails the command"
        );
    }

    /// In a cloud environment the variables *are* the secret store, and a
    /// file written there is discarded with the container. Warning there
    /// would be wrong on every session start.
    #[test]
    fn a_token_in_the_environment_of_a_remote_session_is_fine() {
        let mut rep = healthy();
        rep.token_source = Source::Environment;
        rep.remote_session = true;

        assert!(find(&findings(&rep), "token storage").is_none());
    }

    /// A settings file was read, so it can be named. The shell cannot, and
    /// the finding must not pretend otherwise.
    #[test]
    fn a_token_from_a_settings_file_names_the_file() {
        let mut rep = healthy();
        rep.token_source = Source::Environment;
        rep.declared_env = vec![recall_hooks::declared_env::Declared {
            name: "RECALL_TOKEN".into(),
            file: "/w/app/.claude/settings.local.json".into(),
            shadows_shell: false,
            empty: false,
        }];

        let found = findings(&rep);
        let f = find(&found, "token storage").unwrap();
        assert!(f.detail.contains("settings.local.json"), "{}", f.detail);
        assert!(f.fix.as_deref().unwrap().contains("settings.local.json"));
    }

    #[test]
    fn a_credentials_file_others_can_read_is_a_warning_with_the_chmod() {
        let mut rep = healthy();
        rep.credentials_exposed = true;

        let found = findings(&rep);
        let f = find(&found, "credentials file").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.fix.as_deref().unwrap().starts_with("chmod 600"));
    }

    /// Every check must earn its exit code. A laptop with no machine scope
    /// and no remote memory dir is not broken, and a doctor that says it is
    /// gets `|| true` appended to it and stops checking anything.
    #[test]
    fn warnings_alone_never_fail_the_command() {
        let mut rep = healthy();
        rep.remote_memory_dir_set = false;
        rep.merge_ready = false;

        let found = findings(&rep);

        assert!(found.iter().any(|f| f.level == Level::Warn));
        assert!(found.iter().all(|f| f.level != Level::Fail));
        assert_eq!(verdict(&found), exit::OK);
    }

    /// Files under a scope that is switched off are not filed under the
    /// project either. They sync nowhere, and nothing else in Recall says so.
    #[test]
    fn files_under_a_scope_that_is_off_are_reported() {
        let mut rep = healthy();
        rep.global_files = 3;

        let found = findings(&rep);
        let f = find(&found, "global scope").unwrap();

        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains('3'), "{}", f.detail);
        assert!(
            f.fix.as_deref().unwrap().contains("RECALL_GLOBAL_KEY"),
            "the fix has to name the variable that switches it on"
        );
    }

    /// The bug this project shipped twice, asked as a question: synced is
    /// not the same as read.
    #[test]
    fn a_scope_whose_files_memory_md_does_not_link_is_a_failure() {
        let mut rep = healthy();
        rep.machine_key = Some("machine:mbp".into());
        rep.machine_files = 2;
        rep.machine_linked = false;

        let found = findings(&rep);
        let f = find(&found, "machine scope").unwrap();

        assert_eq!(f.level, Level::Fail);
        assert!(f.detail.contains("MEMORY.md"), "{}", f.detail);
    }

    #[test]
    fn a_linked_scope_with_files_is_fine() {
        let mut rep = healthy();
        rep.machine_key = Some("machine:mbp".into());
        rep.machine_files = 2;
        rep.machine_linked = true;

        assert_eq!(
            find(&findings(&rep), "machine scope").unwrap().level,
            Level::Ok
        );
    }

    /// Invisible on a case-insensitive filesystem, which is most laptops.
    #[test]
    fn a_miscased_reserved_directory_fails() {
        let mut rep = healthy();
        rep.miscased_dir = Some(MiscasedDir {
            found: "Global".into(),
            reserved: "global",
        });

        let found = findings(&rep);

        assert_eq!(
            find(&found, "reserved directory").unwrap().level,
            Level::Fail
        );
        assert_eq!(verdict(&found), exit::CONFIG);
    }

    /// A variable set to a value Recall refused did nothing, and the only
    /// other symptom is memory syncing somewhere you did not ask for.
    #[test]
    fn a_rejected_variable_fails_and_names_itself() {
        let mut rep = healthy();
        rep.rejected_vars = vec!["RECALL_PROJECT_KEY"];

        let found = findings(&rep);
        let f = find(&found, "rejected value").unwrap();

        assert_eq!(f.level, Level::Fail);
        assert!(f.detail.contains("RECALL_PROJECT_KEY"), "{}", f.detail);
    }

    /// Anything not `Ok` has to say what to do about it. A finding a reader
    /// cannot act on teaches them to skip the whole report.
    #[test]
    fn every_finding_that_is_not_ok_carries_a_fix() {
        let mut rep = healthy();
        rep.url_set = false;
        rep.token_set = false;
        rep.hooks_wired = false;
        rep.remote_memory_dir_set = false;
        rep.global_files = 3;
        rep.machine_key = Some("machine:mbp".into());
        rep.machine_files = 1;
        rep.machine_linked = false;
        rep.miscased_dir = Some(MiscasedDir {
            found: "Global".into(),
            reserved: "global",
        });
        rep.rejected_vars = vec!["RECALL_PROJECT_KEY"];
        rep.unreadable_settings = vec![".claude/settings.json".into()];

        for f in findings(&rep).iter().filter(|f| f.level != Level::Ok) {
            assert!(
                f.fix.as_deref().is_some_and(|s| !s.is_empty()),
                "{} has no fix",
                f.check
            );
        }
    }

    /// In a remote session an unset memory dir is not a nuance — Claude
    /// Code's auto-memory is off entirely, so nothing syncs however correct
    /// the rest is. It shipped as a `Warn`, which left the one case where it
    /// is fatal unable to change the exit code.
    ///
    /// The fix has to spell the value out. It is not `$HOME` — in a cloud
    /// session `$HOME` is `/root` while the answer is `/home/user/.claude` —
    /// and before this it appeared nowhere but a ROADMAP checkbox.
    #[test]
    fn an_unset_remote_memory_dir_fails_in_a_remote_session() {
        let mut rep = healthy();
        rep.remote_session = true;
        rep.remote_memory_dir_set = false;

        let found = findings(&rep);
        let f = find(&found, "CLAUDE_CODE_REMOTE_MEMORY_DIR").unwrap();

        assert_eq!(f.level, Level::Fail);
        assert!(
            f.fix.as_deref().unwrap().contains("/home/user/.claude"),
            "{:?}",
            f.fix
        );
        assert_eq!(verdict(&found), exit::CONFIG);
    }

    /// And on a laptop the same state is correct, so reporting it forever
    /// is noise — the kind that teaches someone to stop reading the output.
    #[test]
    fn an_unset_remote_memory_dir_is_fine_on_a_laptop() {
        let mut rep = healthy();
        rep.remote_session = false;
        rep.remote_memory_dir_set = false;

        let found = findings(&rep);

        assert_eq!(
            find(&found, "CLAUDE_CODE_REMOTE_MEMORY_DIR").unwrap().level,
            Level::Ok
        );
        assert_eq!(verdict(&found), exit::OK);
    }

    /// Checking your connection from a home directory is an ordinary thing
    /// to do. The first version of this exited 1 for it, because hooks are
    /// not wired there — true, and not a problem.
    #[test]
    fn unwired_hooks_outside_a_git_repository_are_not_a_failure() {
        let mut rep = healthy();
        rep.in_git_repo = false;
        rep.hooks_wired = false;

        let found = findings(&rep);

        assert_eq!(find(&found, "hooks").unwrap().level, Level::Ok);
        assert_eq!(verdict(&found), exit::OK);
    }

    /// The other side of it, which must keep failing: inside a repository,
    /// unwired hooks mean this project is not syncing at all.
    #[test]
    fn unwired_hooks_inside_a_git_repository_still_fail() {
        let mut rep = healthy();
        rep.in_git_repo = true;
        rep.hooks_wired = false;

        let found = findings(&rep);
        let f = find(&found, "hooks").unwrap();

        assert_eq!(f.level, Level::Fail);
        assert_eq!(f.fix.as_deref(), Some("recall init"));
        assert_eq!(verdict(&found), exit::CONFIG);
    }

    // ---------------------------------------------------------------- off-box

    /// The ordinary case for a deployment that has no off-box backup at all.
    /// A standing complaint here would be the same noise this command has
    /// already had to have removed twice.
    #[test]
    fn no_offbox_stamp_is_reported_as_nothing() {
        let found = findings(&healthy());
        assert!(find(&found, "off-box backup").is_none());
    }

    #[test]
    fn a_recent_offbox_copy_is_fine() {
        let mut rep = healthy();
        rep.last_offbox_at = Some(stamp_days_ago(1));

        let found = findings(&rep);

        assert_eq!(find(&found, "off-box backup").unwrap().level, Level::Ok);
        assert_eq!(verdict(&found), exit::OK);
    }

    /// The case a transient 403 from the bucket produced in production: the
    /// copy stopped, cron kept firing, and nothing anywhere said so.
    #[test]
    fn an_offbox_copy_that_stopped_is_reported_with_its_age() {
        let mut rep = healthy();
        rep.last_offbox_at = Some(stamp_days_ago(9));

        let found = findings(&rep);
        let f = find(&found, "off-box backup").unwrap();

        assert_eq!(f.level, Level::Warn);
        assert!(
            f.detail.contains('9'),
            "the age has to be in it: {}",
            f.detail
        );
        // A backup that stopped is serious and is still not "memory is not
        // syncing", which is what Fail means here. Warn keeps the exit code
        // honest.
        assert_eq!(verdict(&found), exit::OK);
    }

    /// Six-hourly is the recommended cadence, so eight runs have to fail
    /// before this speaks. One missed night must stay silent.
    #[test]
    fn a_single_missed_run_is_not_worth_reporting() {
        let mut rep = healthy();
        rep.last_offbox_at = Some(stamp_days_ago(1));

        assert_eq!(
            find(&findings(&rep), "off-box backup").unwrap().level,
            Level::Ok
        );
    }

    /// A report that invents an age is worse than one admitting it cannot
    /// read the value.
    #[test]
    fn an_unreadable_stamp_says_so_rather_than_guessing() {
        let mut rep = healthy();
        rep.last_offbox_at = Some("yesterday-ish".into());

        let found = findings(&rep);
        let f = find(&found, "off-box backup").unwrap();

        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("yesterday-ish"), "{}", f.detail);
    }

    fn stamp_days_ago(days: i64) -> String {
        let fmt = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        (time::OffsetDateTime::now_utc() - time::Duration::days(days))
            .format(&fmt)
            .unwrap()
    }
}
