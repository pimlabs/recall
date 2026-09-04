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

// ---------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------

#[test]
fn version_prints_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["version"], dir.path(), &[], None);
    assert_eq!(r.code, 0);
    assert!(r.stdout.starts_with("recall "), "stdout: {}", r.stdout);
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
