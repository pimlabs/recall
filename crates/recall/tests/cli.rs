//! Exit codes and stdio, exercised against the real binary.
//!
//! These are a contract, not an implementation detail: these commands run
//! as hooks inside someone's Claude Code session. A `pull` that exits
//! non-zero when the server is down would surface as a hook failure every
//! time a session starts — so "the server is unreachable" has to be a
//! silent, successful no-op, and only genuine misconfiguration is allowed
//! to be loud.
//!
//! Nothing here is covered by the library tests, which call the functions
//! directly and never see an exit code.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn binary() -> PathBuf {
    // Cargo builds integration-test binaries next to the crate's own.
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("recall")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the binary with a clean environment, so the developer's own
/// RECALL_* variables can't make a test pass or fail by accident.
fn run(args: &[&str], cwd: &Path, env: &[(&str, &str)], stdin: Option<&str>) -> Run {
    let mut cmd = Command::new(binary());
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", cwd.to_string_lossy().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().expect("failed to run the recall binary");
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", "git@github.com:acme/app.git"],
    ] {
        assert!(Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    dir
}

/// An address nothing is listening on, so the client's failure path runs
/// for real rather than being mocked.
const DEAD_SERVER: &str = "http://127.0.0.1:9";

// ---------------------------------------------------------------------------
// The contract that matters most
// ---------------------------------------------------------------------------

/// If this regresses, every session in a project with Recall wired starts
/// with a hook error — including sessions where the user is nowhere near a
/// network.
#[test]
fn pull_exits_zero_when_the_server_is_unreachable() {
    let repo = git_repo();
    let r = run(
        &["pull"],
        repo.path(),
        &[("RECALL_URL", DEAD_SERVER), ("RECALL_TOKEN", "t")],
        None,
    );
    assert_eq!(
        r.code, 0,
        "pull must not fail a session start\n{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("leaving local memory untouched"),
        "the user should still be told, on stderr: {:?}",
        r.stderr
    );
}

/// Same reasoning for a project where Recall simply isn't set up: an
/// unconfigured machine must still open sessions.
#[test]
fn pull_exits_zero_when_nothing_is_configured() {
    let repo = git_repo();
    let r = run(&["pull"], repo.path(), &[], None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("RECALL_URL"), "stderr: {:?}", r.stderr);
}

/// Push runs on every memory write. An unconfigured machine editing a file
/// that isn't a memory file must be a silent no-op — not an error per edit.
#[test]
fn push_is_a_silent_no_op_for_a_file_that_is_not_memory() {
    let repo = git_repo();
    let payload = format!(
        r#"{{"tool_input":{{"file_path":"{}/src/main.rs"}}}}"#,
        repo.path().to_string_lossy()
    );
    let r = run(&["push"], repo.path(), &[], Some(&payload));
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.is_empty(), "expected silence, got {:?}", r.stderr);
}

/// Hook payloads come from a tool we don't control. Garbage on stdin must
/// not produce noise in the session.
#[test]
fn push_ignores_a_malformed_hook_payload() {
    let repo = git_repo();
    for payload in ["", "not json", "{}", r#"{"tool_input":{}}"#, "[1,2,3]"] {
        let r = run(&["push"], repo.path(), &[], Some(payload));
        assert_eq!(r.code, 0, "payload {payload:?} -> stderr {}", r.stderr);
        assert!(
            r.stderr.is_empty(),
            "payload {payload:?} produced noise: {:?}",
            r.stderr
        );
    }
}

/// A directory holding a `hostname` shim that records having been run, and the
/// marker it writes. Returned together with a `PATH` that *prepends* the shim
/// rather than replacing the real one: `recall` shells out to `git` to find
/// the project root, and a test that broke that would pass for the wrong
/// reason.
fn hostname_shim() -> (tempfile::TempDir, PathBuf, String) {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("hostname-was-run");
    let script = dir.path().join("hostname");
    std::fs::write(
        &script,
        format!("#!/bin/sh\n: > '{}'\necho shimmed\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (dir, marker, path)
}

/// `push` is synchronous inside a Claude Code session and fires on every Edit
/// and Write, so anything it does before the "is this file even mine?" check
/// is done hundreds of times a session for nothing. Resolving configuration
/// is exactly that: `RECALL_SOURCE_ENV` falls back to the hostname, and that
/// fallback forks `hostname(1)`. A refactor that reads configuration up front
/// leaves no visible symptom — just a process spawned per keystroke-sized
/// edit — which is why it has to be caught here rather than noticed.
///
/// `RECALL_SOURCE_ENV` is deliberately absent from the environment below: set
/// it and the fallback never runs, and the test proves nothing.
#[test]
fn push_does_not_fork_hostname_before_deciding_a_file_is_not_its_business() {
    let repo = git_repo();
    let (_shim, marker, path) = hostname_shim();

    // The positive control, and it is not ceremony: without it this test
    // would pass just as happily if the shim were never reachable at all.
    // `status` does resolve configuration, so it must trip the marker.
    let r = run(&["status"], repo.path(), &[("PATH", &path)], None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        marker.exists(),
        "the shim did not run even where configuration *is* resolved, so \
         nothing below could have detected a fork: {}",
        r.stdout
    );
    std::fs::remove_file(&marker).unwrap();

    let payload = format!(
        r#"{{"tool_input":{{"file_path":"{}/src/main.rs"}}}}"#,
        repo.path().to_string_lossy()
    );
    let r = run(&["push"], repo.path(), &[("PATH", &path)], Some(&payload));
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        !marker.exists(),
        "push forked hostname(1) for a file it went on to ignore — that is \
         one spawned process per edit in every session"
    );
}

// ---------------------------------------------------------------------------
// Where being loud is correct
// ---------------------------------------------------------------------------

/// `init` edits a file the user is expected to commit, so it must refuse to
/// guess at a location when there is no repository to anchor to.
#[test]
fn init_refuses_outside_a_git_repository() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["init"], dir.path(), &[], None);
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("git repository"),
        "the message should say why: {:?}",
        r.stderr
    );
}

#[test]
fn init_is_idempotent_and_says_so() {
    let repo = git_repo();

    let first = run(&["init"], repo.path(), &[], None);
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);
    assert!(first.stdout.contains("wired hooks into"));

    let second = run(&["init"], repo.path(), &[], None);
    assert_eq!(second.code, 0);
    assert!(
        second.stdout.contains("already wired"),
        "a second run should not claim to have done work: {:?}",
        second.stdout
    );

    let settings =
        std::fs::read_to_string(repo.path().join(".claude").join("settings.json")).unwrap();
    assert_eq!(
        settings.matches("recall push").count(),
        1,
        "the hook was duplicated:\n{settings}"
    );
}

/// `init` has to work before the token is set — it is how a machine gets
/// wired in the first place — but it should say what is still missing.
#[test]
fn init_warns_about_unset_variables_without_failing() {
    let repo = git_repo();
    let r = run(&["init"], repo.path(), &[], None);
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("RECALL_URL"), "stdout: {}", r.stdout);
    assert!(r.stdout.contains("RECALL_TOKEN"), "stdout: {}", r.stdout);
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `status` is the command people run when something is wrong, so it has to
/// work when everything is wrong.
#[test]
fn status_reports_rather_than_fails_when_unconfigured() {
    let repo = git_repo();
    let r = run(&["status"], repo.path(), &[], None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.contains("acme/app"), "stdout: {}", r.stdout);
    assert!(r.stdout.contains("(unset)"), "stdout: {}", r.stdout);
    assert!(
        r.stdout.contains("hooks wired  : NO"),
        "an unwired project should say so plainly: {}",
        r.stdout
    );
}

#[test]
fn status_json_is_parseable_and_reports_an_unreachable_server() {
    let repo = git_repo();
    let r = run(
        &["status", "--json"],
        repo.path(),
        &[("RECALL_URL", DEAD_SERVER), ("RECALL_TOKEN", "t")],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let parsed: serde_json::Value = serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("--json did not emit JSON ({e}): {}", r.stdout));
    assert_eq!(parsed["project_key"], "acme/app");
    assert_eq!(parsed["server_ok"], false);
    assert!(
        parsed["server_error"].is_string(),
        "an unreachable server should be explained: {parsed}"
    );
}

/// Outside a repository there is no remote to derive from, so the key falls
/// back to the local path — and `status` must still run.
#[test]
fn status_works_outside_a_git_repository() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["status", "--json"], dir.path(), &[], None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert!(
        parsed["project_key"]
            .as_str()
            .unwrap()
            .starts_with("local:"),
        "expected a local: fallback key, got {parsed}"
    );
}

/// A declared key and a derived one are the same string on the wire, so the
/// report has to say which is in force — otherwise the only way to tell that
/// `RECALL_PROJECT_KEY` took effect is to go and look at the server.
#[test]
fn status_reports_a_declared_project_key_and_where_it_came_from() {
    let repo = git_repo();
    let r = run(
        &["status", "--json"],
        repo.path(),
        &[("RECALL_PROJECT_KEY", "Acme/Monorepo-Api")],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        parsed["project_key"], "acme/monorepo-api",
        "a declaration beats the git remote, normalised: {parsed}"
    );
    assert_eq!(parsed["project_key_source"], "declared");
    assert!(
        parsed.get("rejected_vars").is_none(),
        "nothing was refused, so nothing should be listed: {parsed}"
    );
}

/// The silent failure the source field exists for: a declaration Recall
/// cannot use is dropped and the key falls back, so without this the only
/// symptom is memory syncing to a bucket nobody asked for.
#[test]
fn status_says_when_a_declared_project_key_was_refused() {
    let repo = git_repo();
    let r = run(
        &["status"],
        repo.path(),
        &[("RECALL_PROJECT_KEY", "global:eko")],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("acme/app"),
        "the derived key still stands: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("SET BUT UNUSABLE"),
        "a refused declaration has to be visible: {}",
        r.stdout
    );
}

// ---------------------------------------------------------------------------
// status, against the environment a hook would actually see
// ---------------------------------------------------------------------------

/// `run` points `HOME` at the working directory, which for `git_repo()` makes
/// the user-level settings file the very same path as the project's committed
/// one — so a test naming a single layer would quietly be exercising two. The
/// tests below hand `HOME` somewhere else and keep the layer they name the
/// only one in play.
fn home_elsewhere() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Writes one of a project's settings files, returning its path exactly as
/// `status` will print it. Rebuilding the expected path a second way is how a
/// test ends up asserting nothing.
fn write_settings(repo: &Path, name: &str, body: &str) -> String {
    let dir = repo.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path.display().to_string()
}

/// `status --json`, parsed. A malformed document fails here, naming the
/// output, rather than as an `unwrap` panic five assertions later.
fn status_json(cwd: &Path, env: &[(&str, &str)]) -> serde_json::Value {
    let r = run(&["status", "--json"], cwd, env, None);
    assert_eq!(r.code, 0, "status must never fail: {}", r.stderr);
    serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("--json did not emit JSON ({e}): {}", r.stdout))
}

/// The `declared_env` entry for `name`, failing with the whole report when it
/// is absent — an index-out-of-bounds panic would not say what was reported
/// instead.
fn declared_entry<'a>(rep: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    rep["declared_env"]
        .as_array()
        .unwrap_or_else(|| panic!("nothing was reported as declared at all: {rep}"))
        .iter()
        .find(|var| var["name"] == name)
        .unwrap_or_else(|| panic!("{name} was not reported as declared: {rep}"))
}

/// The bug this whole layer exists for. Claude Code puts the `env` block of a
/// project's settings into every hook it spawns, so a project that commits
/// `RECALL_PROJECT_KEY` syncs under that key — while `status`, typed into a
/// shell Claude Code never touched, used to answer with the git-derived
/// `acme/app`. Someone chasing memories that never arrive would then be
/// reading a report describing a different bucket than the one their session
/// writes to, which is worse than having no report.
#[test]
fn status_reads_a_project_key_that_only_the_settings_file_declares() {
    let repo = git_repo();
    let file = write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/monorepo-api"}}"#,
    );
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let rep = status_json(repo.path(), &[("HOME", &home)]);
    assert_eq!(
        rep["project_key"], "acme/monorepo-api",
        "the committed declaration is what the hooks sync under, and nothing \
         in this shell said so: {rep}"
    );
    assert_eq!(rep["project_key_source"], "declared");

    let var = declared_entry(&rep, "RECALL_PROJECT_KEY");
    assert_eq!(
        var["file"], file,
        "the report has to name the file to go and edit: {rep}"
    );
    assert_eq!(
        var["shadows_shell"], false,
        "nothing in this shell was overridden, and saying otherwise sends \
         someone hunting an export that does not exist: {rep}"
    );
}

/// Settings and shell disagreeing is the case people actually hit, and the two
/// possible answers look equally plausible — so the direction is asserted both
/// ways round. Reporting the shell's value as the winner would have someone
/// "fix" their export and watch nothing change.
#[test]
fn a_settings_declaration_beats_the_shell_and_not_the_other_way_round() {
    let repo = git_repo();
    let file = write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/from-settings"}}"#,
    );
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let rep = status_json(
        repo.path(),
        &[
            ("HOME", &home),
            ("RECALL_PROJECT_KEY", "acme/from-the-shell"),
        ],
    );
    assert_eq!(
        rep["project_key"], "acme/from-settings",
        "the settings file is the layer that wins: {rep}"
    );
    assert_ne!(
        rep["project_key"], "acme/from-the-shell",
        "the shell value is replaced, not preferred: {rep}"
    );

    let var = declared_entry(&rep, "RECALL_PROJECT_KEY");
    assert_eq!(var["file"], file);
    assert_eq!(
        var["shadows_shell"], true,
        "a shell value that is set and not in effect is exactly the \
         disagreement worth naming: {rep}"
    );
}

/// `settings.local.json` is untracked, so it is where someone puts the value
/// that differs from the team's. Naming the committed file as the source would
/// send them editing a file they then have to un-edit before pushing.
#[test]
fn the_local_settings_file_wins_and_is_the_file_status_names() {
    let repo = git_repo();
    write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/committed"}}"#,
    );
    let local = write_settings(
        repo.path(),
        "settings.local.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/untracked"}}"#,
    );
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let rep = status_json(repo.path(), &[("HOME", &home)]);
    assert_eq!(
        rep["project_key"], "acme/untracked",
        "the higher-precedence file is the one in force: {rep}"
    );
    assert_eq!(
        declared_entry(&rep, "RECALL_PROJECT_KEY")["file"],
        local,
        "the report must point at the file that actually won: {rep}"
    );
}

/// A settings file that does not parse is one Claude Code cannot read either,
/// so nothing it declares reaches the hooks — and the file merely being
/// *present* is what makes that invisible. `status` is the command people run
/// when everything is wrong, so this is a finding it prints, never a reason to
/// fail.
#[test]
fn unreadable_settings_are_a_finding_rather_than_a_failure() {
    let repo = git_repo();
    let file = write_settings(repo.path(), "settings.json", "{ not json");
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let r = run(&["status", "--json"], repo.path(), &[("HOME", &home)], None);
    assert_eq!(
        r.code, 0,
        "a broken settings file must not take the diagnostic down with it: {}",
        r.stderr
    );

    let rep: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let unreadable = rep["unreadable_settings"]
        .as_array()
        .unwrap_or_else(|| panic!("a file that cannot be parsed was skipped silently: {rep}"));
    assert!(
        unreadable.iter().any(|f| *f == file),
        "the report has to name the file: {rep}"
    );
    assert_eq!(
        rep["hooks_wired"], false,
        "the same unparseable file is where hooks would have been found: {rep}"
    );
    assert_eq!(
        rep["project_key"], "acme/app",
        "with the declaration unreadable the key falls back to the remote: {rep}"
    );
}

/// The compound failure: Recall reads an empty value as unset, so the setting
/// is off — *and* the working value exported in this shell is hidden behind
/// the declaration. Either half alone is confusing; together they look like
/// global sync simply not existing.
#[test]
fn an_empty_declaration_turns_a_setting_off_and_hides_the_shell_value() {
    let repo = git_repo();
    write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_GLOBAL_KEY":""}}"#,
    );
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let rep = status_json(
        repo.path(),
        &[("HOME", &home), ("RECALL_GLOBAL_KEY", "eko")],
    );
    let var = declared_entry(&rep, "RECALL_GLOBAL_KEY");
    assert_eq!(
        var["empty"], true,
        "an empty declaration is a declaration, not an absence: {rep}"
    );
    assert_eq!(
        var["shadows_shell"], true,
        "the shell's usable value is the thing being hidden: {rep}"
    );
    assert!(
        rep.get("global_key").is_none(),
        "global scope is off, however much the shell exported: {rep}"
    );
}

/// The JSON is for scripts; the text is what someone pastes into a bug report
/// at 1am. Printing the winning value without the file it came from leaves the
/// next question — "then why is my export doing nothing?" — with nowhere to go.
#[test]
fn the_text_report_names_the_file_that_overrides_the_shell() {
    let repo = git_repo();
    let file = write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/from-settings"}}"#,
    );
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let r = run(
        &["status"],
        repo.path(),
        &[
            ("HOME", &home),
            ("RECALL_PROJECT_KEY", "acme/from-the-shell"),
        ],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains(&file),
        "the settings file has to appear by path: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("overrides the value set in this shell"),
        "the shell value being dead has to be said, not implied: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains(&format!("set by {file}")),
        "the project_key line should attribute the key to that file too: {}",
        r.stdout
    );
}

/// `Environment::discover` builds its file list as `[user, project, local]`,
/// and that argument order is the only thing making the user-level file the
/// lowest layer — write it `[project, local, user]` and every other test still
/// passes, because they exercise `from_files`, which takes the order as a
/// parameter. So this pins what `discover` itself chooses.
///
/// Both halves are load-bearing. The project winning on the key it also
/// declares is the precedence; `RECALL_GLOBAL_KEY` still arriving from the
/// home file is what proves the user layer is read at all, rather than never
/// opened — an ordering bug that skipped it would be invisible to the first
/// assertion alone.
#[test]
fn the_user_settings_file_is_the_lowest_layer_and_is_still_read() {
    let repo = git_repo();
    let home = home_elsewhere();
    let user_file = write_settings(
        home.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/from-home","RECALL_GLOBAL_KEY":"eko-home"}}"#,
    );
    let project_file = write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":"acme/from-the-project"}}"#,
    );
    let home = home.path().to_string_lossy().into_owned();

    let rep = status_json(repo.path(), &[("HOME", &home)]);
    assert_eq!(
        rep["project_key"], "acme/from-the-project",
        "the project's file sits above the user's: {rep}"
    );
    assert_eq!(
        declared_entry(&rep, "RECALL_PROJECT_KEY")["file"],
        project_file,
        "the winning layer is the one to name: {rep}"
    );

    assert_eq!(
        declared_entry(&rep, "RECALL_GLOBAL_KEY")["file"],
        user_file,
        "a variable no higher layer declares still comes from the user file, \
         which is how we know it was opened: {rep}"
    );
    assert_eq!(
        rep["global_key"], "global:eko-home",
        "and the value it supplies is actually in force, namespaced the way \
         any accepted global key is: {rep}"
    );
}

/// `"RECALL_PROJECT_KEY": 12345` is somebody plainly meaning to set the key.
/// A number cannot become an environment variable, so it sets nothing — and
/// before this was reported, `status` answered that with a confident derived
/// key and no remark at all, which is the report being wrong about the single
/// thing it was asked.
#[test]
fn a_non_string_declaration_is_reported_rather_than_silently_dropped() {
    let repo = git_repo();
    let file = write_settings(
        repo.path(),
        "settings.json",
        r#"{"env":{"RECALL_PROJECT_KEY":12345}}"#,
    );
    let home = home_elsewhere();
    let home = home.path().to_string_lossy().into_owned();

    let rep = status_json(repo.path(), &[("HOME", &home)]);
    let ignored = rep["ignored_env"]
        .as_array()
        .unwrap_or_else(|| panic!("a declaration that sets nothing went unremarked: {rep}"));
    assert!(
        ignored
            .iter()
            .any(|var| var["name"] == "RECALL_PROJECT_KEY" && var["file"] == file),
        "the report has to name both the variable and the file: {rep}"
    );

    let declared: Vec<&str> = rep["declared_env"]
        .as_array()
        .map(|vars| vars.iter().filter_map(|var| var["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        !declared.contains(&"RECALL_PROJECT_KEY"),
        "a variable that was never set must not also be reported as declared, \
         or the two halves of the report contradict each other: {rep}"
    );
    assert_eq!(
        rep["project_key"], "acme/app",
        "nothing was set, so the key is the derived one: {rep}"
    );
}

// ---------------------------------------------------------------------------
// promote
// ---------------------------------------------------------------------------

/// Every refusal below is decided before anything is sent or moved, so each
/// one exits 1 — the code reserved for what only the user can fix — and none
/// of them needs a server.
#[test]
fn promote_refuses_when_there_is_no_global_scope_to_promote_into() {
    let repo = git_repo();
    let r = run(
        &["promote", "user.md"],
        repo.path(),
        &[("RECALL_URL", DEAD_SERVER), ("RECALL_TOKEN", "t")],
        None,
    );
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("RECALL_GLOBAL_KEY"),
        "the refusal should name the variable to set: {}",
        r.stderr
    );
}

#[test]
fn promote_refuses_a_note_that_is_not_there() {
    let repo = git_repo();
    let r = run(
        &["promote", "topics/nothing-here.md"],
        repo.path(),
        &[
            ("RECALL_URL", DEAD_SERVER),
            ("RECALL_TOKEN", "t"),
            ("RECALL_GLOBAL_KEY", "eko"),
        ],
        None,
    );
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(r.stderr.contains("does not exist"), "stderr: {}", r.stderr);
}

/// The argument is joined onto the memory directory, so a traversing one has
/// to be refused after the join rather than reaching outside it.
#[test]
fn promote_refuses_a_path_that_climbs_out_of_the_memory_directory() {
    let repo = git_repo();
    let r = run(
        &["promote", "../../../../.ssh/id_rsa"],
        repo.path(),
        &[
            ("RECALL_URL", DEAD_SERVER),
            ("RECALL_TOKEN", "t"),
            ("RECALL_GLOBAL_KEY", "eko"),
        ],
        None,
    );
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("not a memory file"),
        "stderr: {}",
        r.stderr
    );
}

/// Unlike the hooks, `promote` is typed on purpose — so an unconfigured
/// machine is told, rather than quietly doing nothing.
#[test]
fn promote_is_loud_about_missing_configuration() {
    let repo = git_repo();
    let r = run(&["promote", "user.md"], repo.path(), &[], None);
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(r.stderr.contains("RECALL_URL"), "stderr: {}", r.stderr);
}

// ---------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------

/// Every command the CLI offers. A new one added without a line here is a
/// command the help tests below will not notice is missing from the help.
const COMMANDS: &[&str] = &[
    "init", "backfill", "promote", "status", "serve", "push", "pull", "version", "help",
];

#[test]
fn version_prints_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["version"], dir.path(), &[], None);
    assert_eq!(r.code, 0);
    // `scripts/release.sh` matches on this prefix to confirm the binary it
    // just built is the one being tagged, so the shape is a contract with the
    // release, not a formatting preference.
    assert!(r.stdout.starts_with("recall "), "stdout: {}", r.stdout);
}

/// Three ways to ask, one answer. The subcommand is the older surface and
/// cannot be dropped; `--version` is what everyone's fingers type and what
/// scripts reach for. Having both is fine. Having both disagree is the bug
/// class this project keeps finding in itself, so they are pinned together
/// rather than each to a literal.
#[test]
fn the_three_ways_to_ask_for_the_version_give_the_same_answer() {
    let dir = tempfile::tempdir().unwrap();

    let sub = run(&["version"], dir.path(), &[], None);
    let long = run(&["--version"], dir.path(), &[], None);
    let short = run(&["-V"], dir.path(), &[], None);

    for (name, r) in [("version", &sub), ("--version", &long), ("-V", &short)] {
        assert_eq!(r.code, 0, "{name} exited {}: {}", r.code, r.stderr);
    }
    assert_eq!(
        long.stdout, sub.stdout,
        "`--version` and `version` disagree, so one of them is lying"
    );
    assert_eq!(short.stdout, sub.stdout, "`-V` drifted from the other two");
}

/// Help has to be reachable by every reflex someone might have, and has to
/// name everything it can do. A command that exists but is absent from the
/// help is a command nobody finds.
#[test]
fn help_answers_to_every_form_and_names_every_command() {
    let dir = tempfile::tempdir().unwrap();

    for form in [vec!["--help"], vec!["-h"], vec!["help"]] {
        let r = run(&form, dir.path(), &[], None);
        assert_eq!(r.code, 0, "{form:?} exited {}: {}", r.code, r.stderr);
        for command in COMMANDS {
            assert!(
                r.stdout.contains(command),
                "{form:?} does not mention `{command}`: {}",
                r.stdout
            );
        }
    }
    assert!(
        run(&["--help"], dir.path(), &[], None)
            .stdout
            .contains("--version"),
        "the version flag should be discoverable from the help that mentions it"
    );
}

/// Per-command help, both ways round. `recall help status` and `recall status
/// --help` are the same question asked by people with different habits.
#[test]
fn per_command_help_works_from_either_direction() {
    let dir = tempfile::tempdir().unwrap();

    let before = run(&["help", "status"], dir.path(), &[], None);
    let after = run(&["status", "--help"], dir.path(), &[], None);

    assert_eq!(before.code, 0, "stderr: {}", before.stderr);
    assert_eq!(after.code, 0, "stderr: {}", after.stderr);
    assert_eq!(before.stdout, after.stdout);
    assert!(
        before.stdout.contains("--json"),
        "a command's own flags belong in its help: {}",
        before.stdout
    );
}

/// Running the bare binary is a question, not an instruction, and answering
/// it with silence and success would be the wrong answer to both halves.
#[test]
fn no_arguments_prints_help_and_does_not_look_successful() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&[], dir.path(), &[], None);

    assert_ne!(r.code, 0, "a bare `recall` should not read as success");
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        combined.contains("Usage") && combined.contains("init"),
        "it should print the help it is refusing to guess at: {combined}"
    );
}

// ---------------------------------------------------------------------------
// backfill
// ---------------------------------------------------------------------------

/// `backfill` is typed on purpose, so an unconfigured machine is told rather
/// than quietly doing nothing — the opposite of the hooks, which must never
/// be the reason a session breaks. Exit 1 is the code reserved for what only
/// the user can fix.
#[test]
fn backfill_is_loud_about_missing_configuration() {
    let repo = git_repo();
    let r = run(&["backfill"], repo.path(), &[], None);

    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("RECALL_URL"),
        "the refusal should name the variable to set: {}",
        r.stderr
    );
}

/// A backfill against an unreachable server exits 2, not 0. The hooks
/// swallow that case deliberately, and this one must not: someone who typed
/// this is waiting to be told their memory is safe, and silence would read
/// as yes.
#[test]
fn backfill_exits_two_when_the_server_cannot_be_reached() {
    let repo = git_repo();
    std::fs::create_dir_all(repo.path().join(".claude")).unwrap();
    let r = run(
        &["backfill"],
        repo.path(),
        &[("RECALL_URL", DEAD_SERVER), ("RECALL_TOKEN", "t")],
        None,
    );

    assert_eq!(r.code, 2, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        !r.stderr.is_empty(),
        "an unreachable server has to say so rather than exit quietly"
    );
    assert!(
        !r.stdout.contains(" sent,"),
        "nothing was sent, so it must not print a summary that implies it was: {}",
        r.stdout
    );
}

#[test]
fn an_unknown_subcommand_fails_with_usage() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["definitely-not-a-command"], dir.path(), &[], None);
    assert_ne!(r.code, 0, "an unknown command should not look successful");
    let combined = format!("{}{}", r.stdout, r.stderr);
    assert!(
        combined.contains("recall") && combined.contains("init"),
        "the failure should point at the real commands: {combined}"
    );
}

/// `serve` is the one command that must refuse to start misconfigured: a
/// server reachable from the internet with no token is not a degraded mode.
#[test]
fn serve_refuses_to_start_without_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(
        &["serve"],
        dir.path(),
        &[("RECALL_DB_PATH", &dir.path().join("x.db").to_string_lossy())],
        None,
    );
    assert_ne!(r.code, 0, "started with no auth");
    assert!(
        r.stderr.contains("RECALL_TOKEN"),
        "the message should name the variable: {:?}",
        r.stderr
    );
}
