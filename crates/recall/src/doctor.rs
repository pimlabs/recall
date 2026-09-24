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
use crate::ui;

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
                format!("unreachable: {err}"),
                "check RECALL_URL. A cloud environment also needs the domain under \
                 Allowed domains",
            )),
            (None, _) if rep.server_version.is_some() => out.push(ok(
                "server",
                format!(
                    "answered, {}{}",
                    rep.server_version.as_deref().unwrap_or_default(),
                    if rep.server_channel.as_deref() == Some("dev") {
                        " (not a release build)"
                    } else {
                        ""
                    }
                ),
            )),
            (None, Some(commit)) => out.push(ok("server", format!("answered, commit {commit}"))),
            (None, None) => out.push(ok("server", "answered")),
        }
        version_findings(rep, &mut out);
    }

    let quiet = worker_quiet(rep).filter(|_| rep.server_ok);
    if let Some(quiet) = quiet {
        // Before the CLI: what the worker last said about it is only as
        // current as the worker.
        out.push(warn(
            "merge",
            format!(
                "the merge worker has not asked for work in {} minutes, so conflicting \
                 edits wait in its queue unmerged, unless the server's own Claude CLI \
                 can merge them",
                quiet.whole_minutes()
            ),
            "on the server: docker compose logs recall-worker, and check it is running",
        ));
    } else if rep.server_ok && !rep.merge_ready && rep.merge_worker {
        out.push(warn(
            "merge",
            "the merge worker's Claude CLI is not logged in, so conflicting edits wait \
             in its queue unmerged",
            "on the server: docker compose exec -it -u node recall-worker claude setup-token",
        ));
    } else if rep.server_ok && !rep.merge_ready {
        out.push(warn(
            "merge",
            "the server's Claude CLI is not logged in, so conflicting edits use \
             last-write-wins",
            "on the server: docker compose exec -it -u node recall-server claude setup-token",
        ));
    }
    if rep.server_ok {
        queue_finding(rep, &mut out);
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
        (false, false) => out.push(ok("hooks", "not in a git repository")),
    }

    // Windows only: Claude Code runs hook commands through Git Bash there
    // and falls back to PowerShell without it, which cannot run the
    // bash-form command `recall init` writes at all — the hook silently does
    // nothing on every edit, and nothing else in this report would explain
    // why. Nothing to check on Unix, where the hook command runs directly.
    #[cfg(windows)]
    out.push(git_bash_finding());

    // `CLAUDE_CODE_REMOTE` answers the question that made this a permanent
    // warning in both directions: unset is correct on a laptop and means
    // Claude Code's auto-memory is off entirely in a remote session, where
    // Recall then has nothing to sync no matter what else is right.
    match (rep.remote_session, rep.remote_memory_dir_set) {
        (true, false) => out.push(fail(
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "not set, so auto-memory is off in this remote session",
            // The one value in the whole setup that cannot be reasoned out.
            "set it to /home/user/.claude on the cloud environment (not $HOME)",
        )),
        (true, true) => out.push(ok("CLAUDE_CODE_REMOTE_MEMORY_DIR", "set")),
        (false, true) => out.push(ok(
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "set, memory lives here instead of ~/.claude",
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
                "{}/ should be {}/ (case matters), so nothing under it syncs",
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
            "fix the JSON, Claude Code cannot read it either".to_string(),
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

/// Whether this client and the server can talk at all, from the server's
/// discovery document. Silent against a server too old to publish one:
/// such a server speaks protocol 1, which is what this client speaks.
fn version_findings(rep: &Report, out: &mut Vec<Finding>) {
    if !rep.server_protocols.is_empty() && !rep.server_protocols.contains(&recall_wire::PROTOCOL) {
        out.push(fail(
            "version",
            format!(
                "the server speaks protocol {:?} and this client speaks {}",
                rep.server_protocols,
                recall_wire::PROTOCOL
            ),
            "upgrade whichever side is older",
        ));
    }
    let too_old = rep.min_client.as_deref().and_then(|min| {
        let min_v = recall_wire::discovery::Version::parse(min)?;
        let mine = recall_wire::discovery::Version::parse(&rep.client_version)?;
        (mine < min_v).then(|| min.to_string())
    });
    if let Some(min) = too_old {
        out.push(fail(
            "version",
            format!(
                "this client is {} and the server needs at least {min}",
                rep.client_version
            ),
            "upgrade recall: brew upgrade recall, or npm install -g @pimlabs/recall",
        ));
    }
}

/// Whether Claude Code has a Git Bash to run hook commands through.
///
/// Checked the same way Claude Code itself is documented to resolve it:
/// `CLAUDE_CODE_GIT_BASH_PATH` first, then `bash.exe` actually belonging to
/// Git for Windows on `PATH` (not WSL's `System32\bash.exe`, which answers to
/// the same name but is a different thing), then the two locations Git for
/// Windows' own installer offers by default. **Not verified against a real
/// Windows Claude Code install** — the env var name and the fallback order
/// are the best-documented guess, not a confirmed contract. See
/// docs/reference/install.md's Windows section.
#[cfg(windows)]
fn git_bash_finding() -> Finding {
    if git_bash_available() {
        ok("Git Bash", "found")
    } else {
        fail(
            "Git Bash",
            "not found, so Claude Code cannot run the hooks recall init writes \
             (it falls back to PowerShell, which the committed hook command \
             cannot run)",
            "install Git for Windows: https://git-scm.com/download/win",
        )
    }
}

#[cfg(windows)]
fn git_bash_available() -> bool {
    if let Ok(path) = std::env::var("CLAUDE_CODE_GIT_BASH_PATH") {
        if !path.is_empty() && std::path::Path::new(&path).is_file() {
            return true;
        }
    }
    if path_bash_is_git_bash() {
        return true;
    }
    // Git for Windows' installer offers to skip adding itself to PATH, so a
    // working install can still miss the check above.
    for candidate in [
        r"C:\Program Files\Git\bin\bash.exe",
        r"C:\Program Files (x86)\Git\bin\bash.exe",
    ] {
        if std::path::Path::new(candidate).is_file() {
            return true;
        }
    }
    false
}

/// `bash.exe` on `PATH` might be WSL's launcher (`System32\bash.exe`) rather
/// than Git for Windows' — same name, and WSL's answers to `bash -c` too, so
/// it cannot be told apart by running it. Only a path that plainly names a
/// Git installation is trusted.
#[cfg(windows)]
fn path_bash_is_git_bash() -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join("bash.exe").is_file()
                    && dir.to_string_lossy().to_ascii_lowercase().contains("git")
            })
        })
        .unwrap_or(false)
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
        Source::ConfigFile => format!(
            "saved in {}",
            rep.config_file.as_deref().unwrap_or("the config file")
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
                "RECALL_TOKEN is exported by your shell, so every program you start \
                 can read it"
                    .to_string(),
                "your shell profile".to_string(),
            ),
        };
        out.push(warn(
            "token storage",
            detail,
            format!(
                "run recall connect to save it in ~/.recall/credentials.toml, then \
                 remove RECALL_TOKEN from {remove_from}"
            ),
        ));
    }

    let config_file = rep
        .config_file
        .as_deref()
        .unwrap_or("~/.recall/config.toml");
    for problem in &rep.config_problems {
        out.push(warn(
            "config file",
            problem.clone(),
            format!("edit {config_file}"),
        ));
    }
    // The environment wins over the file, deliberately — but on a laptop
    // that has run `recall connect`, a different value left in the shell is
    // almost always a leftover, and it silently decides which machine this
    // is.
    for o in &rep.overridden {
        let from = rep
            .declared_env
            .iter()
            .find(|d| d.name == o.variable)
            .map(|d| d.file.clone())
            .unwrap_or_else(|| "your shell profile".to_string());
        out.push(warn(
            "config overridden",
            format!(
                "{}={} wins over {} = {:?} in {config_file}",
                o.variable, o.environment, o.setting, o.config
            ),
            format!(
                "remove {} from {from}, or change {} to match",
                o.variable, o.setting
            ),
        ));
    }

    if let Some(err) = &rep.credentials_error {
        out.push(warn(
            "credentials file",
            format!("{err}, so nothing in it is in effect"),
            "move it aside and run recall connect again",
        ));
    }

    if rep.credentials_exposed {
        let file = rep
            .credentials_file
            .as_deref()
            .unwrap_or("~/.recall/credentials.toml");
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
                "last verified copy is {} days old ({stamp}), anything newer exists \
                 only on the server",
                age.whole_days()
            ),
            "on the server: check the cron job's mail, then run \
             RECALL_BACKUP_REMOTE=... ./deploy/backup-offbox.sh by hand",
        ));
    } else {
        out.push(ok("off-box backup", format!("verified {stamp}")));
    }
}

/// How long the oldest merge may wait before it is worth saying so. A
/// worker drains a job within seconds of the push; an hour means it is not
/// running, or cannot merge.
const QUEUE_STALE_AFTER: time::Duration = time::Duration::hours(1);

/// How long a merge worker may go without asking for work before it is
/// worth saying so. A running one asks at least every half minute.
const WORKER_QUIET_AFTER: time::Duration = time::Duration::minutes(2);

/// How long the merge worker has gone without asking for work, when that is
/// long enough to say so: [`None`] without a worker, or with one that asks.
pub(crate) fn worker_quiet(rep: &Report) -> Option<time::Duration> {
    if !rep.merge_worker {
        return None;
    }
    rep.merge_worker_seen_at
        .as_deref()
        .and_then(age_of)
        .filter(|quiet| *quiet >= WORKER_QUIET_AFTER)
}

/// The merge worker's queue, when there is a worker: silent while it
/// drains, loud once the oldest job has waited an hour, the way a merge
/// that degrades to last-write-wins is made visible rather than silent.
fn queue_finding(rep: &Report, out: &mut Vec<Finding>) {
    let Some(q) = &rep.merge_queue else {
        return;
    };
    let waited = q.oldest_queued_at.as_deref().and_then(age_of);
    match waited {
        Some(age) if age >= QUEUE_STALE_AFTER => out.push(warn(
            "merge queue",
            format!(
                "{} merge{} waiting, the oldest for {} minutes; pushes still land, \
                 unmerged, until the worker takes them",
                q.queued,
                if q.queued == 1 { "" } else { "s" },
                age.whole_minutes()
            ),
            "on the server: docker compose logs recall-worker, and check it is running",
        )),
        _ => out.push(ok(
            "merge queue",
            match q.queued {
                0 => "nothing waiting".to_string(),
                n => format!("{n} waiting"),
            },
        )),
    }
    // A failed merge left the newest push standing and kept the merge in
    // its job, where nothing retries it by itself.
    if q.failed > 0 {
        out.push(warn(
            "failed merges",
            format!(
                "{} merge{} failed; for each, the newest push stands and the merge is kept \
                 in its job",
                q.failed,
                if q.failed == 1 { "" } else { "s" },
            ),
            "list them with GET /v1/jobs?state=failed and retry one with \
             POST /v1/jobs/{id}/retry, both with the operator token",
        ));
    }
}

/// How long ago a timestamp in the API's format was.
///
/// [`None`] rather than a guess when it cannot be parsed — a report that
/// invents an age is worse than one that admits it cannot read the value.
pub(crate) fn age_of(stamp: &str) -> Option<time::Duration> {
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
                    "{k}: {files} file(s) synced, but MEMORY.md links none of them, so \
                     Claude Code never reads them"
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
        print_text(&cfg, &rep, &found);
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

/// The groups a report is read in, in order. A check belongs to the first
/// group that names it; anything unlisted lands in the last, so a check
/// added later is never silently left out of the report.
const SECTIONS: &[(&str, &[&str])] = &[
    (
        "Connection",
        &[
            "RECALL_URL",
            "RECALL_TOKEN",
            "server",
            "version",
            "merge",
            "merge queue",
            "failed merges",
            "token storage",
            "credentials file",
            "config file",
            "config overridden",
        ],
    ),
    (
        "This project",
        &[
            "hooks",
            "Git Bash",
            "memory dir",
            "CLAUDE_CODE_REMOTE_MEMORY_DIR",
            "settings file",
            "ignored value",
            "rejected value",
            "reserved directory",
        ],
    ),
    ("Scopes", &["global scope", "machine scope"]),
    ("Backup", &["off-box backup"]),
];

const OTHER: &str = "Other";

fn section_of(check: &str) -> &'static str {
    SECTIONS
        .iter()
        .find(|(_, checks)| checks.contains(&check))
        .map(|(name, _)| *name)
        .unwrap_or(OTHER)
}

/// How a finding is marked. An `ok` that describes something switched off
/// or not applicable is quiet rather than green: "global scope: off" is not
/// an achievement, and marking it like one dilutes the marks that are.
fn tone_of(f: &Finding) -> ui::Tone {
    match f.level {
        Level::Fail => ui::Tone::Bad,
        Level::Warn => ui::Tone::Warn,
        Level::Ok
            if f.detail == "off"
                || f.detail.starts_with("not needed")
                || f.detail.starts_with("not in a git repository") =>
        {
            ui::Tone::Quiet
        }
        Level::Ok => ui::Tone::Good,
    }
}

fn print_text(cfg: &ClientConfig, rep: &Report, found: &[Finding]) {
    let server = cfg
        .url
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/'))
        .unwrap_or("no server");
    ui::title("recall doctor", &format!("{} → {server}", cfg.source_env));

    let width = found.iter().map(|f| f.check.len()).max().unwrap_or(0);
    let names = SECTIONS.iter().map(|(n, _)| *n).chain([OTHER]);
    for name in names {
        let items: Vec<&Finding> = found
            .iter()
            .filter(|f| section_of(f.check) == name)
            .collect();
        if items.is_empty() {
            continue;
        }
        let about = if name == "This project" {
            rep.project_key.as_str()
        } else {
            ""
        };
        ui::section(name, about);
        for f in items {
            ui::check(
                tone_of(f),
                f.check,
                width,
                &ui::tilde(&f.detail),
                f.fix.as_deref().map(ui::tilde).as_deref(),
            );
        }
    }

    let fails = found.iter().filter(|f| f.level == Level::Fail).count();
    let warns = found.iter().filter(|f| f.level == Level::Warn).count();
    match (fails, warns) {
        (0, 0) => ui::verdict(ui::Tone::Good, "Everything checks out."),
        // Named plainly, because the whole point is that this state used to
        // look identical to a healthy one.
        (0, w) => ui::verdict(
            ui::Tone::Warn,
            &format!("Nothing broken. {w} thing(s) worth a look."),
        ),
        (f, _) => ui::verdict(
            ui::Tone::Bad,
            &format!(
                "{f} problem(s). {}",
                if cfg.url.is_empty() {
                    "Nothing syncs here."
                } else {
                    "Some of it is not syncing."
                }
            ),
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
            url_source: Source::ConfigFile,
            token_source: Source::CredentialsFile,
            credentials_file: Some("/h/.recall/credentials.json".into()),
            credentials_error: None,
            credentials_exposed: false,
            config_file: Some("/h/.recall/config.toml".into()),
            machine_source: Source::Unset,
            config_problems: Vec::new(),
            overridden: Vec::new(),
            server_ok: true,
            server_error: None,
            git_commit: Some("a1b2c3d".into()),
            server_version: Some("0.3.2".into()),
            server_channel: Some("release".into()),
            server_protocols: vec![1],
            min_client: Some("0.1.0".into()),
            client_version: "0.3.3".into(),
            merge_ready: true,
            merge_worker: false,
            merge_queue: None,
            merge_worker_seen_at: None,
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
    fn a_client_older_than_the_server_accepts_fails_and_says_to_upgrade() {
        let mut r = healthy();
        r.min_client = Some("0.4.0".into());
        let found = findings(&r);
        let v = found
            .iter()
            .find(|f| f.check == "version")
            .expect("a version finding");
        assert_eq!(v.level, Level::Fail);
        assert!(
            v.detail.contains("0.3.3") && v.detail.contains("0.4.0"),
            "{}",
            v.detail
        );
    }

    #[test]
    fn a_protocol_the_client_does_not_speak_fails() {
        let mut r = healthy();
        r.server_protocols = vec![2];
        let found = findings(&r);
        assert!(found
            .iter()
            .any(|f| f.check == "version" && f.level == Level::Fail));
    }

    /// A server too old to publish a discovery document says nothing about
    /// versions, and that is not a problem: it speaks protocol 1.
    #[test]
    fn a_server_without_discovery_raises_no_version_finding() {
        let mut r = healthy();
        r.server_version = None;
        r.server_channel = None;
        r.server_protocols = Vec::new();
        r.min_client = None;
        let found = findings(&r);
        assert!(!found.iter().any(|f| f.check == "version"));
        let server = found.iter().find(|f| f.check == "server").unwrap();
        assert!(server.detail.contains("a1b2c3d"), "{}", server.detail);
    }

    #[test]
    fn a_dev_server_is_named_as_one() {
        let mut r = healthy();
        r.server_version = Some("0.3.3-dev+ge100cfd".into());
        r.server_channel = Some("dev".into());
        let server = findings(&r)
            .into_iter()
            .find(|f| f.check == "server")
            .unwrap();
        assert!(
            server.detail.contains("not a release build"),
            "{}",
            server.detail
        );
    }

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

    /// A leftover shell variable deciding which machine this is, named with
    /// both values and a fix — but only a warning: the environment winning
    /// is the documented rule, and in a cloud session it is the point.
    #[test]
    fn an_environment_value_overriding_the_config_is_named() {
        let mut rep = healthy();
        rep.overridden = vec![crate::status::Override {
            variable: "RECALL_SOURCE_ENV",
            environment: "laptop".into(),
            setting: "machine.name",
            config: "jarvis".into(),
        }];
        let found = findings(&rep);
        let f = find(&found, "config overridden").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(
            f.detail.contains("laptop") && f.detail.contains("jarvis"),
            "{}",
            f.detail
        );
        assert!(f.fix.as_deref().unwrap().contains("RECALL_SOURCE_ENV"));
        assert_eq!(verdict(&found), exit::OK);
    }

    #[test]
    fn a_config_problem_is_a_warning_with_the_file_to_edit() {
        let mut rep = healthy();
        rep.config_problems = vec!["`machine.nmae` is not a setting Recall knows".into()];
        let found = findings(&rep);
        let f = find(&found, "config file").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.fix.as_deref().unwrap().contains("config.toml"));
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

    // ----------------------------------------------------------- merge queue

    fn with_worker(oldest: Option<String>, queued: u64) -> Report {
        let mut rep = healthy();
        rep.merge_worker = true;
        rep.merge_queue = Some(recall_wire::QueueStatus {
            queued,
            leased: 0,
            failed: 0,
            oldest_queued_at: oldest,
        });
        rep
    }

    fn stamp_minutes_ago(minutes: i64) -> String {
        let fmt = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        (time::OffsetDateTime::now_utc() - time::Duration::minutes(minutes))
            .format(&fmt)
            .unwrap()
    }

    #[test]
    fn no_worker_no_queue_finding() {
        assert!(find(&findings(&healthy()), "merge queue").is_none());
    }

    #[test]
    fn a_queue_that_drains_is_fine() {
        let found = findings(&with_worker(Some(stamp_minutes_ago(2)), 1));
        assert_eq!(find(&found, "merge queue").unwrap().level, Level::Ok);
        let found = findings(&with_worker(None, 0));
        assert_eq!(
            find(&found, "merge queue").unwrap().detail,
            "nothing waiting"
        );
    }

    /// The worker stopped: pushes keep landing, and the only sign is a job
    /// that keeps getting older.
    #[test]
    fn a_job_waiting_an_hour_is_a_warning() {
        let found = findings(&with_worker(Some(stamp_minutes_ago(61)), 3));
        let f = find(&found, "merge queue").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.starts_with("3 merges waiting"), "{}", f.detail);
        assert!(f.fix.as_deref().unwrap().contains("recall-worker"));
        assert_eq!(
            verdict(&found),
            exit::OK,
            "a warning never fails the command"
        );
    }

    /// A worker that stopped asking for work is not ready, whatever its
    /// last report of its CLI said.
    #[test]
    fn a_worker_that_stopped_asking_is_a_warning() {
        let mut rep = with_worker(None, 0);
        rep.merge_ready = true;
        rep.merge_worker_seen_at = Some(stamp_minutes_ago(1));
        assert!(worker_quiet(&rep).is_none());
        assert!(find(&findings(&rep), "merge").is_none());

        rep.merge_worker_seen_at = Some(stamp_minutes_ago(3));
        assert!(worker_quiet(&rep).is_some());
        let found = findings(&rep);
        let f = find(&found, "merge").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.contains("has not asked for work"), "{}", f.detail);
        assert!(f.fix.as_deref().unwrap().contains("logs recall-worker"));

        // Without a worker there is nothing to go quiet.
        rep.merge_worker = false;
        assert!(worker_quiet(&rep).is_none());
    }

    /// A failed merge is a warning however new: nothing retries it.
    #[test]
    fn a_failed_merge_is_a_warning() {
        let mut rep = with_worker(None, 0);
        assert!(find(&findings(&rep), "failed merges").is_none());
        rep.merge_queue.as_mut().unwrap().failed = 2;
        let found = findings(&rep);
        let f = find(&found, "failed merges").unwrap();
        assert_eq!(f.level, Level::Warn);
        assert!(f.detail.starts_with("2 merges failed"), "{}", f.detail);
        assert!(f.fix.as_deref().unwrap().contains("/v1/jobs"));
        // Also with no worker: a revoked worker's queue, failed here.
        rep.merge_worker = false;
        assert!(find(&findings(&rep), "failed merges").is_some());
    }

    #[test]
    fn a_worker_that_cannot_merge_names_the_worker() {
        let mut rep = with_worker(None, 0);
        rep.merge_ready = false;
        let found = findings(&rep);
        assert!(find(&found, "merge")
            .unwrap()
            .fix
            .as_deref()
            .unwrap()
            .contains("recall-worker claude setup-token"));
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

    // ---------------------------------------------------------------- Git Bash

    /// Exercises the real environment rather than a stand-in: GitHub's
    /// `windows-latest` runner is documented to ship Git for Windows, so this
    /// pins that assumption down where a change to the runner image would be
    /// noticed, instead of only being noticed by someone hitting the failure
    /// on their own machine. Not verified anywhere but CI — there is no
    /// Windows machine in the environment this was written in.
    #[cfg(windows)]
    #[test]
    fn git_bash_is_found_on_this_runner() {
        assert!(
            git_bash_available(),
            "expected Git for Windows on this runner; if the image changed, \
             widen the search in git_bash_available()"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_healthy_windows_report_includes_a_git_bash_finding() {
        let found = findings(&healthy());
        let f = find(&found, "Git Bash").expect("a Git Bash finding on Windows");
        // Whichever it says, it has to be one of the two — never silently
        // absent, which is the whole point of this check existing.
        assert!(matches!(f.level, Level::Ok | Level::Fail));
    }
}
