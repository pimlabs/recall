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

/// The PostToolUse payload Claude Code sends for an edit to `file`.
///
/// Built as JSON rather than spliced into a string: a Windows path is full
/// of backslashes, which a hand-assembled string leaves as invalid escapes.
/// The hook then reads a malformed payload and stays silent, so a test
/// expecting silence passes for the wrong reason and one expecting a
/// message fails without saying why.
fn hook_payload(file: &Path) -> String {
    serde_json::json!({ "tool_input": { "file_path": file } }).to_string()
}

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
    let mut cmd = command(args, cwd, env);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

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

/// The binary with a clean environment, as [`run`] runs it, for a test that
/// has to talk to it while it runs.
fn command(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(binary());
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", cwd.to_string_lossy().to_string());
    // env_clear() strips everything, and on Windows that includes the
    // variables CreateProcess itself needs just to start a process at all —
    // without `SystemRoot` in particular, spawning can fail before the
    // binary under test ever runs. These are OS plumbing, never RECALL_* or
    // anything a test reads, so the "clean environment" property below is
    // unaffected. Not verified on a real Windows machine; if the windows CI
    // job's `cargo test -p recall` fails at `spawn()` rather than in an
    // assertion, this list is the first thing to widen.
    #[cfg(windows)]
    for var in ["SystemRoot", "windir", "TEMP", "TMP", "LOCALAPPDATA"] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd
}

/// A temporary git repository, and the path the binary will call it.
///
/// Those are not always the same string, which is the whole reason this type
/// exists rather than a bare `TempDir`. `recall` finds its project root with
/// `git rev-parse --show-toplevel`, and git resolves symlinks. On macOS the
/// per-user temporary directory lives under `/var`, which is a symlink to
/// `/private/var` — so `TempDir::path()` says `/var/folders/…` while every
/// path the binary prints says `/private/var/folders/…`, and a test naming a
/// file compares one spelling against the other.
///
/// It fails only on macOS. The Linux runner's `/tmp` is a real directory, so
/// both spellings agree there and CI stayed green while seven tests could not
/// pass on the machine this project is developed on.
///
/// Windows has the same shape of problem for a different reason:
/// `canonicalize()` always returns the verbatim `\\?\C:\...` form, which
/// `git rev-parse --show-toplevel` never produces — [`strip_verbatim_prefix`]
/// is this platform's half of what resolving symlinks is on macOS.
///
/// `home_elsewhere()` deliberately stays a plain `TempDir`: `HOME` is read
/// from the environment verbatim and never goes through git, so the files
/// under it really are reported with the unresolved spelling.
struct Repo {
    /// Held for its `Drop`, which removes the directory. Never read — reading
    /// it is the mistake this type exists to make impossible.
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Repo {
    /// Where the repository is, spelled the way the binary will spell it.
    fn path(&self) -> &Path {
        &self.path
    }
}

fn git_repo() -> Repo {
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
    // Resolved once, here, rather than at each assertion: rebuilding an
    // expected path a second way is how a test ends up asserting nothing.
    let path = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let path = strip_verbatim_prefix(path);
    Repo { _dir: dir, path }
}

/// `\\?\C:\...` back to `C:\...`, the "dunce" trick, inlined rather than
/// taken as a dependency for four lines.
///
/// `canonicalize()` on Windows always returns the verbatim form — there is
/// no flag to opt out — but the binary under test never produces one: `git
/// rev-parse --show-toplevel` doesn't emit it, and nothing downstream adds
/// it. Left unstripped, every expected path built from this helper carries
/// a prefix the real output never has, and — the sharper edge — `Path::join`
/// on a verbatim path does not treat `/` as a separator at all, so a
/// `.join("a/b/c")` (several of these tests join a single string containing
/// its own separators) becomes one bizarre component instead of three.
/// A no-op on every other platform.
#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(s) => match s.strip_prefix(r"\\?\") {
            // `\\?\UNC\server\share\...` is verbatim for a UNC path, and
            // stripping only the `\\?\` would leave `UNC\...`, not a share
            // path (`\\server\share\...`) — a temp directory is never one,
            // so this is unreached in practice, but wrong is wrong.
            Some(rest) if !rest.starts_with(r"UNC\") => PathBuf::from(rest),
            _ => path,
        },
        None => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// The guard for the above, and it is filesystem-independent on purpose: it
/// asks git what it thinks the root is and compares that with what the tests
/// build their expectations from. On Linux both answers are the unresolved
/// path and this passes trivially; on macOS they differ unless `git_repo()`
/// resolves, which is exactly the bug.
///
/// On Windows a second, legitimate difference joins the comparison: git
/// always prints `/`-separated paths, and `project::root()` in the binary
/// turns those into `\` before doing anything else with them — see
/// `project.rs::git_toplevel`. `git_says` gets the identical transform here,
/// so this stays a guard on the *fixture* rather than growing a second copy
/// of what the binary does and silently passing if the two drifted apart.
#[test]
fn the_repo_helper_agrees_with_git_about_where_the_repo_is() {
    let repo = git_repo();
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-parse failed in the fixture");
    let git_says = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let git_says = native_separators(git_says);
    assert_eq!(
        repo.path().display().to_string(),
        git_says,
        "the tests build expected paths from one spelling and the binary \
         reports another, so every assertion naming a path is comparing two \
         different strings"
    );
}

/// What `project::root()` does to git's output before anything else touches
/// it — duplicated here rather than imported, because `recall` has no
/// library target for an integration test to link against; see the module
/// doc.
#[cfg(windows)]
fn native_separators(path: String) -> String {
    path.replace('/', "\\")
}

#[cfg(not(windows))]
fn native_separators(path: String) -> String {
    path
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
    let payload = hook_payload(&repo.path().join("src/main.rs"));
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
///
/// Unix only: an extensionless `#!/bin/sh` script made executable with
/// `chmod` is not something Windows can run by that name at all — it would
/// need a `.exe`/`.cmd`/`.bat` extension and a completely different
/// mechanism. The fork this guards against (`Command::new("hostname")` in
/// `config.rs`) is not itself gated, so this is a gap in the test's own
/// technique, not a known gap in the behaviour under test.
#[cfg(unix)]
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
#[cfg(unix)]
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

    let payload = hook_payload(&repo.path().join("src/main.rs"));
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
    "init", "backfill", "promote", "status", "doctor", "push", "pull", "version", "help",
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

/// `recall serve` moved to its own binary in 0.4.0. Anyone who still types
/// it is told where it went, rather than handed clap's "unrecognized
/// subcommand", and it fails: a deployment still starting it must not look
/// healthy.
#[test]
fn serve_says_the_server_moved_to_recall_server() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["serve"], dir.path(), &[], None);
    assert_eq!(r.code, 1, "stdout: {} stderr: {}", r.stdout, r.stderr);
    assert!(
        r.stderr.contains("recall-server"),
        "the message should name the new binary: {:?}",
        r.stderr
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// The pair's whole point, asserted as a pair: on the same unconfigured
/// project, `status` exits 0 and `doctor` does not. Recall shipped for
/// months with only the first, and an environment that never synced anything
/// was indistinguishable from one with nothing new to sync.
#[test]
fn doctor_exits_non_zero_where_status_exits_zero() {
    let repo = git_repo();

    let status = run(&["status"], repo.path(), &[], None);
    let doctor = run(&["doctor"], repo.path(), &[], None);

    assert_eq!(status.code, 0, "status must stay informational");
    assert_eq!(
        doctor.code, 1,
        "doctor must fail an unconfigured project: {}",
        doctor.stdout
    );
}

/// A failure nobody can act on trains the reader to skip the output, so the
/// fix is part of the contract rather than a nicety.
#[test]
fn doctor_names_what_is_missing_and_what_to_do() {
    let repo = git_repo();
    let r = run(&["doctor"], repo.path(), &[], None);

    assert!(r.stdout.contains("RECALL_URL"), "stdout: {}", r.stdout);
    assert!(r.stdout.contains("RECALL_TOKEN"), "stdout: {}", r.stdout);
    assert!(
        r.stdout.contains("recall init"),
        "an unwired project should be told the command that wires it: {}",
        r.stdout
    );
}

/// In a remote session an unset memory dir is not a nuance: Claude Code's
/// auto-memory is off entirely, so nothing syncs however correct the rest
/// is. The fix has to carry the value, which is not `$HOME` and used to
/// appear nowhere but a ROADMAP checkbox.
#[test]
fn doctor_fails_a_remote_session_with_no_memory_dir() {
    let repo = git_repo();
    let r = run(
        &["doctor"],
        repo.path(),
        &[("CLAUDE_CODE_REMOTE", "true")],
        None,
    );

    assert_eq!(r.code, 1, "stdout: {}", r.stdout);
    assert!(
        r.stdout.contains("✗ CLAUDE_CODE_REMOTE_MEMORY_DIR"),
        "stdout: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("/home/user/.claude"),
        "the value has to be in the output: {}",
        r.stdout
    );
}

/// The same state on a laptop is correct, and saying so every time is the
/// noise that teaches someone to stop reading the report.
#[test]
fn doctor_says_nothing_about_the_memory_dir_outside_a_remote_session() {
    let repo = git_repo();
    let r = run(&["doctor"], repo.path(), &[], None);

    assert!(
        !r.stdout.contains("✗ CLAUDE_CODE_REMOTE_MEMORY_DIR")
            && !r.stdout.contains("! CLAUDE_CODE_REMOTE_MEMORY_DIR"),
        "stdout: {}",
        r.stdout
    );
}

/// Checking your connection from a directory that is not a project is an
/// ordinary thing to do, and the first version of this exited 1 for it —
/// because hooks are not wired there, which is true and not a problem.
#[test]
fn doctor_does_not_fail_on_unwired_hooks_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["doctor", "--json"], dir.path(), &[], None);

    let parsed: serde_json::Value = serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("--json did not emit JSON ({e}): {}", r.stdout));
    let hooks = parsed
        .as_array()
        .expect("doctor --json emits an array")
        .iter()
        .find(|f| f["check"] == "hooks")
        .expect("hooks is checked");

    assert_eq!(
        hooks["level"], "ok",
        "outside a repository there is nothing to wire: {hooks}"
    );
}

#[test]
fn doctor_json_is_parseable_and_carries_the_levels() {
    let repo = git_repo();
    let r = run(&["doctor", "--json"], repo.path(), &[], None);

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);

    let parsed: serde_json::Value = serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("--json did not emit JSON ({e}): {}", r.stdout));
    let found = parsed.as_array().expect("doctor --json emits an array");

    let url = found
        .iter()
        .find(|f| f["check"] == "RECALL_URL")
        .expect("RECALL_URL is checked");
    assert_eq!(url["level"], "fail");
    assert!(url["fix"].is_string(), "a fail carries a fix: {url}");
}

/// Exercised through the real binary because the unit tests construct a
/// `Report` by hand: this is the one place the collecting and the judging
/// are proven to meet.
#[test]
fn doctor_passes_a_wired_project_that_can_reach_a_server() {
    let repo = git_repo();
    let init = run(&["init"], repo.path(), &[], None);
    assert_eq!(init.code, 0, "stderr: {}", init.stderr);

    let r = run(
        &["doctor"],
        repo.path(),
        &[("RECALL_URL", DEAD_SERVER), ("RECALL_TOKEN", "t")],
        None,
    );

    // The server is deliberately dead, so this still fails — but on the
    // server, and no longer on the three things `init` and the environment
    // just supplied.
    assert!(
        r.stdout.contains("✓ hooks"),
        "init wired the hooks, doctor should see it: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("✓ RECALL_URL") && r.stdout.contains("✓ RECALL_TOKEN"),
        "both variables were set: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("✗ server"),
        "and the dead server is what is left: {}",
        r.stdout
    );
}

// ---------------------------------------------------------------------------
// connect / disconnect, and the credentials file the other commands read
// ---------------------------------------------------------------------------

/// A `RECALL_HOME` holding a credentials file, as `recall connect` would
/// have left it. Written by hand rather than through `connect`, which needs
/// a terminal the tests do not have.
fn recall_home_with(servers: &[(&str, &str)], server: &str) -> tempfile::TempDir {
    recall_home_named(servers, server, None)
}

/// The same, with a machine name in `config.toml` as well.
fn recall_home_named(
    servers: &[(&str, &str)],
    server: &str,
    name: Option<&str>,
) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut creds = String::from("version = 1\n");
    for (url, token) in servers {
        creds.push_str(&format!("\n[servers.\"{url}\"]\ntoken = \"{token}\"\n"));
    }
    let creds_path = dir.path().join("credentials.toml");
    std::fs::write(&creds_path, creds).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let mut config = String::from("version = 1\n");
    if !server.is_empty() {
        config.push_str(&format!("server = \"{server}\"\n"));
    }
    if let Some(name) = name {
        config.push_str(&format!("\n[machine]\nname = \"{name}\"\n"));
    }
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    dir
}

/// 0.3.0 wrote `credentials.json`. The first command to read configuration
/// moves it into the two TOML files and removes it — through the binary,
/// because the migration lives in the CLI's resolution, not the library.
#[test]
fn a_0_3_0_credentials_file_is_migrated_by_the_first_command() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let legacy = home.path().join("credentials.json");
    std::fs::write(
        &legacy,
        format!(
            r#"{{"version":1,"default":"{DEAD_SERVER}","servers":{{"{DEAD_SERVER}":{{"token":"t"}}}}}}"#
        ),
    )
    .unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let rep = status_json(repo.path(), &[("RECALL_HOME", &home_str)]);

    assert!(!legacy.exists(), "the old file is gone");
    assert_eq!(rep["url_source"], "config_file", "{rep}");
    assert_eq!(rep["token_source"], "credentials_file", "{rep}");
    let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(
        config.contains(&format!("server = \"{DEAD_SERVER}\"")),
        "{config}"
    );
}

/// One name in `config.toml`, both uses — through the binary.
#[test]
fn the_machine_name_in_config_turns_on_the_machine_scope() {
    let repo = git_repo();
    let home = recall_home_named(&[(DEAD_SERVER, "t")], DEAD_SERVER, Some("jarvis"));
    let home_str = home.path().to_string_lossy().to_string();

    let rep = status_json(repo.path(), &[("RECALL_HOME", &home_str)]);
    assert_eq!(rep["machine_key"], "machine:jarvis", "{rep}");
    assert_eq!(rep["machine_source"], "config_file", "{rep}");

    // A leftover shell label that disagrees is reported, with both values.
    let rep = status_json(
        repo.path(),
        &[("RECALL_HOME", &home_str), ("RECALL_SOURCE_ENV", "laptop")],
    );
    let o = &rep["overridden"][0];
    assert_eq!(o["variable"], "RECALL_SOURCE_ENV", "{rep}");
    assert_eq!(o["environment"], "laptop");
    assert_eq!(o["config"], "jarvis");

    // One that agrees is not.
    let rep = status_json(
        repo.path(),
        &[
            ("RECALL_HOME", &home_str),
            ("RECALL_MACHINE_KEY", "machine:jarvis"),
        ],
    );
    assert!(rep.get("overridden").is_none(), "{rep}");
}

/// A file written in an ephemeral container evaporates with it, and a
/// command that appears to succeed there is worse than one that declines.
#[test]
fn connect_refuses_in_a_remote_session_and_writes_nothing() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "https://recall.example.com"],
        repo.path(),
        &[("CLAUDE_CODE_REMOTE", "true"), ("RECALL_HOME", &home_str)],
        None,
    );

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    // Named, because the terminal check further on would also refuse here —
    // and a test passing for that reason proves nothing about this one.
    assert!(
        r.stderr.contains("remote session"),
        "refused for being remote: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains("RECALL_TOKEN"),
        "and says what to do instead: {}",
        r.stderr
    );
    assert!(!home.path().join("credentials.toml").exists());
}

/// The token is read from a terminal and nowhere else. Piped stdin is not a
/// terminal, and must not become a quiet second way in.
#[test]
fn connect_without_a_terminal_refuses_and_writes_nothing() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "https://recall.example.com"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        Some("s3cret\n"),
    );

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("terminal"), "stderr: {}", r.stderr);
    assert!(!home.path().join("credentials.toml").exists());
}

#[test]
fn connect_refuses_something_that_is_not_a_server_url() {
    let repo = git_repo();
    let r = run(&["connect", "recall.example.com"], repo.path(), &[], None);
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("not a server URL"),
        "stderr: {}",
        r.stderr
    );
}

/// A file this build cannot read is never overwritten — `connect` stops
/// before asking for a secret.
#[test]
fn connect_will_not_overwrite_a_credentials_file_it_cannot_read() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("credentials.toml");
    std::fs::write(&path, "{ this is not toml").unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "https://recall.example.com"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("Nothing was changed"),
        "stderr: {}",
        r.stderr
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ this is not toml"
    );
}

/// `recall connect` is a complete setup: with nothing in the environment,
/// status reports both values and says where they came from.
#[test]
fn status_reads_the_url_and_token_that_connect_saved() {
    let repo = git_repo();
    let home = recall_home_with(&[(DEAD_SERVER, "t")], DEAD_SERVER);
    let home_str = home.path().to_string_lossy().to_string();

    let rep = status_json(repo.path(), &[("RECALL_HOME", &home_str)]);

    assert_eq!(rep["url_set"], true, "{rep}");
    assert_eq!(rep["token_set"], true, "{rep}");
    assert_eq!(rep["url_source"], "config_file");
    assert_eq!(rep["token_source"], "credentials_file");
    assert_eq!(rep["credentials_exposed"], false);
}

/// Below the environment, never above it.
#[test]
fn an_exported_token_still_wins_over_the_saved_one() {
    let repo = git_repo();
    let home = recall_home_with(&[(DEAD_SERVER, "saved")], DEAD_SERVER);
    let home_str = home.path().to_string_lossy().to_string();

    let rep = status_json(
        repo.path(),
        &[("RECALL_HOME", &home_str), ("RECALL_TOKEN", "exported")],
    );
    assert_eq!(rep["token_source"], "environment", "{rep}");
    assert_eq!(rep["url_source"], "config_file", "{rep}");
}

/// The migration nudge, through the real binary: a shell token on a laptop
/// is a warning, and the warning carries the command.
#[test]
fn doctor_suggests_connect_for_a_shell_token_on_a_laptop() {
    let repo = git_repo();
    let r = run(
        &["doctor"],
        repo.path(),
        &[("RECALL_URL", DEAD_SERVER), ("RECALL_TOKEN", "t")],
        None,
    );
    assert!(r.stdout.contains("! token storage"), "stdout: {}", r.stdout);
    assert!(r.stdout.contains("recall connect"), "stdout: {}", r.stdout);
}

/// Removing the saved copy is all `disconnect` can do, and it must not
/// imply more: the shell still supplies a token, and the report says so
/// without naming a profile it cannot see.
#[test]
fn disconnect_removes_the_saved_token_and_is_honest_about_the_shell() {
    let repo = git_repo();
    let home = recall_home_with(
        &[("https://recall.example.com", "t")],
        "https://recall.example.com",
    );
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["disconnect"],
        repo.path(),
        &[("RECALL_HOME", &home_str), ("RECALL_TOKEN", "exported")],
        None,
    );

    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        !home.path().join("credentials.toml").exists(),
        "the last server gone, the file goes too"
    );
    assert!(
        r.stdout
            .contains("Removed the token for https://recall.example.com"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("still works on the server"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("Your shell still supplies RECALL_TOKEN")
            && r.stdout.contains("Remove it from your shell profile"),
        "{}",
        r.stdout
    );
    assert!(
        !r.stdout.contains(".zshrc") && !r.stdout.contains(".zprofile"),
        "no guessing: {}",
        r.stdout
    );
}

/// With two servers saved and neither named, there is no right guess.
#[test]
fn disconnect_asks_which_when_it_cannot_tell() {
    let repo = git_repo();
    // Two servers saved, and the config naming neither.
    let home = recall_home_with(
        &[
            ("https://a.example.com", "ta"),
            ("https://b.example.com", "tb"),
        ],
        "",
    );
    let path = home.path().join("credentials.toml");
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["disconnect"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("recall disconnect https://a.example.com"),
        "{}",
        r.stderr
    );
    assert!(path.exists(), "nothing was removed");
}

// ---------------------------------------------------------------------------
// push, standing in the wrong project
// ---------------------------------------------------------------------------

/// The failure this was written for, reproduced through the binary: a memory
/// file belonging to another project under the same Claude root. It happens
/// in a git worktree, whose project root — and therefore whose memory
/// directory — differs even though `project_key` does not, because that comes
/// from the git remote a worktree shares.
///
/// Three real edits were lost to this. The hook exited 0 without a word, and
/// the next `recall pull` restored the server's older copy over them.
#[test]
fn push_says_so_when_the_file_is_another_projects_memory() {
    let repo = git_repo();
    let foreign = repo
        .path()
        .join(".claude/projects/-somewhere-else/memory/note.md");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, "# note\n").unwrap();

    let payload = hook_payload(&foreign);
    let r = run(&["push"], repo.path(), &[], Some(&payload));

    // Still exit 0 — a hook must never be the reason a session breaks.
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("-somewhere-else"),
        "the slug has to be named, or the reader cannot tell which project: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains("nothing was pushed"),
        "and it has to say nothing happened: {}",
        r.stderr
    );
}

/// The other half, which must stay silent. A push hook that comments on
/// every unrelated file touched in a session is one nobody reads.
#[test]
fn push_stays_silent_about_an_ordinary_file() {
    let repo = git_repo();
    let source = repo.path().join("src/main.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();

    let payload = hook_payload(&source);
    let r = run(&["push"], repo.path(), &[], Some(&payload));

    assert_eq!(r.code, 0);
    assert_eq!(r.stderr.trim(), "", "an ordinary edit is not worth a word");
}

// ---------------------------------------------------------------------------
// connect as one flow, against a real server
// ---------------------------------------------------------------------------

/// A real server on a free port, in this process, stopped on drop.
///
/// The server is its own binary since 0.4.0 and this crate no longer builds
/// it, so it is started from the library instead: the same router and
/// store `recall-server` runs, without a process to find.
struct LiveServer {
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _db: tempfile::TempDir,
    /// The same configuration and store the running server has, for a test
    /// to act on them directly: sweeping idle devices now, rather than after
    /// the day a real deployment waits.
    cfg: recall_server::Config,
    store: std::sync::Arc<recall_server::Store>,
}

impl LiveServer {
    /// Removes every ephemeral device at once, as the server's own sweep
    /// does once one has been idle for `RECALL_EPHEMERAL_DEVICE_TTL_HOURS`.
    fn sweep_every_ephemeral_device(&self) {
        let cfg = recall_server::Config {
            ephemeral_device_ttl: std::time::Duration::ZERO,
            ..self.cfg.clone()
        };
        // Past the millisecond the last request was seen in, so "idle
        // before now" covers it.
        std::thread::sleep(std::time::Duration::from_millis(5));
        recall_server::Server::new(cfg, self.store.clone())
            .sweep_devices()
            .unwrap();
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn live_server(token: &str) -> LiveServer {
    live_server_started(token, true)
}

/// A server that has only just started, so for its first few seconds it
/// refuses every signature, as a deployed one does after each deploy.
fn live_server_just_started(token: &str) -> LiveServer {
    live_server_started(token, false)
}

/// Every other test's server acts as if it started a minute ago, so that
/// requests are signed and accepted at once.
fn live_server_started(token: &str, a_minute_ago: bool) -> LiveServer {
    let db = tempfile::tempdir().unwrap();
    let cfg = recall_server::Config {
        token: token.to_string(),
        db_path: db.path().join("recall.db").to_string_lossy().to_string(),
        merge_enabled: false,
        ..Default::default()
    };
    let store = std::sync::Arc::new(recall_server::Store::open(&cfg.db_path).expect("store opens"));
    let (kept_cfg, kept_store) = (cfg.clone(), store.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let server = recall_server::Server::new(cfg, store);
                if a_minute_ago {
                    server.backdate_start(60);
                }
                server
                    .serve_with_shutdown(listener, async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
    });
    LiveServer {
        url: format!("http://127.0.0.1:{port}"),
        stop: Some(stop),
        thread: Some(thread),
        _db: db,
        cfg: kept_cfg,
        store: kept_store,
    }
}

/// The whole flow with nobody at the keyboard: a saved token that still
/// works is kept rather than asked for, the name comes from `--name`, and
/// `--yes` wires the project — so a machine can be set up by a script once
/// it holds a token.
#[test]
fn connect_with_a_working_saved_token_sets_the_machine_up_without_asking() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );

    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Saved token OK"), "stderr: {}", r.stderr);
    let config = std::fs::read_to_string(home.path().join("config.toml")).unwrap();
    assert!(
        config.contains("name = \"jarvis\""),
        "config.toml: {config}"
    );
    assert!(
        config.contains(&format!("server = \"{}\"", server.url)),
        "config.toml: {config}"
    );
    let settings =
        std::fs::read_to_string(repo.path().join(".claude").join("settings.json")).unwrap();
    assert!(settings.contains("recall push"), "wired: {settings}");
    // No memory here yet, so there is no first sync to offer.
    assert!(!r.stderr.contains("Uploaded"), "stderr: {}", r.stderr);

    // And a second run finds nothing left to do in the project.
    let again = run(
        &["connect", "--yes"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(again.code, 0, "stderr: {}", again.stderr);
    assert!(
        again.stderr.contains("already syncs"),
        "stderr: {}",
        again.stderr
    );
    assert!(
        again.stderr.contains("jarvis"),
        "keeps the name: {}",
        again.stderr
    );
}

/// Wired by `connect`, memory that predates Recall is offered its first
/// sync — and with `--yes`, sent.
#[test]
fn connect_sends_existing_memory_when_it_wires_a_project() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];

    let memory_dir = status_json(repo.path(), &env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        Path::new(&memory_dir).join("fact.md"),
        "---\nname: fact\ndescription: a fact\n---\n\nA fact.\n",
    )
    .unwrap();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &env,
        None,
    );

    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Uploaded"), "stderr: {}", r.stderr);
    assert!(!r.stderr.contains("Uploaded 0"), "stderr: {}", r.stderr);
}

/// A saved token the server now rejects needs a person to type a new one.
/// Without a terminal that is a refusal, and nothing is rewritten.
#[test]
fn connect_with_a_rejected_saved_token_and_no_terminal_changes_nothing() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "stale")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let before = std::fs::read_to_string(home.path().join("config.toml")).unwrap();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("needs a new token"),
        "stderr: {}",
        r.stderr
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
        before
    );
    assert!(!repo.path().join(".claude").join("settings.json").exists());
}

#[test]
fn connect_with_no_server_named_or_saved_says_to_name_one() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("no server named"), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("recall connect https://"),
        "stderr: {}",
        r.stderr
    );
}

/// A name that cannot be used is refused before the server is contacted —
/// this one is not a server at all, and the refusal still names the name.
#[test]
fn connect_refuses_an_unusable_name_before_anything_else() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", DEAD_SERVER, "--name", "my laptop"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );

    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("not a usable machine name"),
        "stderr: {}",
        r.stderr
    );
    assert!(!r.stderr.contains("reach"), "stderr: {}", r.stderr);
}

/// What is left in the shell after `connect` is named: a variable that
/// disagrees with the name is a warning, one that agrees is only redundant.
#[test]
fn connect_names_the_machine_variables_a_shell_profile_still_exports() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[
            ("RECALL_HOME", &home_str),
            ("RECALL_SOURCE_ENV", "laptop"),
            ("RECALL_MACHINE_KEY", "machine:jarvis"),
        ],
        None,
    );

    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr
            .contains("RECALL_SOURCE_ENV=laptop overrides the name jarvis"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("remove from your shell profile: RECALL_MACHINE_KEY"),
        "stderr: {}",
        r.stderr
    );
}

// ---------------------------------------------------------------------------
// devices: enrolment, signed requests, and the owner's commands
// ---------------------------------------------------------------------------

/// Runs `fut` to completion, for a test that talks to a server itself.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(fut)
}

/// The operator, talking to the server directly with its token.
fn operator(server: &LiveServer) -> recall_hooks::client::Client {
    recall_hooks::client::Client::new(&server.url, "right").unwrap()
}

/// The device `device.key` in `home` holds for `url`.
fn saved_device(home: &Path, url: &str) -> Option<recall_hooks::home::DeviceEntry> {
    recall_hooks::home::Home::at(home)
        .load_devices()
        .unwrap()
        .and_then(|d| d.for_url(url).cloned())
}

/// A machine connected the way a person's first one is: its token saved,
/// then `recall connect --yes`, which enrols it and approves it with that
/// token.
fn enrolled(server: &LiveServer, repo: &Repo, name: &str) -> tempfile::TempDir {
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let r = run(
        &["connect", "--yes", "--name", name],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 0, "connect failed: {}", r.stderr);
    home
}

/// Writes a memory file and runs the push hook on it, as Claude Code does
/// after an edit.
fn push_memory(repo: &Repo, env: &[(&str, &str)], name: &str, body: &str) -> Run {
    let memory_dir = status_json(repo.path(), env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::create_dir_all(&memory_dir).unwrap();
    let file = Path::new(&memory_dir).join(name);
    std::fs::write(&file, body).unwrap();
    run(&["push"], repo.path(), env, Some(&hook_payload(&file)))
}

/// What the server holds for this repository, read with the operator's
/// token.
fn stored(server: &LiveServer, repo: &Repo, env: &[(&str, &str)]) -> Vec<recall_wire::File> {
    let key = status_json(repo.path(), env)["project_key"]
        .as_str()
        .unwrap()
        .to_string();
    block_on(operator(server).pull(&key)).unwrap().files
}

/// The finding `doctor --json` reports for `check`.
fn doctor_finding(cwd: &Path, env: &[(&str, &str)], check: &str) -> serde_json::Value {
    let r = run(&["doctor", "--json"], cwd, env, None);
    let found: serde_json::Value = serde_json::from_str(&r.stdout)
        .unwrap_or_else(|e| panic!("doctor --json is not JSON ({e}): {}", r.stdout));
    found
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["check"] == check)
        .cloned()
        .unwrap_or_else(|| panic!("no {check} finding: {found}"))
}

/// A machine that has asked to enrol and is waiting for approval, made
/// directly so the test holds its key.
fn pending_enrollment(
    url: &str,
    name: &str,
) -> (recall_hooks::device::DeviceKey, recall_wire::EnrollPending) {
    use recall_hooks::client::{Client, Enrolled};
    let key = recall_hooks::device::DeviceKey::generate().unwrap();
    let open = Client::new(url, "").unwrap();
    match block_on(open.enroll(&key.enroll_request(name, None))).unwrap() {
        Enrolled::Pending(pending) => (key, pending),
        other => panic!("expected to wait for approval, got {other:?}"),
    }
}

/// The first machine: `connect --yes` with the operator's token saved
/// enrols it, shows the code and the fingerprint, approves it as admin,
/// and stops keeping the token. From then on it needs no token for
/// anything, the owner's commands included.
#[test]
fn connect_enrolls_the_first_machine_and_approves_it_with_the_operator_token() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let device = saved_device(home.path(), &server.url).expect("a device key is saved");
    assert_eq!(
        (device.name.as_str(), device.scope.as_str()),
        ("jarvis", "admin")
    );
    assert!(!device.ephemeral);
    let listed = block_on(operator(&server).devices()).unwrap().devices;
    let on_server = listed
        .iter()
        .find(|d| d.id == device.device_id)
        .expect("the server knows the device this machine saved");
    assert!(
        r.stderr
            .contains(&format!("Fingerprint  {}", on_server.fingerprint)),
        "the fingerprint shown is the one the server holds: {}",
        r.stderr
    );
    assert!(r.stderr.contains("Code "), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("Removed the shared token"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        !home.path().join("credentials.toml").exists(),
        "the token is no longer kept once it is not needed"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.path().join("device.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the private key is the owner's alone");
    }

    // An admin device, with no token anywhere.
    let list = run(&["devices", "list", "--json"], repo.path(), &env, None);
    assert_eq!(list.code, 0, "stderr: {}", list.stderr);
    let doc: serde_json::Value = serde_json::from_str(&list.stdout).unwrap();
    assert_eq!(doc["devices"][0]["name"], "jarvis", "{doc}");
    let text = run(&["devices", "list"], repo.path(), &env, None);
    assert!(
        text.stdout.contains("jarvis") && text.stdout.contains("this machine"),
        "{}",
        text.stdout
    );

    let rep = status_json(repo.path(), &env);
    assert_eq!(rep["auth"], "device", "{rep}");
    assert_eq!(rep["device"]["name"], "jarvis", "{rep}");
    assert_eq!(rep["device"]["confirmed"], true, "{rep}");
    assert_eq!(rep["device"]["key_storage"], "file", "{rep}");
    assert_eq!(rep["server_devices"], true, "{rep}");
    let finding = doctor_finding(repo.path(), &env, "device");
    assert_eq!(finding["level"], "ok", "{finding}");
    assert!(
        finding["detail"]
            .as_str()
            .unwrap()
            .contains("enrolled as jarvis (admin)"),
        "{finding}"
    );
    assert_eq!(
        doctor_finding(repo.path(), &env, "RECALL_TOKEN")["level"],
        "ok"
    );

    // Connecting again finds it enrolled and changes nothing.
    let again = run(&["connect", "--yes"], repo.path(), &env, None);
    assert_eq!(again.code, 0, "stderr: {}", again.stderr);
    assert!(
        again.stderr.contains("Enrolled as jarvis (admin)"),
        "stderr: {}",
        again.stderr
    );
    assert_eq!(
        saved_device(home.path(), &server.url).unwrap().device_id,
        device.device_id
    );
}

/// With a device key, the hooks sign instead of sending a token, and the
/// server files a push under the device's name, not the label the push
/// claims.
#[test]
fn a_device_signs_its_pushes_and_pulls_and_the_name_belongs_to_the_key() {
    let server = live_server("right");
    let repo = git_repo();
    let home = enrolled(&server, &repo, "jarvis");
    let home_str = home.path().to_string_lossy().to_string();
    // A label claiming to be some other machine.
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_SOURCE_ENV", "not-jarvis"),
    ];

    let r = push_memory(&repo, &env, "fact.md", "A fact.\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("pushed 1"), "stderr: {}", r.stderr);

    let files = stored(&server, &repo, &env);
    let fact = files
        .iter()
        .find(|f| f.file_path == "fact.md")
        .expect("the push arrived");
    assert_eq!(fact.source_env, "jarvis");

    let pulled = run(&["pull"], repo.path(), &env, None);
    assert_eq!(pulled.code, 0, "stderr: {}", pulled.stderr);
    assert!(
        pulled.stderr.contains("synced 1 memory file(s)"),
        "a signed pull: {}",
        pulled.stderr
    );
}

/// `approve --fingerprint` compares what the machine shows with what the
/// code would approve, and a difference approves nothing. So does having
/// nobody to confirm. The details are shown before anything is decided.
#[test]
fn devices_approve_refuses_a_key_whose_fingerprint_is_not_the_one_given() {
    use recall_hooks::client::{Client, Poll};
    let server = live_server("right");
    let repo = git_repo();
    let (key, pending) = pending_enrollment(&server.url, "phone");
    let open = Client::new(&server.url, "").unwrap();
    let env = [
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];

    let someone_else = recall_hooks::device::DeviceKey::generate().unwrap();
    let r = run(
        &[
            "devices",
            "approve",
            &pending.user_code,
            "--fingerprint",
            &someone_else.fingerprint(),
            "--yes",
        ],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("Nothing was approved"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        r.stderr.contains(&key.fingerprint()) && r.stderr.contains("phone"),
        "what the code would approve is shown: {}",
        r.stderr
    );
    assert!(!matches!(
        block_on(open.poll(&pending.enrollment_id)).unwrap(),
        Poll::Approved(_)
    ));

    let r = run(
        &["devices", "approve", &pending.user_code.to_lowercase()],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("needs a terminal"),
        "stderr: {}",
        r.stderr
    );

    // The right one, typed without its prefix.
    let bare = key.fingerprint().trim_start_matches("SHA256:").to_string();
    let r = run(
        &[
            "devices",
            "approve",
            &pending.user_code,
            "--fingerprint",
            &bare,
            "--yes",
        ],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("Approved phone (sync)"),
        "stdout: {}",
        r.stdout
    );
    match block_on(open.poll(&pending.enrollment_id)).unwrap() {
        Poll::Approved(approved) => assert_eq!(approved.scope, "sync"),
        other => panic!("expected approval, got {other:?}"),
    }
}

/// A cloud session holds `RECALL_AUTHKEY` and nothing else. Its first
/// pull enrols it, approved at once and ephemeral, with nothing typed; its
/// later hooks sign as that device; and it cannot do anything an admin can.
#[test]
fn a_cloud_session_enrolls_itself_at_its_first_pull_with_an_enrolment_key() {
    let server = live_server("right");
    let repo = git_repo();
    let key = block_on(
        operator(&server).create_authkey(&recall_wire::AuthkeyRequest {
            tag: "cloud".into(),
            expires_in_days: 1,
            ephemeral: true,
            max_devices: None,
        }),
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.key.as_str()),
    ];

    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("enrolled this session as device cloud-"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        !r.stderr.contains("leaving local memory untouched"),
        "and then it pulled: {}",
        r.stderr
    );
    let device = saved_device(home.path(), &server.url).expect("the key is kept");
    assert!(device.ephemeral);
    assert_eq!(device.scope, "sync");

    // The next session start is the same device, not another one.
    let again = run(&["pull"], repo.path(), &env, None);
    assert_eq!(again.code, 0, "stderr: {}", again.stderr);
    assert!(
        !again.stderr.contains("enrolled"),
        "stderr: {}",
        again.stderr
    );
    assert_eq!(
        saved_device(home.path(), &server.url).unwrap().device_id,
        device.device_id
    );

    let r = push_memory(&repo, &env, "cloud.md", "From the cloud.\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let files = stored(&server, &repo, &env);
    let cloud = files.iter().find(|f| f.file_path == "cloud.md").unwrap();
    assert_eq!(cloud.source_env, device.name);

    let r = run(&["devices", "list"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("sync scope"), "stderr: {}", r.stderr);
}

/// An authkey for cloud sessions, which enrols ephemeral devices.
fn ephemeral_authkey(server: &LiveServer) -> String {
    block_on(
        operator(server).create_authkey(&recall_wire::AuthkeyRequest {
            tag: "cloud".into(),
            expires_in_days: 1,
            ephemeral: true,
            max_devices: None,
        }),
    )
    .unwrap()
    .key
}

/// A device entry made here rather than by enrolling, for a server that
/// has never heard of it.
fn made_up_device(id: &str, name: &str, ephemeral: bool) -> recall_hooks::home::DeviceEntry {
    recall_hooks::device::DeviceKey::generate()
        .unwrap()
        .entry(id, name, "admin", ephemeral)
}

/// Whether a hook's stderr says it enrolled, or set about enrolling.
fn enrolled_again(stderr: &str) -> bool {
    stderr.contains("enrolling again") || stderr.contains("enrolled this session")
}

/// Every device the server lists, revoked ones included.
fn devices_on(server: &LiveServer) -> Vec<recall_wire::Device> {
    block_on(operator(server).devices()).unwrap().devices
}

/// A cloud session's device swept away after it sat idle is refused as
/// unknown, and a session holding `RECALL_AUTHKEY` enrols again, once,
/// and the hook that noticed still does its work. Only that server's key is
/// replaced: another server's, in the same file, is kept.
#[test]
fn a_cloud_session_enrolls_again_after_its_device_is_swept() {
    let server = live_server("right");
    let repo = git_repo();
    let key = ephemeral_authkey(&server);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.as_str()),
    ];
    assert_eq!(run(&["pull"], repo.path(), &env, None).code, 0);
    let first = saved_device(home.path(), &server.url).unwrap();
    let elsewhere = "https://elsewhere.example.com";
    let other = made_up_device("dev_elsewhere", "jarvis", false);
    recall_hooks::home::Home::at(home.path())
        .save_device(elsewhere, other.clone())
        .unwrap();

    server.sweep_every_ephemeral_device();
    let r = push_memory(&repo, &env, "after-sweep.md", "Still here.\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("(unknown device), enrolling again"),
        "stderr: {}",
        r.stderr
    );
    let second = saved_device(home.path(), &server.url).unwrap();
    assert_ne!(second.device_id, first.device_id);
    assert!(second.ephemeral);
    let files = stored(&server, &repo, &env);
    let file = files
        .iter()
        .find(|f| f.file_path == "after-sweep.md")
        .expect("the push went through after enrolling again");
    assert_eq!(file.source_env, second.name);
    assert_eq!(
        saved_device(home.path(), elsewhere).as_ref(),
        Some(&other),
        "another server's key is kept"
    );

    server.sweep_every_ephemeral_device();
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("(unknown device), enrolling again"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        !r.stderr.contains("leaving local memory untouched"),
        "stderr: {}",
        r.stderr
    );
    let third = saved_device(home.path(), &server.url).unwrap();
    assert_ne!(third.device_id, second.device_id);
    assert_eq!(saved_device(home.path(), elsewhere).as_ref(), Some(&other));
}

/// Revoking a device cuts it off, whatever the machine holds. A cloud
/// session with `RECALL_AUTHKEY` does not enrol again, and neither does an
/// admin laptop that has one set: the hook says so, a session still
/// starts, the key stays where it is, and the server gains no device.
#[test]
fn a_revoked_device_is_not_enrolled_again_even_with_an_authkey() {
    let server = live_server("right");
    let repo = git_repo();
    let key = ephemeral_authkey(&server);

    let cloud = tempfile::tempdir().unwrap();
    let cloud_str = cloud.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", cloud_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.as_str()),
    ];
    assert_eq!(run(&["pull"], repo.path(), &env, None).code, 0);
    let session = saved_device(cloud.path(), &server.url).unwrap();
    block_on(operator(&server).revoke_device(&session.device_id)).unwrap();

    let r = push_memory(&repo, &env, "after-revoke.md", "Not synced.\n");
    assert_eq!(r.code, 2, "the push is refused: {}", r.stderr);
    assert!(
        r.stderr.contains("this device has been revoked")
            && r.stderr.contains("not enrolled again")
            && !r.stderr.contains("enrolling again"),
        "stderr: {}",
        r.stderr
    );
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "a session must still start: {}", r.stderr);
    assert!(
        r.stderr.contains("not enrolled again"),
        "stderr: {}",
        r.stderr
    );
    assert_eq!(
        saved_device(cloud.path(), &server.url).as_ref(),
        Some(&session),
        "the revoked key is kept, so the next hook is refused too"
    );
    assert_eq!(devices_on(&server).len(), 1);

    let laptop = enrolled(&server, &repo, "jarvis");
    let laptop_str = laptop.path().to_string_lossy().to_string();
    let admin = saved_device(laptop.path(), &server.url).unwrap();
    assert_eq!(admin.scope, "admin");
    block_on(operator(&server).revoke_device(&admin.device_id)).unwrap();
    let env = [
        ("RECALL_HOME", laptop_str.as_str()),
        ("RECALL_AUTHKEY", key.as_str()),
    ];
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("not enrolled again") && r.stderr.contains("recall connect"),
        "stderr: {}",
        r.stderr
    );
    let r = push_memory(&repo, &env, "laptop.md", "Not synced either.\n");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert_eq!(
        saved_device(laptop.path(), &server.url).as_ref(),
        Some(&admin)
    );
    assert_eq!(devices_on(&server).len(), 2, "no new device");
}

/// A device the server has no record of, and that was not made to be thrown
/// away, is not replaced by a hook even with `RECALL_AUTHKEY` set: the key
/// is kept and the hook says to run `recall connect`.
#[test]
fn a_lasting_device_the_server_does_not_know_is_left_to_connect() {
    let server = live_server("right");
    let repo = git_repo();
    let key = ephemeral_authkey(&server);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let lasting = made_up_device("dev_neverenrolledhere", "jarvis", false);
    recall_hooks::home::Home::at(home.path())
        .save_device(&server.url, lasting.clone())
        .unwrap();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.as_str()),
    ];

    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("unknown device") && r.stderr.contains("run recall connect"),
        "stderr: {}",
        r.stderr
    );
    assert!(!enrolled_again(&r.stderr), "stderr: {}", r.stderr);
    assert_eq!(
        saved_device(home.path(), &server.url).as_ref(),
        Some(&lasting)
    );
    assert!(devices_on(&server).is_empty());
}

/// Any other refusal of a signature, here one that does not verify, says
/// nothing about whether the device still exists: the key is kept and
/// nothing is enrolled, even for an ephemeral device with an authkey at
/// hand.
#[test]
fn a_signature_the_server_refuses_keeps_the_key_and_enrols_nothing() {
    let server = live_server("right");
    let repo = git_repo();
    let key = ephemeral_authkey(&server);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.as_str()),
    ];
    assert_eq!(run(&["pull"], repo.path(), &env, None).code, 0);
    let session = saved_device(home.path(), &server.url).unwrap();
    // The device the server knows, signed for with some other key.
    let wrong = recall_hooks::device::DeviceKey::generate().unwrap().entry(
        &session.device_id,
        &session.name,
        &session.scope,
        true,
    );
    recall_hooks::home::Home::at(home.path())
        .save_device(&server.url, wrong.clone())
        .unwrap();

    let started = std::time::Instant::now();
    let r = push_memory(&repo, &env, "fact.md", "A fact.\n");
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(!enrolled_again(&r.stderr), "stderr: {}", r.stderr);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "nor signed again and again"
    );
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(!enrolled_again(&r.stderr), "stderr: {}", r.stderr);
    assert_eq!(
        saved_device(home.path(), &server.url).as_ref(),
        Some(&wrong)
    );
    assert_eq!(devices_on(&server).len(), 1);
}

/// Hooks run at once: a new session's first burst of edits, or a burst
/// just after its device was swept. Between them they enrol one device,
/// not one each, and every one of them does its work.
#[test]
fn hooks_that_start_at_once_enrol_one_device_between_them() {
    const AT_ONCE: usize = 8;
    let server = live_server("right");
    let repo = git_repo();
    let key = ephemeral_authkey(&server);
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.as_str()),
    ];
    let finish = |children: Vec<std::process::Child>| {
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    };

    let pulls = (0..AT_ONCE)
        .map(|_| {
            command(&["pull"], repo.path(), &env)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    finish(pulls);
    let listed = devices_on(&server);
    assert_eq!(listed.len(), 1, "{listed:?}");
    let first = saved_device(home.path(), &server.url).unwrap();
    assert_eq!(listed[0].id, first.device_id);

    server.sweep_every_ephemeral_device();
    let memory_dir = status_json(repo.path(), &env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::create_dir_all(&memory_dir).unwrap();
    let pushes = (0..AT_ONCE)
        .map(|i| {
            let file = Path::new(&memory_dir).join(format!("burst-{i}.md"));
            std::fs::write(&file, format!("Edit {i}.\n")).unwrap();
            let mut child = command(&["push"], repo.path(), &env)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(hook_payload(&file).as_bytes())
                .unwrap();
            child
        })
        .collect();
    finish(pushes);
    let second = saved_device(home.path(), &server.url).unwrap();
    assert_ne!(second.device_id, first.device_id);
    let listed = devices_on(&server);
    let new: Vec<_> = listed.iter().filter(|d| d.id != first.device_id).collect();
    assert_eq!(new.len(), 1, "{listed:?}");
    assert_eq!(new[0].id, second.device_id);
    let files = stored(&server, &repo, &env);
    for i in 0..AT_ONCE {
        assert!(
            files.iter().any(|f| f.file_path == format!("burst-{i}.md")),
            "burst-{i}.md was pushed: {files:?}"
        );
    }
}

/// A `device.key` that cannot be read stops the hooks with a line saying
/// so, rather than sending `RECALL_TOKEN` in its place: it may hold this
/// server's key, and the token is the shared secret enrolling stopped
/// sending. Nor is anything enrolled, which could not be saved.
#[test]
fn an_unreadable_device_key_is_no_reason_to_send_the_token() {
    let fake = fake_server(release_before_devices);
    let repo = git_repo();
    let home = recall_home_with(&[(&fake.url, "right")], &fake.url);
    let home_str = home.path().to_string_lossy().to_string();
    std::fs::write(home.path().join("device.key"), "servers = [").unwrap();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_TOKEN", "right"),
        ("RECALL_AUTHKEY", "recall-ak-whatever"),
    ];

    let r = push_memory(&repo, &env, "fact.md", "A fact.\n");
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("device.key") && r.stderr.contains("RECALL_TOKEN included"),
        "stderr: {}",
        r.stderr
    );
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("device.key") && r.stderr.contains("leaving local memory untouched"),
        "stderr: {}",
        r.stderr
    );

    let seen = fake.seen();
    assert!(
        seen.iter()
            .all(|s| s.authorization.is_none() && !s.signed && s.path != "/sync"),
        "{seen:?}"
    );
    assert!(
        seen.iter().all(|s| !s.path.starts_with("/v1/")),
        "nothing enrolled: {seen:?}"
    );
}

/// `recall status` reports a `device.key` others can read, and leaves it
/// as it is; the next hook makes it its owner's alone and says so, once.
#[cfg(unix)]
#[test]
fn a_device_key_others_can_read_is_reported_then_made_private_by_a_hook() {
    use std::os::unix::fs::PermissionsExt;
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    recall_hooks::home::Home::at(home.path())
        .save_device(DEAD_SERVER, made_up_device("dev_x", "jarvis", false))
        .unwrap();
    let key_file = home.path().join("device.key");
    std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mode = || std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777;
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", DEAD_SERVER),
    ];

    let rep = status_json(repo.path(), &env);
    assert_eq!(rep["device_file_exposed"], true, "{rep}");
    assert_eq!(mode(), 0o644, "status only reports");

    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("was readable by other users"),
        "stderr: {}",
        r.stderr
    );
    assert_eq!(mode(), 0o600);
    let again = run(&["pull"], repo.path(), &env, None);
    assert!(
        !again.stderr.contains("readable by other users"),
        "said once: {}",
        again.stderr
    );
    assert_eq!(status_json(repo.path(), &env)["device_file_exposed"], false);
}

/// A damaged device key is the device's problem, and status reports it as
/// one: the server is still asked whether it is up, with nothing that
/// identifies this machine, so `server_ok` goes on meaning just that.
#[test]
fn status_reports_a_damaged_device_key_as_the_devices_problem() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];

    std::fs::write(home.path().join("device.key"), "servers = [").unwrap();
    let rep = status_json(repo.path(), &env);
    assert_eq!(rep["server_ok"], true, "{rep}");
    assert!(rep.get("server_error").is_none(), "{rep}");
    assert!(rep["device_error"].is_string(), "{rep}");
    assert_eq!(rep["auth"], "none", "{rep}");
    assert_eq!(
        doctor_finding(repo.path(), &env, "device key")["level"],
        "fail"
    );

    // A file that reads, holding something for this server that is no key.
    std::fs::remove_file(home.path().join("device.key")).unwrap();
    let mut damaged = made_up_device("dev_x", "jarvis", false);
    damaged.private_key = "not a key".into();
    recall_hooks::home::Home::at(home.path())
        .save_device(&server.url, damaged)
        .unwrap();
    let rep = status_json(repo.path(), &env);
    assert_eq!(rep["server_ok"], true, "{rep}");
    assert!(rep.get("server_error").is_none(), "{rep}");
    assert!(
        rep["device_error"].as_str().unwrap().contains("damaged"),
        "{rep}"
    );
    assert_eq!(rep["auth"], "none", "{rep}");
}

/// A laptop's revoked device, with no authkey: the session still
/// starts, the hook says what to do, doctor fails it, and `connect`
/// enrols it afresh, approved this time from another machine by code.
#[test]
fn a_revoked_laptop_is_told_to_connect_and_connect_enrolls_it_again() {
    let server = live_server("right");
    let repo = git_repo();
    let home = enrolled(&server, &repo, "jarvis");
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];
    let first = saved_device(home.path(), &server.url).unwrap();
    block_on(operator(&server).revoke_device(&first.device_id)).unwrap();

    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "a session must still start: {}", r.stderr);
    assert!(
        r.stderr.contains("this device has been revoked") && r.stderr.contains("recall connect"),
        "stderr: {}",
        r.stderr
    );
    let finding = doctor_finding(repo.path(), &env, "device");
    assert_eq!(finding["level"], "fail", "{finding}");
    assert_eq!(finding["fix"], "recall connect", "{finding}");

    // No token is kept any more, so `connect --yes` enrols and waits for
    // someone to approve the code it shows.
    let mut child = command(&["connect", "--yes"], repo.path(), &env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (lines, seen) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            let _ = lines.send(line);
        }
    });
    let mut shown = Vec::new();
    let code = loop {
        let line = seen
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("no code shown: {}", shown.join("\n")));
        shown.push(line.clone());
        if let Some(at) = line.find("Code ") {
            break line[at + 5..].trim().to_string();
        }
    };
    let approve = run(
        &["devices", "approve", &code, "--yes"],
        repo.path(),
        &[
            ("RECALL_URL", server.url.as_str()),
            ("RECALL_TOKEN", "right"),
        ],
        None,
    );
    assert_eq!(approve.code, 0, "stderr: {}", approve.stderr);
    let status = child.wait().unwrap();
    reader.join().unwrap();
    shown.extend(seen.try_iter());
    assert!(status.success(), "connect: {}", shown.join("\n"));
    assert!(
        shown.iter().any(|l| l.contains("recall devices approve")),
        "it said how to approve it: {}",
        shown.join("\n")
    );

    let second = saved_device(home.path(), &server.url).unwrap();
    assert_ne!(second.device_id, first.device_id);
    assert_eq!(second.scope, "sync", "approved by code, the narrow scope");
    let r = run(&["pull"], repo.path(), &env, None);
    assert!(!r.stderr.contains("revoked"), "stderr: {}", r.stderr);
}

/// One request a fake server received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    /// Whether it carried a signature.
    signed: bool,
    body: String,
}

/// A server that answers from a table, and remembers what it was sent:
/// for what the real one cannot be made to do, such as being a release
/// from before devices existed.
struct FakeServer {
    url: String,
    seen: std::sync::Arc<std::sync::Mutex<Vec<Seen>>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeServer {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn fake_server(respond: fn(&Seen) -> (u16, serde_json::Value)) -> FakeServer {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let app = axum::Router::new().fallback(
                    move |method: axum::http::Method,
                          uri: axum::http::Uri,
                          headers: axum::http::HeaderMap,
                          body: axum::body::Bytes| async move {
                        let seen = Seen {
                            method: method.to_string(),
                            path: uri.path().to_string(),
                            authorization: headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string),
                            signed: headers.contains_key("signature-input")
                                || headers.contains_key("signature"),
                            body: String::from_utf8_lossy(&body).into_owned(),
                        };
                        let (code, reply) = respond(&seen);
                        record.lock().unwrap().push(seen);
                        (
                            axum::http::StatusCode::from_u16(code).unwrap(),
                            axum::Json(reply),
                        )
                    },
                );
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
    });
    FakeServer {
        url: format!("http://127.0.0.1:{port}"),
        seen,
        stop: Some(stop),
        thread: Some(thread),
    }
}

/// What approving looks up, and what it then sends: the fingerprint it
/// showed, so the server can refuse a key that is not the one the owner
/// saw. The real server would approve without it, which is why this is
/// checked here rather than there.
#[test]
fn devices_approve_binds_the_approval_to_the_fingerprint_it_showed() {
    const FINGERPRINT: &str = "SHA256:sWwtG+rRJiY5dk/bDuTTd0WZM2vUk0BM2ksRNsWfIGI";
    let fake = fake_server(|seen| match (seen.method.as_str(), seen.path.as_str()) {
        ("GET", "/v1/devices/pending/WDJB-MJHT") => (
            200,
            serde_json::json!({
                "user_code": "WDJB-MJHT",
                "name": "phone",
                "agent": "recall/0.4.1 (linux-x86_64)",
                "fingerprint": FINGERPRINT,
                "expires_in": 600
            }),
        ),
        ("POST", "/v1/devices/approve") => (
            200,
            serde_json::to_value(recall_wire::Device {
                id: "dev_x".into(),
                name: "phone".into(),
                scope: "sync".into(),
                fingerprint: FINGERPRINT.into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        _ => (404, serde_json::json!({ "error": "not found" })),
    });
    let repo = git_repo();

    let r = run(
        &["devices", "approve", "wdjb mjht", "--yes"],
        repo.path(),
        &[("RECALL_URL", fake.url.as_str()), ("RECALL_TOKEN", "right")],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let seen = fake.seen();
    let looked = seen
        .iter()
        .position(|s| s.path == "/v1/devices/pending/WDJB-MJHT")
        .expect("looked the code up first");
    let approved = seen
        .iter()
        .position(|s| s.path == "/v1/devices/approve")
        .expect("then approved it");
    assert!(looked < approved);
    let sent: recall_wire::ApproveRequest = serde_json::from_str(&seen[approved].body).unwrap();
    assert_eq!(sent.user_code, "WDJB-MJHT");
    assert_eq!(sent.scope, "sync");
    assert_eq!(sent.fingerprint.as_deref(), Some(FINGERPRINT));
}

/// A 0.4.0 server: discovery lists only the token, and there are no
/// device routes. Against it nothing changes: connect saves the token,
/// no key is made, every request carries the token and none a signature,
/// and an authkey set anyway falls back to the token with a line.
fn release_before_devices(seen: &Seen) -> (u16, serde_json::Value) {
    let authorized = seen.authorization.as_deref() == Some("Bearer right");
    match (seen.method.as_str(), seen.path.as_str()) {
        ("GET", "/health") => (
            200,
            serde_json::to_value(recall_wire::Health {
                status: "ok".into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        ("GET", "/.well-known/recall") => (
            200,
            serde_json::json!({
                "protocol": { "current": 1, "supported": [1] },
                "server": { "version": "0.4.0", "build": { "channel": "release" } },
                "min_client": "0.1.0",
                "auth": { "methods": ["bearer"] },
                "capabilities": { "merge_base": {} }
            }),
        ),
        (_, "/admin/stats" | "/sync") if !authorized => {
            (401, serde_json::json!({ "error": "unauthorized" }))
        }
        ("GET", "/admin/stats") => (200, serde_json::json!({})),
        ("GET", "/sync") => (200, serde_json::json!({ "project_key": "", "files": [] })),
        ("POST", "/sync") => (
            200,
            serde_json::to_value(recall_wire::PushResponse {
                ok: true,
                ..Default::default()
            })
            .unwrap(),
        ),
        _ => (404, serde_json::json!({ "error": "not found" })),
    }
}

/// A server that enrols devices, answering as the stand-in is told for
/// discovery and for `GET /v1/devices/me`. `RECALL_TOKEN` is `right`; an
/// enrolment is pending until its first poll, which approves it as admin.
fn device_era(seen: &Seen, discovery: u16, me: u16) -> (u16, serde_json::Value) {
    let operator = seen.authorization.as_deref() == Some("Bearer right");
    match (seen.method.as_str(), seen.path.as_str()) {
        ("GET", "/health") => (
            200,
            serde_json::to_value(recall_wire::Health {
                status: "ok".into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        ("GET", "/.well-known/recall") if discovery == 200 => (
            200,
            serde_json::json!({
                "protocol": { "current": 1, "supported": [1] },
                "server": { "version": "0.4.1", "build": { "channel": "release" } },
                "min_client": "0.1.0",
                "auth": { "methods": ["bearer", "device-sig-v1"] },
                "capabilities": { "devices": {
                    "enroll_path": "/v1/devices/enroll", "code_ttl_seconds": 900,
                    "poll_interval_seconds": 1, "signature_window_seconds": 60
                } }
            }),
        ),
        ("GET", "/.well-known/recall") => (discovery, serde_json::json!({ "error": "no" })),
        ("GET", "/admin/stats") if operator => (200, serde_json::json!({})),
        ("POST", "/v1/devices/approve") if operator => (
            200,
            serde_json::to_value(recall_wire::Device {
                id: "dev_fake".into(),
                name: "jarvis".into(),
                scope: "admin".into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        (_, "/admin/stats" | "/v1/devices/approve") => {
            (401, serde_json::json!({ "error": "unauthorized" }))
        }
        ("POST", "/v1/devices/enroll") => (
            200,
            serde_json::to_value(recall_wire::EnrollPending {
                enrollment_id: "enr_fake".into(),
                user_code: "WDJB-MJHT".into(),
                expires_in: 900,
                interval: 1,
            })
            .unwrap(),
        ),
        ("POST", "/v1/devices/enroll/poll") => (
            200,
            serde_json::to_value(recall_wire::EnrollPollResponse {
                device_id: "dev_fake".into(),
                scope: "admin".into(),
            })
            .unwrap(),
        ),
        ("GET", "/v1/devices/me") if me == 200 && seen.signed => (
            200,
            serde_json::to_value(recall_wire::DeviceIdentity {
                device_id: "dev_x".into(),
                name: "jarvis".into(),
                scope: "admin".into(),
                ephemeral: false,
            })
            .unwrap(),
        ),
        ("GET", "/v1/devices/me") => (me, serde_json::json!({ "error": "no" })),
        _ => (404, serde_json::json!({ "error": "not found" })),
    }
}

fn devices_server(seen: &Seen) -> (u16, serde_json::Value) {
    device_era(seen, 200, 200)
}

/// The same server, answering a device's check of itself with a `500`.
fn devices_server_failing_me(seen: &Seen) -> (u16, serde_json::Value) {
    device_era(seen, 200, 500)
}

/// The same server, whose discovery document fails with a `500`.
fn devices_server_failing_discovery(seen: &Seen) -> (u16, serde_json::Value) {
    device_era(seen, 500, 200)
}

/// The same server, with no discovery document at all, as one too old to
/// publish it answers.
fn devices_server_without_discovery(seen: &Seen) -> (u16, serde_json::Value) {
    device_era(seen, 404, 200)
}

/// Whether any request the stand-in saw carried `token` as a bearer.
fn sent_token(fake: &FakeServer, token: &str) -> bool {
    let bearer = format!("Bearer {token}");
    fake.seen()
        .iter()
        .any(|s| s.authorization.as_deref() == Some(bearer.as_str()))
}

/// Approving this machine with the operator's token names the fingerprint
/// it showed, so the server approves exactly the key that was enrolled.
#[test]
fn connect_approves_its_own_key_by_the_fingerprint_it_showed() {
    let fake = fake_server(devices_server);
    let repo = git_repo();
    let home = recall_home_with(&[(&fake.url, "right")], &fake.url);
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let saved = saved_device(home.path(), &fake.url).expect("the key is saved");
    let fingerprint = recall_hooks::device::DeviceKey::from_saved(&saved.private_key)
        .unwrap()
        .fingerprint();
    let seen = fake.seen();
    let approve = seen
        .iter()
        .find(|s| s.path == "/v1/devices/approve")
        .expect("it approved itself");
    assert_eq!(approve.authorization.as_deref(), Some("Bearer right"));
    let sent: recall_wire::ApproveRequest = serde_json::from_str(&approve.body).unwrap();
    assert_eq!(sent.user_code, "WDJB-MJHT");
    assert_eq!(sent.scope, "admin");
    assert_eq!(sent.fingerprint.as_deref(), Some(fingerprint.as_str()));
    assert!(
        r.stderr.contains(&format!("Fingerprint  {fingerprint}")),
        "the one it showed: {}",
        r.stderr
    );
}

/// `RECALL_TOKEN` in the environment belongs to the server `RECALL_URL`
/// names. Connecting to another server never sends it there, not even to
/// ask whether it works: that server enrols this machine and waits for it
/// to be approved from elsewhere.
#[test]
fn connect_never_sends_one_servers_token_to_another() {
    let other = fake_server(devices_server);
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", &other.url, "--yes", "--name", "jarvis"],
        repo.path(),
        &[
            ("RECALL_HOME", &home_str),
            ("RECALL_URL", "https://a.example.com"),
            ("RECALL_TOKEN", "token-for-a"),
        ],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(!sent_token(&other, "token-for-a"), "{:?}", other.seen());
    assert!(
        other
            .seen()
            .iter()
            .any(|s| s.path == "/v1/devices/enroll/poll"),
        "approved elsewhere: {:?}",
        other.seen()
    );
    assert!(saved_device(home.path(), &other.url).is_some());
}

/// Enrolling stops keeping this server's token, and only this server's:
/// one saved for another server is still there.
#[test]
fn connect_removes_only_this_servers_token() {
    let server = live_server("right");
    let repo = git_repo();
    let elsewhere = "https://other.example.com";
    let home = recall_home_with(
        &[(&server.url, "right"), (elsewhere, "other-token")],
        &server.url,
    );
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let creds = recall_hooks::home::Home::at(home.path())
        .load_credentials()
        .unwrap()
        .expect("another server's token is still kept");
    assert_eq!(creds.token_for(&server.url), None);
    assert_eq!(creds.token_for(elsewhere), Some("other-token"));
}

/// A device key the server could not be asked about is neither dropped nor
/// replaced: a `500` says nothing about the device either way.
#[test]
fn connect_keeps_a_device_key_it_could_not_check() {
    let fake = fake_server(devices_server_failing_me);
    let repo = git_repo();
    let home = recall_home_with(&[], &fake.url);
    let home_str = home.path().to_string_lossy().to_string();
    let entry = made_up_device("dev_x", "jarvis", false);
    recall_hooks::home::Home::at(home.path())
        .save_device(&fake.url, entry.clone())
        .unwrap();

    let r = run(
        &["connect", "--yes"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr
            .contains("could not check this machine's device key"),
        "stderr: {}",
        r.stderr
    );
    assert_eq!(saved_device(home.path(), &fake.url).as_ref(), Some(&entry));
    assert!(
        fake.seen()
            .iter()
            .all(|s| !s.path.starts_with("/v1/devices/enroll")),
        "nothing enrolled: {:?}",
        fake.seen()
    );
}

/// A machine with a device key is checked as one whatever the discovery
/// document says, and the shared token is neither sent nor saved again. A
/// discovery document that fails is the server not answering properly,
/// which stops `connect`; one that is missing, as on a server too old to
/// publish it, still leaves the device to be checked.
#[test]
fn connect_with_a_device_key_never_falls_back_to_the_token() {
    let repo = git_repo();
    let entry = made_up_device("dev_x", "jarvis", false);

    let failing = fake_server(devices_server_failing_discovery);
    let home = recall_home_with(&[(&failing.url, "right")], &failing.url);
    let home_str = home.path().to_string_lossy().to_string();
    recall_hooks::home::Home::at(home.path())
        .save_device(&failing.url, entry.clone())
        .unwrap();
    let creds = std::fs::read_to_string(home.path().join("credentials.toml")).unwrap();
    let r = run(
        &["connect", "--yes"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("/.well-known/recall"),
        "stderr: {}",
        r.stderr
    );
    assert!(!sent_token(&failing, "right"), "{:?}", failing.seen());
    assert_eq!(
        std::fs::read_to_string(home.path().join("credentials.toml")).unwrap(),
        creds
    );
    assert_eq!(
        saved_device(home.path(), &failing.url).as_ref(),
        Some(&entry)
    );

    let missing = fake_server(devices_server_without_discovery);
    let home = recall_home_with(&[(&missing.url, "right")], &missing.url);
    let home_str = home.path().to_string_lossy().to_string();
    recall_hooks::home::Home::at(home.path())
        .save_device(&missing.url, entry.clone())
        .unwrap();
    let r = run(
        &["connect", "--yes"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("Enrolled as jarvis (admin)"),
        "stderr: {}",
        r.stderr
    );
    assert!(!sent_token(&missing, "right"), "{:?}", missing.seen());
    assert!(
        missing
            .seen()
            .iter()
            .any(|s| s.path == "/v1/devices/me" && s.signed),
        "{:?}",
        missing.seen()
    );
    let saved = recall_hooks::home::Home::at(home.path())
        .load_credentials()
        .unwrap()
        .and_then(|c| c.token_for(&missing.url).map(str::to_string));
    assert_eq!(saved, None, "the token is not saved again");
}

/// `connect` refuses at once when `device.key` cannot be read: before the
/// server is asked anything, so no device is approved whose key could then
/// not be saved.
#[test]
fn connect_refuses_when_the_device_key_file_cannot_be_read() {
    let fake = fake_server(devices_server);
    let repo = git_repo();
    let home = recall_home_with(&[(&fake.url, "right")], &fake.url);
    let home_str = home.path().to_string_lossy().to_string();
    std::fs::write(home.path().join("device.key"), "servers = [").unwrap();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 1, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("device.key") && r.stderr.contains("Nothing was changed"),
        "stderr: {}",
        r.stderr
    );
    assert!(fake.seen().is_empty(), "{:?}", fake.seen());
    assert_eq!(
        std::fs::read_to_string(home.path().join("device.key")).unwrap(),
        "servers = ["
    );
}

#[test]
fn against_a_server_without_devices_everything_stays_on_the_token() {
    let fake = fake_server(release_before_devices);
    let repo = git_repo();
    let home = recall_home_with(&[(&fake.url, "right")], &fake.url);
    let home_str = home.path().to_string_lossy().to_string();

    let r = run(
        &["connect", "--yes", "--name", "jarvis"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("Saved token OK"), "stderr: {}", r.stderr);
    assert!(!r.stderr.contains("Fingerprint"), "stderr: {}", r.stderr);
    assert!(!home.path().join("device.key").exists());
    let creds = std::fs::read_to_string(home.path().join("credentials.toml")).unwrap();
    assert!(creds.contains("token = \"right\""), "{creds}");

    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_AUTHKEY", "recall-ak-notfromthisserver"),
    ];
    let r = push_memory(&repo, &env, "fact.md", "A fact.\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("this server does not enrol devices")
            && r.stderr.contains("using RECALL_TOKEN instead"),
        "stderr: {}",
        r.stderr
    );
    assert!(!home.path().join("device.key").exists());

    let rep = status_json(repo.path(), &[("RECALL_HOME", home_str.as_str())]);
    assert_eq!(rep["auth"], "bearer", "{rep}");
    assert_eq!(rep["server_devices"], false, "{rep}");
    let finding = doctor_finding(repo.path(), &[("RECALL_HOME", home_str.as_str())], "device");
    assert_eq!(finding["level"], "ok", "{finding}");

    let seen = fake.seen();
    let synced: Vec<&Seen> = seen.iter().filter(|s| s.path == "/sync").collect();
    assert!(!synced.is_empty(), "{seen:?}");
    for s in &synced {
        assert_eq!(s.authorization.as_deref(), Some("Bearer right"), "{s:?}");
        assert!(!s.signed, "{s:?}");
    }
    assert!(seen.iter().all(|s| !s.signed), "{seen:?}");
}

/// A deploy restarts the server, and for a few seconds after it refuses
/// every signature, since the nonces that would catch a replay went with
/// the process before. A hook firing then signs again a moment later
/// rather than dropping the push or the pull.
#[test]
fn a_signed_request_refused_just_after_a_server_start_is_signed_again() {
    let server = live_server_just_started("right");
    let started = std::time::Instant::now();
    let repo = git_repo();
    let key = block_on(
        operator(&server).create_authkey(&recall_wire::AuthkeyRequest {
            tag: "cloud".into(),
            expires_in_days: 1,
            ephemeral: true,
            max_devices: None,
        }),
    )
    .unwrap();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.key.as_str()),
    ];

    // Enrolling is unsigned; the pull after it is the first signature.
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("enrolled this session"),
        "stderr: {}",
        r.stderr
    );
    assert!(
        !r.stderr.contains("leaving local memory untouched"),
        "the pull went through: {}",
        r.stderr
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(2),
        "a signature this soon after the start is refused, so it waited and signed again"
    );
}
