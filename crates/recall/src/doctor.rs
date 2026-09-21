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
    "cloud environment: the \"Add/Edit cloud environment\" dialog; laptop: your shell profile";

/// Reads the report into findings, most fundamental first.
///
/// Ordered so the first `FAIL` is the one worth fixing first: there is no
/// point reporting an unreachable server above an unset URL, because the
/// second explains the first.
pub(crate) fn findings(rep: &Report) -> Vec<Finding> {
    let mut out = Vec::new();

    // ---- can it reach a server at all
    if rep.url_set {
        out.push(ok("RECALL_URL", "set"));
    } else {
        out.push(fail("RECALL_URL", "not set anywhere", WHERE_TO_SET));
    }

    if rep.token_set {
        out.push(ok("RECALL_TOKEN", "set"));
    } else {
        out.push(fail("RECALL_TOKEN", "not set anywhere", WHERE_TO_SET));
    }

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
    if rep.hooks_wired {
        out.push(ok("hooks", "wired in .claude/settings.json"));
    } else {
        out.push(fail(
            "hooks",
            "this project's .claude/settings.json has no Recall hooks",
            "recall init",
        ));
    }

    // Unset is right on a laptop and fatal in a remote session, and nothing
    // here can tell which this is — so it is a warning either way, with the
    // value spelled out, because it is the one value in the whole setup
    // that cannot be worked out from first principles.
    if !rep.remote_memory_dir_set {
        out.push(warn(
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "not set — correct on a laptop; in a remote or cloud session it means \
             Claude Code's auto-memory is off entirely, and Recall has nothing to sync",
            "cloud environment only, and note it is not $HOME: /home/user/.claude",
        ));
    } else {
        out.push(ok("CLAUDE_CODE_REMOTE_MEMORY_DIR", "set"));
    }

    out.push(ok(
        "memory dir",
        format!("{} ({} files)", rep.memory_dir, rep.memory_files),
    ));

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
            server_ok: true,
            server_error: None,
            git_commit: Some("a1b2c3d".into()),
            merge_ready: true,
            synced_files: 4,
            last_synced_at: None,
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

    /// The value that cannot be worked out from first principles — it is not
    /// `$HOME`, and in a cloud session `$HOME` is `/root` while the answer is
    /// `/home/user/.claude`. If it is not in the output, the reader has to
    /// find it in a ROADMAP checkbox, which is where it used to live.
    #[test]
    fn the_remote_memory_dir_warning_spells_out_the_value() {
        let mut rep = healthy();
        rep.remote_memory_dir_set = false;

        let found = findings(&rep);
        let f = find(&found, "CLAUDE_CODE_REMOTE_MEMORY_DIR").unwrap();

        assert_eq!(f.level, Level::Warn);
        assert!(
            f.fix.as_deref().unwrap().contains("/home/user/.claude"),
            "{:?}",
            f.fix
        );
    }
}
