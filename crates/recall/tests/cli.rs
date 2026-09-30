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
    assert!(
        first.stdout.contains("✓ Wired Recall's hooks into"),
        "{}",
        first.stdout
    );
    // What to do next, as one command, run where the person already is:
    // the path is not repeated in it.
    assert!(
        first
            .stdout
            .contains("→ git add .claude/settings.json && git commit"),
        "{}",
        first.stdout
    );
    assert!(!first.stdout.contains("git -C"), "{}", first.stdout);

    let second = run(&["init"], repo.path(), &[], None);
    assert_eq!(second.code, 0);
    assert!(
        second.stdout.contains("already wired"),
        "a second run should not claim to have done work: {:?}",
        second.stdout
    );
    assert!(
        !second.stdout.contains("git add"),
        "nothing new to commit: {}",
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
    assert!(
        r.stdout.contains("→ recall connect https://"),
        "and what supplies them: {}",
        r.stdout
    );
}

/// The `→` line under the line containing `marked`: the fix a problem
/// carries, which has to be there for the problem to be worth printing.
fn fix_after<'a>(out: &'a str, marked: &str) -> Option<&'a str> {
    out.lines()
        .skip_while(|l| !l.contains(marked))
        .skip(1)
        .take_while(|l| l.starts_with("    "))
        .map(str::trim)
        .find(|l| l.starts_with('→'))
}

/// `out` with every run of whitespace made one space: a report folds its
/// long lines to fit a terminal, and where the fold falls is not what a
/// test that looks for a phrase is asking about.
fn words(out: &str) -> String {
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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
    assert!(
        r.stdout.contains("✗ RECALL_URL") && r.stdout.contains("not set"),
        "stdout: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains("✗ hooks"),
        "an unwired project should say so plainly: {}",
        r.stdout
    );
    // Each problem carries what to run, as doctor's do.
    assert_eq!(
        fix_after(&r.stdout, "✗ hooks"),
        Some("→ recall init"),
        "{}",
        r.stdout
    );
    assert!(
        fix_after(&r.stdout, "✗ RECALL_URL").is_some_and(|f| f.contains("recall connect")),
        "{}",
        r.stdout
    );
    // `run` makes the repository the home directory, so every path in the
    // report is under it, and none is spelled out.
    assert!(
        !r.stdout.contains(&repo.path().display().to_string()),
        "home is shown as ~: {}",
        r.stdout
    );
    assert!(
        r.stdout.contains(" problems. ") || r.stdout.contains(" problem. "),
        "a closing line: {}",
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
        words(&r.stdout).contains("RECALL_PROJECT_KEY is set but unusable"),
        "a refused declaration has to be visible: {}",
        r.stdout
    );
    assert!(
        fix_after(&r.stdout, "✗ key").is_some_and(|f| f.contains("RECALL_PROJECT_KEY")),
        "and says what would make it usable: {}",
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
    let text = words(&r.stdout);
    assert!(
        text.contains(&file),
        "the settings file has to appear by path: {}",
        r.stdout
    );
    assert!(
        text.contains("overrides the value set in this shell"),
        "the shell value being dead has to be said, not implied: {}",
        r.stdout
    );
    assert!(
        text.contains(&format!("set by {file}")),
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
    "connect",
    "init",
    "backfill",
    "disconnect",
    "status",
    "doctor",
    "promote",
    "review",
    "eval",
    "devices",
    "authkey",
    "audit",
    "push",
    "pull",
    "version",
    "help",
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
        // Listed, as a line of its own, not merely mentioned: `init` is in
        // plenty of other commands' summaries.
        for command in COMMANDS {
            assert!(
                r.stdout
                    .lines()
                    .any(|l| l.starts_with("  ") && l.split_whitespace().next() == Some(command)),
                "{form:?} does not list `{command}`: {}",
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

/// Every way of asking for help answers with the version first: the line
/// `recall version` prints, byte for byte, so a pasted help carries what a
/// bug report needs first. Pinned to `version` rather than to a literal,
/// the way the three ways to ask for the version are pinned to each other.
#[test]
fn every_way_of_asking_for_help_starts_with_the_version() {
    let dir = tempfile::tempdir().unwrap();
    let version = run(&["version"], dir.path(), &[], None).stdout;
    let version = version.trim_end();
    assert!(version.starts_with("recall "), "{version}");

    let forms: &[&[&str]] = &[
        &["--help"],
        &["-h"],
        &["help"],
        &[],
        &["status", "--help"],
        &["status", "-h"],
        &["help", "status"],
        &["devices", "approve", "--help"],
        &["help", "devices", "approve"],
        &["devices"],
    ];
    for form in forms {
        let r = run(form, dir.path(), &[], None);
        // `recall` and `recall devices` alone print their help as an error,
        // on stderr, so that they exit non-zero.
        let text = if r.stdout.is_empty() {
            &r.stderr
        } else {
            &r.stdout
        };
        assert_eq!(
            text.lines().next(),
            Some(version),
            "recall {form:?}:\n{text}"
        );
    }
}

/// The top-level help lists the commands by what they are for, starting
/// with the one a new machine needs, and ends by saying where to start.
#[test]
fn the_top_level_help_groups_the_commands_and_says_where_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let r = run(&["--help"], dir.path(), &[], None);
    let out = &r.stdout;

    let headings: Vec<&str> = out
        .lines()
        .filter(|l| !l.starts_with(' ') && l.ends_with(':'))
        .collect();
    assert_eq!(
        headings,
        [
            "Get started:",
            "Every day:",
            "Memory quality:",
            "Your server:",
            "Run by Claude Code (hooks):",
            "Other:",
            "Options:",
        ],
        "{out}"
    );
    let first = out.lines().skip_while(|l| *l != "Get started:").nth(1);
    assert!(
        first.is_some_and(|l| l.split_whitespace().next() == Some("connect")),
        "{out}"
    );
    assert!(out.contains("recall connect https://"), "{out}");
    assert!(out.contains("recall help <command>"), "{out}");
    assert!(
        !out.lines()
            .any(|l| l.split_whitespace().next() == Some("serve")),
        "the hidden command should stay hidden: {out}"
    );
}

/// `-h` is the summary and `--help` the whole story: the one-line summary
/// is in both, and only `--help` goes on to explain.
#[test]
fn short_help_summarises_and_long_help_explains() {
    let dir = tempfile::tempdir().unwrap();
    let short = run(&["connect", "-h"], dir.path(), &[], None);
    let long = run(&["connect", "--help"], dir.path(), &[], None);
    assert_eq!(short.code, 0, "stderr: {}", short.stderr);
    assert_eq!(long.code, 0, "stderr: {}", long.stderr);

    // The version line, a blank line, then the summary.
    let summary = short.stdout.lines().nth(2).unwrap_or_default();
    assert!(!summary.is_empty(), "{}", short.stdout);
    assert!(long.stdout.contains(summary), "{}", long.stdout);
    assert!(
        long.stdout.lines().count() > short.stdout.lines().count(),
        "--help should say more than -h:\n{}\n{}",
        short.stdout,
        long.stdout
    );
    assert!(
        short.stdout.contains("see more with '--help'"),
        "{}",
        short.stdout
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
    assert!(
        fix_after(&r.stdout, "! token storage").is_some_and(|f| f.contains("recall connect")),
        "stdout: {}",
        r.stdout
    );
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
    assert_eq!(
        fix_after(&r.stdout, "! Your shell still supplies RECALL_TOKEN"),
        Some("→ remove it from your shell profile"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("! Removed what was saved here"),
        "and the closing line does not claim more than that: {}",
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

impl LiveServer {
    /// Stops the server, lets `edit` change its database file as someone
    /// holding that file can, and starts it again on the same port: to its
    /// clients, the same server, with whatever history the file now holds.
    fn restart_after(mut self, edit: impl FnOnce(&Path)) -> LiveServer {
        let port: u16 = self.url.rsplit(':').next().unwrap().parse().unwrap();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        let db = std::mem::replace(&mut self._db, tempfile::tempdir().unwrap());
        let token = self.cfg.token.clone();
        drop(self);
        edit(&db.path().join("recall.db"));
        // Free once the old listener closed; asked again for a moment in
        // case the system is slow to let it go.
        let listener = (0..50)
            .find_map(|_| {
                std::net::TcpListener::bind(("127.0.0.1", port))
                    .map_err(|_| std::thread::sleep(std::time::Duration::from_millis(100)))
                    .ok()
            })
            .expect("the same port again");
        serve(db, &token, 60, listener)
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
    live_server_started(token, 60)
}

/// A server that has only just started, so for its first few seconds it
/// refuses every signature, as a deployed one does after each deploy.
///
/// A server refuses what is signed up to `MAX_AHEAD_SECONDS`, five, after
/// its start, counting in whole seconds, so its refusal ends five to six
/// seconds after it starts, by how far into a second that was. This one
/// acts as if it started three or four seconds earlier, whichever ends
/// the refusal one and a half to two and a half seconds after it starts:
/// still well after a request made at once is first signed, and before
/// the client, two and a half seconds on, signs it again. So a test waits
/// out one of the client's waits, not the two or three a whole refusal
/// takes.
fn live_server_just_started(token: &str) -> LiveServer {
    let into_second = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_millis();
    live_server_started(token, if into_second < 500 { 4 } else { 3 })
}

/// A server acting as if it started `seconds_ago`. Every other test's
/// started a minute ago, so that requests are signed and accepted at once.
fn live_server_started(token: &str, seconds_ago: i64) -> LiveServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    serve(tempfile::tempdir().unwrap(), token, seconds_ago, listener)
}

/// A server on `listener`, over the database in `db`, acting as if it
/// started `seconds_ago`.
fn serve(
    db: tempfile::TempDir,
    token: &str,
    seconds_ago: i64,
    listener: std::net::TcpListener,
) -> LiveServer {
    let cfg = recall_server::Config {
        token: token.to_string(),
        db_path: db.path().join("recall.db").to_string_lossy().to_string(),
        merge_enabled: false,
        ..Default::default()
    };
    let store = std::sync::Arc::new(recall_server::Store::open(&cfg.db_path).expect("store opens"));
    let (kept_cfg, kept_store) = (cfg.clone(), store.clone());
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
                server.backdate_start(seconds_ago);
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
    assert!(
        r.stderr
            .contains("syncs from its next Claude Code session."),
        "the closing line says what happens next: {}",
        r.stderr
    );

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

/// A backfill says in one line what it came to, and says a zero only when
/// the zero is the answer: a column of zeros reads as though something
/// happened when nothing did.
#[test]
fn backfill_sums_up_in_one_line_and_hides_the_zeros() {
    let server = live_server("right");
    let repo = git_repo();
    let env = [
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];
    let memory_dir = status_json(repo.path(), &env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::create_dir_all(&memory_dir).unwrap();
    for name in ["a", "b"] {
        std::fs::write(
            Path::new(&memory_dir).join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: a fact\n---\n\nA fact.\n"),
        )
        .unwrap();
    }

    let first = run(&["backfill"], repo.path(), &env, None);
    assert_eq!(first.code, 0, "stderr: {}", first.stderr);
    assert!(
        first.stdout.contains("✓ Sent all 2 files here."),
        "{}",
        first.stdout
    );
    let again = run(&["backfill"], repo.path(), &env, None);
    assert_eq!(again.code, 0, "stderr: {}", again.stderr);
    assert!(
        again
            .stdout
            .contains("✓ Nothing to send: the server already has all 2 files."),
        "{}",
        again.stdout
    );
    for out in [&first.stdout, &again.stdout] {
        assert!(!out.contains(" 0 "), "a zero that is not the answer: {out}");
    }
}

/// A promotion says where the note went, one step a line, and closes with
/// who will see it now.
#[test]
fn promote_says_where_the_note_went_and_who_sees_it() {
    let server = live_server("right");
    let repo = git_repo();
    let env = [
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
        ("RECALL_GLOBAL_KEY", "eko"),
    ];
    let memory_dir = status_json(repo.path(), &env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        Path::new(&memory_dir).join("user.md"),
        "---\nname: user\ndescription: who I am\n---\n\nA person.\n",
    )
    .unwrap();

    let r = run(&["promote", "user.md"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("✓ Moved user.md to global/user.md"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout
            .contains("✓ Every other project picks it up at its next session start."),
        "{}",
        r.stdout
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
    // A table under its header, no line ending in spaces, and the key's
    // fingerprint cut short: it is whole in --json.
    assert!(text.stdout.contains("NAME"), "{}", text.stdout);
    assert!(
        text.stdout.lines().all(|l| l == l.trim_end()),
        "{}",
        text.stdout
    );
    let fingerprint = doc["devices"][0]["fingerprint"].as_str().unwrap();
    let bare = fingerprint.trim_start_matches("SHA256:");
    assert!(
        text.stdout.contains(&format!("{}…", &bare[..8])) && !text.stdout.contains(bare),
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

/// `approve --worker` approves a merge worker with the worker scope, and
/// asking for two scopes at once is refused before anything is sent.
#[test]
fn devices_approve_can_make_a_worker() {
    use recall_hooks::client::{Client, Poll};
    let server = live_server("right");
    let repo = git_repo();
    let (_key, pending) = pending_enrollment(&server.url, "worker");
    let open = Client::new(&server.url, "").unwrap();
    let env = [
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];

    let r = run(
        &[
            "devices",
            "approve",
            &pending.user_code,
            "--worker",
            "--admin",
            "--yes",
        ],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(!matches!(
        block_on(open.poll(&pending.enrollment_id)).unwrap(),
        Poll::Approved(_)
    ));

    let r = run(
        &[
            "devices",
            "approve",
            &pending.user_code,
            "--worker",
            "--yes",
        ],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("Approved worker (worker)"),
        "stdout: {}",
        r.stdout
    );
    match block_on(open.poll(&pending.enrollment_id)).unwrap() {
        Poll::Approved(approved) => assert_eq!(approved.scope, "worker"),
        other => panic!("expected approval, got {other:?}"),
    }
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

/// `authkey create` shows the key once, alone on a line of its own so it
/// can be copied whole, and says how to revoke it; `list` puts the keys
/// that still enrol first and a revoked one after them, marked; `revoke`
/// names the command that revokes its devices too.
#[test]
fn authkey_create_shows_the_key_alone_and_list_marks_a_revoked_one() {
    let server = live_server("right");
    let repo = git_repo();
    let env = [
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];

    let made = run(
        &["authkey", "create", "--tag", "cloud", "--expires", "90d"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(made.code, 0, "stderr: {}", made.stderr);
    let keys = block_on(operator(&server).authkeys()).unwrap().authkeys;
    let id = keys[0].id.clone();
    let key = made
        .stdout
        .lines()
        .find(|l| l.starts_with("recall-ak-"))
        .unwrap_or_else(|| panic!("the key on a line of its own: {}", made.stdout));
    assert_eq!(key.trim_end(), key, "{}", made.stdout);
    assert!(
        made.stdout.contains("expires in 90 days"),
        "{}",
        made.stdout
    );
    assert!(
        made.stdout
            .contains(&format!("→ recall authkey revoke {id}")),
        "{}",
        made.stdout
    );

    let other = block_on(
        operator(&server).create_authkey(&recall_wire::AuthkeyRequest {
            tag: "old".into(),
            expires_in_days: 1,
            ephemeral: true,
            max_devices: None,
        }),
    )
    .unwrap();
    let revoked = run(&["authkey", "revoke", &other.id], repo.path(), &env, None);
    assert_eq!(revoked.code, 0, "stderr: {}", revoked.stderr);
    assert!(
        revoked.stdout.contains(&format!(
            "→ recall authkey revoke {} --revoke-devices",
            other.id
        )),
        "{}",
        revoked.stdout
    );

    let list = run(&["authkey", "list"], repo.path(), &env, None);
    assert_eq!(list.code, 0, "stderr: {}", list.stderr);
    let live = list.stdout.find(&id).expect("the live key is listed");
    let gone = list
        .stdout
        .find(&other.id)
        .expect("the revoked key is listed");
    assert!(live < gone, "the live key first: {}", list.stdout);
    let row = list.stdout.lines().find(|l| l.contains(&other.id)).unwrap();
    assert!(
        row.trim_start().starts_with('○') && row.contains("revoked just now"),
        "{}",
        list.stdout
    );
    assert!(list.stdout.contains("1 in use"), "{}", list.stdout);

    let missing = run(&["authkey", "revoke", "ak_nope"], repo.path(), &env, None);
    assert_eq!(missing.code, 2, "{}", missing.stderr);
    assert!(
        missing.stderr.starts_with("recall authkey: "),
        "named after the command run: {}",
        missing.stderr
    );
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

/// The case that lost memory, end to end against a real server: a note
/// edited through the shell (no hook fires) survives the next pull, which a
/// session start, resume or compaction runs, because the pull sends it
/// first; and `recall sync` does the same by hand, mid-session.
#[test]
fn a_shell_edit_survives_the_next_pull_and_recall_sync_sends_one() {
    let server = live_server("right");
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];
    let r = push_memory(&repo, &env, "notes.md", "v1\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let memory_dir = write_memory(repo.path(), &env, "notes.md", "v1\nfrom the shell\n");
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("first sent 1 local change(s)"),
        "stderr: {}",
        r.stderr
    );
    let notes = Path::new(&memory_dir).join("notes.md");
    assert_eq!(
        std::fs::read_to_string(&notes).unwrap(),
        "v1\nfrom the shell\n",
        "the pull overwrote an edit no hook had seen"
    );
    let files = stored(&server, &repo, &env);
    let stored_notes = files.iter().find(|f| f.file_path == "notes.md").unwrap();
    assert_eq!(
        stored_notes.content.as_deref(),
        Some("v1\nfrom the shell\n")
    );

    std::fs::write(Path::new(&memory_dir).join("later.md"), "made with cat\n").unwrap();
    let r = run(&["sync"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("recall-sync: "), "stderr: {}", r.stderr);
    let files = stored(&server, &repo, &env);
    assert!(
        files.iter().any(|f| f.file_path == "later.md"),
        "recall sync did not send a file created through the shell"
    );
}

/// A hook must never fail a session, but `recall sync` is a command someone
/// ran, so it says when it could not sync.
#[test]
fn recall_sync_exits_non_zero_when_it_cannot_reach_the_server() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", "http://127.0.0.1:9"),
        ("RECALL_TOKEN", "t"),
    ];
    let r = run(&["sync"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("recall-sync:"), "stderr: {}", r.stderr);
    assert_eq!(run(&["pull"], repo.path(), &env, None).code, 0);
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
    // Everything that takes time is ready before the server starts, so the
    // first signature comes as soon after its start as it can, well inside
    // the refusal (see `live_server_just_started`).
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let server = live_server_just_started("right");
    let key = block_on(
        operator(&server).create_authkey(&recall_wire::AuthkeyRequest {
            tag: "cloud".into(),
            expires_in_days: 1,
            ephemeral: true,
            max_devices: None,
        }),
    )
    .unwrap();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_AUTHKEY", key.key.as_str()),
    ];

    // Enrolling is unsigned; the pull after it is the first signature.
    let started = std::time::Instant::now();
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

// ---------------------------------------------------------------------------
// the audit log: this machine as its witness, export and offline verify
// ---------------------------------------------------------------------------

/// The record `audit.json` in `home` holds for `server`.
fn witnessed(home: &Path, server: &LiveServer) -> recall_hooks::audit::Saved {
    recall_hooks::audit::Witness::new(home.join("audit.json"), &server.url)
        .load()
        .unwrap()
}

/// `scripts/audit-verify.py` over `file`, with the built-in Ed25519 so what
/// is tested does not depend on what this machine has installed: its exit
/// code and all it printed.
fn audit_verify_py(file: &Path, args: &[String]) -> (i32, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/audit-verify.py");
    let out = Command::new("python3")
        .arg(script)
        .arg(file)
        .arg("--ed25519=builtin")
        .args(args)
        .output()
        .expect("python3 must be on PATH");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// Rewrites the first leaf of the audit log in the database at `db` that
/// holds `from`, and its stored hash to match, so the server starts on it:
/// what someone holding the database file can do, and what only a
/// checkpoint saved elsewhere can show.
fn rewrite_leaf(db: &Path, from: &str, to: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute_batch("DROP TRIGGER audit_log_no_update")
        .unwrap();
    let leaves: Vec<(i64, Vec<u8>)> = conn
        .prepare("SELECT seq, leaf FROM audit_log ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let (seq, leaf) = leaves
        .into_iter()
        .find(|(_, l)| String::from_utf8_lossy(l).contains(from))
        .unwrap_or_else(|| panic!("no leaf holds {from}"));
    let forged = String::from_utf8(leaf).unwrap().replacen(from, to, 1);
    let hash = recall_wire::audit::merkle::hash_leaf(forged.as_bytes());
    conn.execute(
        "UPDATE audit_log SET leaf = ?1, leaf_hash = ?2 WHERE seq = ?3",
        (forged.as_bytes(), &hash[..], seq),
    )
    .unwrap();
}

/// The whole round: pulls save checkpoints, an admin device exports the
/// log, `recall audit verify` and the script both accept the export and the
/// checkpoints this machine saved, and both refuse it with a byte changed,
/// a leaf missing or two swapped.
#[test]
fn an_export_verifies_here_and_with_the_script_and_tampering_does_not() {
    let server = live_server("right");
    let repo = git_repo();
    let home = enrolled(&server, &repo, "laptop");
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];

    let r = push_memory(&repo, &env, "fact.md", "A fact.\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let saved = witnessed(home.path(), &server);
    assert!(
        !saved.unchecked.is_empty(),
        "every pull saves the checkpoint it carries: {saved:?}"
    );

    let file = home.path().join("audit.jsonl");
    let file_str = file.to_string_lossy().to_string();
    let r = run(
        &["audit", "export", "-o", &file_str],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("wrote"), "stderr: {}", r.stderr);
    assert!(
        r.stderr.contains("it extends the"),
        "the export held the saved checkpoints to its leaves: {}",
        r.stderr
    );
    let saved = witnessed(home.path(), &server);
    assert!(saved.unchecked.is_empty(), "now proven: {saved:?}");

    let r = run(&["audit", "verify", &file_str], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.starts_with("recall audit verify"), "{}", r.stdout);
    assert!(r.stdout.contains("every one checked"), "{}", r.stdout);
    assert!(
        r.stdout.contains("saved here for 127.0.0.1:"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.trim_end().ends_with("✓ The export checks out."),
        "{}",
        r.stdout
    );

    // The script reads the same file, held to the same checkpoints.
    let checkpoints: Vec<String> = saved
        .all()
        .iter()
        .map(|c| format!("--checkpoint={}", c.header().replacen(' ', ":", 1)))
        .collect();
    let (code, out) = audit_verify_py(&file, &checkpoints);
    assert_eq!(code, 0, "{out}");

    let export = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = export.lines().collect();
    let mut dropped = lines.clone();
    dropped.remove(3);
    let mut swapped = lines.clone();
    swapped.swap(2, 3);
    for (what, forged) in [
        ("a changed byte", export.replacen("fact.md", "fakt.md", 1)),
        ("a missing leaf", dropped.join("\n") + "\n"),
        ("two swapped leaves", swapped.join("\n") + "\n"),
    ] {
        let path = home.path().join("forged.jsonl");
        std::fs::write(&path, forged).unwrap();
        let path_str = path.to_string_lossy().to_string();
        let r = run(&["audit", "verify", &path_str], repo.path(), &env, None);
        assert_eq!(r.code, 1, "{what} was accepted: {}", r.stdout);
        assert!(
            r.stderr.contains("✗ The export does not check out"),
            "{what}: {}",
            r.stderr
        );
        // Each root in a problem is cut short, so the problem reads in a
        // line: none of the checkpoints' 44 characters of base64 is left.
        for cp in saved.all() {
            let root = cp.header().split_once(' ').unwrap().1.to_string();
            assert!(!r.stderr.contains(&root), "{what}: {}", r.stderr);
        }
        let (code, out) = audit_verify_py(&path, &checkpoints);
        assert_eq!(code, 1, "{what}, the script: {out}");
    }

    // And with no file, the server proves it to any credential.
    let r = run(&["audit", "verify"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("extends every checkpoint saved here"),
        "{}",
        r.stdout
    );
}

/// The other way round: an export the script accepts, the 0.4.2 capture of
/// a real server's log, is accepted here, and one it refuses is refused.
#[test]
fn a_file_the_script_accepts_is_accepted_here() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../recall-wire/fixtures/wire/0.4.2/audit_entries_response.json");
    let page: recall_wire::AuditEntriesResponse =
        serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
    let checkpoint: recall_wire::AuditCheckpoint = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../recall-wire/fixtures/wire/0.4.2/audit_checkpoint_response.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let mut export = format!("{}\n", checkpoint.to_header_value());
    for leaf in &page.entries {
        export.push_str(leaf);
        export.push('\n');
    }
    let file = dir.path().join("captured.jsonl");
    std::fs::write(&file, &export).unwrap();
    let file_str = file.to_string_lossy().to_string();

    let (code, out) = audit_verify_py(&file, &[]);
    assert_eq!(code, 0, "{out}");
    let r = run(&["audit", "verify", &file_str], dir.path(), &[], None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(r.stdout.contains("leaves       10,"), "{}", r.stdout);
    assert!(r.stdout.contains("on 1 signed leaf"), "{}", r.stdout);
    assert!(
        r.stdout.contains("none saved here or given"),
        "{}",
        r.stdout
    );

    // A saved checkpoint given by hand, as to the script.
    let early = format!(
        "--checkpoint={}",
        checkpoint.to_header_value().replacen(' ', ":", 1)
    );
    let r = run(
        &["audit", "verify", &file_str, &early],
        dir.path(),
        &[],
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    // The push's signature, one bit off: both refuse it.
    let push = page
        .entries
        .iter()
        .find(|l| l.contains("\"signature\":\"x4bs"))
        .unwrap();
    let forged = export.replacen(
        push,
        &push.replacen("\"signature\":\"x4bs", "\"signature\":\"y4bs", 1),
        1,
    );
    std::fs::write(&file, forged).unwrap();
    let (code, out) = audit_verify_py(&file, &[]);
    assert_eq!(code, 1, "{out}");
    let r = run(&["audit", "verify", &file_str], dir.path(), &[], None);
    assert_eq!(r.code, 1, "stdout: {}", r.stdout);
    assert!(r.stderr.contains("does not verify"), "stderr: {}", r.stderr);

    // A file that is not there, or a checkpoint that is not one, cannot be
    // checked at all: 2, as the script says it.
    let r = run(&["audit", "verify", "no-such.jsonl"], dir.path(), &[], None);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    let r = run(
        &["audit", "verify", &file_str, "--checkpoint", "12:nope"],
        dir.path(),
        &[],
        None,
    );
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
}

/// Someone with the server's database rewrites a push already witnessed,
/// and restarts it. The hooks carry on, exit 0, and save what they are
/// shown; `recall doctor` catches it and fails, `recall status` shows it,
/// every session start says so, and it stays caught until reset.
#[test]
fn a_server_that_rewrote_its_history_is_caught_and_stays_caught() {
    let server = live_server("right");
    let repo = git_repo();
    let home = enrolled(&server, &repo, "laptop");
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];

    let r = push_memory(&repo, &env, "fact.md", "A fact.\n");
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let before = doctor_finding(repo.path(), &env, "audit log");
    assert_eq!(before["level"], "ok", "{before}");
    assert!(
        before["detail"]
            .as_str()
            .unwrap()
            .contains("extends every checkpoint saved here"),
        "{before}"
    );

    let server = server.restart_after(|db| {
        rewrite_leaf(db, "\"file_path\":\"fact.md\"", "\"file_path\":\"fake.md\"")
    });

    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "a hook never fails for this: {}", r.stderr);

    let r = run(&["doctor", "--json"], repo.path(), &env, None);
    assert_ne!(r.code, 0, "doctor fails on it: {}", r.stdout);
    let found = doctor_finding(repo.path(), &env, "audit log");
    assert_eq!(found["level"], "fail", "{found}");
    assert!(
        found["detail"]
            .as_str()
            .unwrap()
            .contains("no longer extends"),
        "{found}"
    );
    assert!(
        found["fix"]
            .as_str()
            .unwrap()
            .contains("recall audit reset"),
        "{found}"
    );

    let status = status_json(repo.path(), &env);
    assert!(
        status["audit"]["inconsistent"]["detail"].is_string(),
        "status shows it: {}",
        status["audit"]
    );
    assert_eq!(status["audit"]["extends"], false);
    let r = run(&["status"], repo.path(), &env, None);
    assert!(
        r.stdout.contains("✗ audit log")
            && words(&r.stdout).contains("rewritten: the server's log no longer extends"),
        "{}",
        r.stdout
    );
    assert!(
        fix_after(&r.stdout, "✗ audit log").is_some()
            && words(&r.stdout).contains("recall audit reset"),
        "{}",
        r.stdout
    );

    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stderr
            .contains("WARNING: the server's audit log no longer extends a checkpoint"),
        "every session start says so: {}",
        r.stderr
    );

    let r = run(&["audit", "verify"], repo.path(), &env, None);
    assert_eq!(r.code, 1, "stdout: {}", r.stdout);
    assert!(
        r.stderr
            .contains("✗ the server's audit log no longer extends a checkpoint"),
        "{}",
        r.stderr
    );
    // The two ways out, each a command of its own.
    assert!(
        r.stderr.contains("→ recall audit reset")
            && r.stderr
                .contains("→ recall audit export -o audit-evidence.jsonl"),
        "{}",
        r.stderr
    );

    // Kept through a server that looks fine again, until reset.
    assert!(witnessed(home.path(), &server).inconsistent.is_some());
    let r = run(&["audit", "reset"], repo.path(), &env, None);
    assert_ne!(r.code, 0, "not without --yes or a terminal: {}", r.stderr);
    assert!(witnessed(home.path(), &server).inconsistent.is_some());
    let r = run(&["audit", "reset", "--yes"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("Forgot") && r.stdout.contains("and the rewrite found"),
        "{}",
        r.stdout
    );
    let after = doctor_finding(repo.path(), &env, "audit log");
    assert_eq!(after["level"], "ok", "{after}");
}

/// The leaves are admin-only. A sync device has no business reading
/// them, and is told what would; the proof anyone may ask for still works.
#[test]
fn an_export_needs_an_admin_and_says_so() {
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
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);

    let r = run(&["audit", "export"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stdout.is_empty(), "nothing half-written: {}", r.stdout);
    assert!(
        r.stderr
            .contains("needs an admin device or the server's RECALL_TOKEN")
            && r.stderr.contains("a sync device"),
        "stderr: {}",
        r.stderr
    );

    let r = run(&["audit", "verify"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
}

/// A root, standard base64, for checkpoints a test makes up.
const A_ROOT: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";

/// Saves `sizes` as checkpoints this machine saw from the server at `url`,
/// in `home`, as pulls would have.
fn saw_checkpoints(home: &Path, url: &str, sizes: std::ops::RangeInclusive<u64>) {
    let witness = recall_hooks::audit::Witness::new(home.join("audit.json"), url);
    for size in sizes {
        witness.record(&format!("{size} {A_ROOT}")).unwrap();
    }
}

/// A server whose `/health` answers and whose discovery document fails,
/// with an audit log of five leaves whose root is [`A_ROOT`].
fn discovery_down(seen: &Seen) -> (u16, serde_json::Value) {
    match seen.path.as_str() {
        "/health" => (
            200,
            serde_json::to_value(recall_wire::Health {
                status: "ok".into(),
                ..Default::default()
            })
            .unwrap(),
        ),
        "/.well-known/recall" => (500, serde_json::json!({ "error": "boom" })),
        "/sync" => (
            200,
            serde_json::json!({ "project_key": "acme/app", "files": [] }),
        ),
        "/v1/audit/checkpoint" => (
            200,
            serde_json::json!({ "tree_size": 5, "root_hash": A_ROOT }),
        ),
        _ => (404, serde_json::json!({ "error": "not found" })),
    }
}

/// Discovery failing is no reason to leave saved checkpoints unproven: the
/// audit routes answer for themselves. Mutation: check only a server whose
/// discovery document lists the log, as before.
#[test]
fn saved_checkpoints_are_checked_when_discovery_fails() {
    let server = fake_server(discovery_down);
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    saw_checkpoints(home.path(), &server.url, 5..=5);
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];
    let status = status_json(repo.path(), &env);
    assert_eq!(status["audit"]["extends"], true, "{}", status["audit"]);
    assert!(
        server
            .seen()
            .iter()
            .any(|s| s.path == "/v1/audit/checkpoint"),
        "the server was asked"
    );
}

/// `recall audit verify` with no file, against a server that answers the
/// audit routes 404: a log this machine saw it keep is a history lost, 1,
/// as doctor fails it; a log never seen is not there to check, 2.
/// Mutation: answer 2 for both.
#[test]
fn a_log_that_went_away_fails_verify_and_one_never_kept_cannot_be_checked() {
    let server = fake_server(|_| (404, serde_json::json!({ "error": "not found" })));
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];
    let r = run(&["audit", "verify"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "never kept: {}", r.stderr);
    saw_checkpoints(home.path(), &server.url, 5..=5);
    let r = run(&["audit", "verify"], repo.path(), &env, None);
    assert_eq!(r.code, 1, "went away: {}", r.stderr);
    assert!(r.stderr.contains("keeps no audit log"), "{}", r.stderr);
}

/// An export says the same: a log never kept cannot be exported (2), and
/// one this machine saw kept and the server no longer keeps is a history
/// lost (1). Mutation: 2 whatever was saved, as before.
#[test]
fn an_export_of_a_log_that_went_away_fails() {
    let server = fake_server(|_| (404, serde_json::json!({ "error": "not found" })));
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];
    let r = run(&["audit", "export"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "never kept: {}", r.stderr);
    saw_checkpoints(home.path(), &server.url, 5..=5);
    let r = run(&["audit", "export"], repo.path(), &env, None);
    assert_eq!(r.code, 1, "went away: {}", r.stderr);
    assert!(r.stderr.contains("keeps no audit log"), "{}", r.stderr);
}

/// A credential the server refuses for the audit routes is "could not be
/// checked" (2), said with what to do about the credential, not as a
/// server that would not prove. Mutation: report it as any unanswered
/// check.
#[test]
fn a_refused_credential_says_what_to_do_about_it() {
    let server = fake_server(|seen| match seen.path.as_str() {
        "/v1/audit/checkpoint" => (403, serde_json::json!({ "error": "forbidden" })),
        _ => (404, serde_json::json!({ "error": "not found" })),
    });
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", server.url.as_str()),
        ("RECALL_TOKEN", "right"),
    ];
    saw_checkpoints(home.path(), &server.url, 5..=5);
    let r = run(&["audit", "verify"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(
        r.stderr.contains("refused this machine's credential"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("recall connect"), "{}", r.stderr);
}

/// `reset` never forgets what it cannot read: an `audit.json` that cannot
/// be read is "could not" (2), as install.md says, and is left as it was.
/// Mutation: answer 1, as for a reset declined.
#[test]
fn reset_cannot_forget_what_it_cannot_read() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let file = home.path().join("audit.json");
    std::fs::write(&file, "{ not json").unwrap();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", DEAD_SERVER),
    ];
    let r = run(&["audit", "reset", "--yes"], repo.path(), &env, None);
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert_eq!(std::fs::read(&file).unwrap(), b"{ not json");
}

/// Not set up to ask is "could not be checked", 2, as install.md says: no
/// server, and no credential. Mutation: 1, as before.
#[test]
fn not_set_up_to_ask_cannot_be_checked() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let r = run(
        &["audit", "verify"],
        repo.path(),
        &[("RECALL_HOME", &home_str)],
        None,
    );
    assert_eq!(r.code, 2, "no server: {}", r.stderr);
    let r = run(
        &["audit", "export"],
        repo.path(),
        &[("RECALL_HOME", &home_str), ("RECALL_URL", DEAD_SERVER)],
        None,
    );
    assert_eq!(r.code, 2, "no credential: {}", r.stderr);
}

/// The pull hook says what the session should know about the witness, and
/// still exits 0: more than a handful waiting to be checked, and a record
/// it cannot read, which doctor then fails on. Mutation: say nothing of
/// either.
#[test]
fn the_pull_hook_nudges_and_warns_about_the_audit_record() {
    let repo = git_repo();
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let env = [
        ("RECALL_HOME", home_str.as_str()),
        ("RECALL_URL", DEAD_SERVER),
        ("RECALL_TOKEN", "right"),
    ];
    saw_checkpoints(home.path(), DEAD_SERVER, 1..=16);
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0);
    assert!(!r.stderr.contains("wait to be checked"), "{}", r.stderr);
    saw_checkpoints(home.path(), DEAD_SERVER, 17..=17);
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0);
    assert!(
        r.stderr
            .contains("17 audit checkpoints wait to be checked; recall doctor checks them"),
        "{}",
        r.stderr
    );

    std::fs::write(home.path().join("audit.json"), "{ not json").unwrap();
    let r = run(&["pull"], repo.path(), &env, None);
    assert_eq!(r.code, 0);
    assert!(
        r.stderr.contains("may hold the only record of a rewrite"),
        "{}",
        r.stderr
    );
    let found = doctor_finding(repo.path(), &env, "audit log");
    assert_eq!(found["level"], "fail", "{found}");
}

/// An export that cannot hold itself to the checkpoints saved here, because
/// `audit.json` cannot be read, does not pass: it says so and exits 2, and
/// leaves the file as it was. Mutation: say so and exit 0, as before.
#[test]
fn an_export_that_cannot_check_the_saved_checkpoints_does_not_pass() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];
    std::fs::write(home.path().join("audit.json"), "{ not json").unwrap();
    let out = home.path().join("audit.jsonl");
    let out_str = out.to_string_lossy().to_string();
    let r = run(
        &["audit", "export", "-o", &out_str],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 2, "stderr: {}", r.stderr);
    assert!(r.stderr.contains("could not be checked"), "{}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(home.path().join("audit.json")).unwrap(),
        "{ not json"
    );
}

/// An export writes through no link left in its way: a `.partial` that is
/// a symlink is removed, not followed to whatever it points at. Mutation:
/// create the file by opening that name.
#[cfg(unix)]
#[test]
fn an_export_never_writes_through_a_link_in_its_way() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];
    let victim = home.path().join("precious.txt");
    std::fs::write(&victim, "precious").unwrap();
    let out = home.path().join("audit.jsonl");
    std::os::unix::fs::symlink(&victim, home.path().join("audit.jsonl.partial")).unwrap();

    let out_str = out.to_string_lossy().to_string();
    let r = run(
        &["audit", "export", "-o", &out_str],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
    let written = std::fs::read_to_string(&out).unwrap();
    assert!(written.split_once(' ').is_some(), "{written}");
    assert!(!home.path().join("audit.jsonl.partial").exists());
}

// ---------------------------------------------------------------------------
// eval
// ---------------------------------------------------------------------------

/// A finished report, as the worker leaves one, written straight into the
/// server's database: the command is what is under test here, and the
/// worker's side has tests of its own
/// (`crates/recall-server/tests/evaluations.rs`). One finding, a secret on
/// line 2 of `deploy.md`, whose suggested edit masks it.
fn plant_report(server: &LiveServer, project_key: &str, content: &str) {
    let edit = serde_json::json!({"project_key": project_key, "file_path": "deploy.md",
                      "base_sha256": recall_wire::content_sha256(content),
                      "lines": [2, 2], "replacement": "- key: [removed]\n"});
    let findings = serde_json::json!([{"id": "f1", "kind": "secret", "severity": "high",
                           "project_key": project_key, "file_path": "deploy.md",
                           "lines": [2, 2], "related": []}]);
    let details = serde_json::json!({"findings": {"f1": {"excerpt": "- key: abc1… (masked)\n",
                                             "reasoning": "A key is kept in memory.",
                                             "suggested_edit": edit}},
                         "skipped": []});
    let conn = rusqlite::Connection::open(&server.cfg.db_path).unwrap();
    conn.execute(
        "INSERT INTO evaluations (id, state, job_id, projects, contradictions, findings, details,
                                  created_at, finished_at)
         VALUES ('eval_cli', 'done', 'job_cli', '[]', 0, ?1, ?2,
                 '2026-09-25T10:00:00.000Z', '2026-09-25T10:01:00.000Z')",
        (findings.to_string(), details.to_string()),
    )
    .unwrap();
}

/// `apply` makes a finding's suggested edit to the local file and pushes
/// it; it refuses once the file is no longer the version the report read,
/// and a finding that is not there. `list` and `show` say what there is,
/// and asking for a run with no worker enrolled is the server's refusal.
#[test]
fn eval_apply_makes_the_suggested_edit_and_pushes_it() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];
    let content = "# Deploy\n- key: abc123\n- ship it\n";
    let pushed = push_memory(&repo, &env, "deploy.md", content);
    assert_eq!(pushed.code, 0, "{}", pushed.stderr);
    let key = status_json(repo.path(), &env)["project_key"]
        .as_str()
        .unwrap()
        .to_string();
    plant_report(&server, &key, content);

    let list = run(&["eval", "list"], repo.path(), &env, None);
    assert_eq!(list.code, 0, "{}", list.stderr);
    assert!(
        list.stdout.contains("eval_cli") && list.stdout.contains("1 secret"),
        "{}",
        list.stdout
    );
    assert!(
        list.stdout.contains("! eval_cli") && list.stdout.contains("→ recall eval show eval_cli"),
        "{}",
        list.stdout
    );
    let show = run(&["eval", "show", "eval_cli"], repo.path(), &env, None);
    assert_eq!(show.code, 0, "{}", show.stderr);
    for want in [
        // The summary first, then the finding by its id, its severity
        // marked, then what it quotes and the command that applies it.
        "✗ 1 high   ! 0 medium   ○ 0 low",
        "✗ f1  secret  deploy.md L2",
        "│ - key: abc1… (masked)",
        "→ recall eval apply f1 --eval eval_cli",
    ] {
        assert!(show.stdout.contains(want), "{want:?} in {}", show.stdout);
    }
    assert!(
        show.stdout.lines().all(|l| l == l.trim_end()),
        "{}",
        show.stdout
    );

    let applied = run(&["eval", "apply", "f1", "--yes"], repo.path(), &env, None);
    assert_eq!(applied.code, 0, "{}", applied.stderr);
    let memory_dir = status_json(repo.path(), &env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    let fixed = "# Deploy\n- key: [removed]\n- ship it\n";
    assert_eq!(
        std::fs::read_to_string(Path::new(&memory_dir).join("deploy.md")).unwrap(),
        fixed
    );
    let on_server = stored(&server, &repo, &env);
    let file = on_server
        .iter()
        .find(|f| f.file_path == "deploy.md")
        .unwrap();
    assert_eq!(file.content.as_deref(), Some(fixed));

    let again = run(&["eval", "apply", "f1", "--yes"], repo.path(), &env, None);
    assert_eq!(again.code, 1, "{}", again.stdout);
    assert!(
        again.stderr.contains("has changed since"),
        "{}",
        again.stderr
    );
    let missing = run(
        &["eval", "apply", "f9", "--eval", "eval_cli", "--yes"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("has no finding f9"),
        "{}",
        missing.stderr
    );

    let asked = run(&["eval", "run"], repo.path(), &env, None);
    assert_eq!(asked.code, 2, "{}", asked.stdout);
    assert!(
        asked.stderr.contains("no worker is enrolled"),
        "{}",
        asked.stderr
    );
}

// ---------------------------------------------------------------------------
// recall review
// ---------------------------------------------------------------------------

/// A fixture modelled on `docs/history/memory-truth.md`'s "Why", and on this
/// repository's own history (`git log --diff-filter=D -- '*lib.sh'`, commit
/// `5b828c1`): a git repository that once had `hooks/lib.sh` and
/// `hooks/recall-pull`, deleted both in a later commit (the Rust rewrite),
/// and still tracks `docs/plan.md`. `lib.sh` lives under `hooks/`, not at
/// the root — the real file did too — so a note that names it by its bare
/// filename, the way people actually write, still has to be found by a
/// glob, not a root-only lookup.
fn review_repo() -> Repo {
    let repo = git_repo();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success());
    };
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::create_dir_all(repo.path().join("hooks")).unwrap();
    std::fs::write(repo.path().join("hooks").join("lib.sh"), "echo hi\n").unwrap();
    std::fs::write(repo.path().join("hooks").join("recall-pull"), "echo pull\n").unwrap();
    std::fs::create_dir_all(repo.path().join("docs")).unwrap();
    std::fs::write(repo.path().join("docs").join("plan.md"), "# Plan\n").unwrap();
    git(&["add", "-A"]);
    git(&[
        "commit",
        "-q",
        "-m",
        "add hooks/lib.sh and hooks/recall-pull",
    ]);
    git(&["rm", "-q", "hooks/lib.sh"]);
    git(&["rm", "-q", "hooks/recall-pull"]);
    git(&["commit", "-q", "-m", "remove the old hooks (Rust rewrite)"]);
    repo
}

/// Writes a memory file at `rel`, relative to the memory directory `status
/// --json` reports for `repo`, creating parent directories as needed.
fn write_memory(repo: &Path, env: &[(&str, &str)], rel: &str, body: &str) -> String {
    let memory_dir = status_json(repo, env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    let path = Path::new(&memory_dir).join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    memory_dir
}

/// The claim in `claims` (a `--json` `claims[]` array) whose `text` contains
/// `needle`, failing with the whole array when there is none — an
/// index/`find` panic would not say what was actually extracted.
fn claim_containing<'a>(claims: &'a serde_json::Value, needle: &str) -> &'a serde_json::Value {
    claims
        .as_array()
        .unwrap_or_else(|| panic!("claims[] is not an array: {claims}"))
        .iter()
        .find(|c| c["text"].as_str().is_some_and(|t| t.contains(needle)))
        .unwrap_or_else(|| panic!("no claim contains {needle:?}: {claims}"))
}

/// The design's own worked example (`docs/history/memory-truth.md`'s Layer 2
/// section), rebuilt as a fixture: `lib.sh` and `hooks/recall-pull` named as
/// present, deleted in git history since; a present claim naming a hostname
/// this PR cannot check; and a history-section sentence that names the same
/// two dead paths and must never be flagged, whatever the evidence.
///
/// Mutation this pins against (design's test table, row 1): dropping the
/// git-history lookup (`Repo::deleted`) would turn the `stale` verdict below
/// into `cant_tell` — verified by hand while developing this test, not left
/// in the shipped code.
#[test]
fn the_phase1_deploy_fixture_gets_the_documented_verdicts() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "project_phase1_deploy.md",
        "---\nname: project-phase1-deploy\n---\n\n\
         - Recall's server is live at `recall.pimlabs.id`, deployed via OrbStack and a Cloudflare Tunnel.\n\
         - Run `lib.sh` to start the legacy hooks; `hooks/recall-pull` runs at session start.\n\
         \n\
         ## History\n\
         \n\
         - Not the old `hooks/recall-pull` script, and not `lib.sh`: both were deleted in the Rust rewrite.\n",
    );

    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(
        r.code, 0,
        "review must exit 0 whenever it ran: {}",
        r.stderr
    );
    let report: serde_json::Value =
        serde_json::from_str(&r.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", r.stdout));
    let claims = &report["claims"];

    let hooks = claim_containing(claims, "Run `lib.sh`");
    assert_eq!(hooks["class"], "present");
    assert_eq!(hooks["verdict"], "stale", "{hooks}");
    let evidence: Vec<&str> = hooks["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["detail"].as_str().unwrap())
        .collect();
    assert!(
        evidence
            .iter()
            .any(|d| d.contains("lib.sh") && d.contains("deleted")),
        "{evidence:?}"
    );
    assert!(
        evidence
            .iter()
            .any(|d| d.contains("hooks/recall-pull") && d.contains("deleted")),
        "{evidence:?}"
    );

    let hostname = claim_containing(claims, "is live at");
    assert_eq!(hostname["class"], "present");
    assert_eq!(hostname["verdict"], "cant_tell", "{hostname}");

    let history = claim_containing(claims, "Not the old");
    assert_eq!(history["class"], "record");
    assert!(
        history.get("verdict").is_none() || history["verdict"].is_null(),
        "a record must never carry a verdict: {history}"
    );

    let text = run(&["review", "run"], repo.path(), &env, None);
    assert_eq!(text.code, 0);
    assert!(text.stdout.contains("stale"), "{}", text.stdout);
    // The answer comes first, in words.
    assert!(text.stdout.contains("claim(s) need you"), "{}", text.stdout);
    assert!(
        text.stdout.contains("1 line(s) of history"),
        "records are counted, not listed: {}",
        text.stdout
    );
    // A count of nothing is left out rather than printed as a zero.
    assert!(
        !text.stdout.contains(" 0 line(s) of history") && !text.stdout.contains(" 0 with nothing"),
        "{}",
        text.stdout
    );
    // A stale claim says why, and both things to do about it, under it:
    // fix it when it is about now, dismiss it when it is history.
    for wanted in [
        "Why: this reads as how things are now",
        "→ If it is about now, fix it: edit L6 and write what is true now, then recall sync",
        "→ If it is history, right as written: recall review dismiss t2",
        "project_phase1_deploy.md  ",
        "What to do now",
        "1. t2 project_phase1_deploy.md L6: edit it (write what is true now),",
        "or recall review dismiss t2 if it is history",
        // What each of those commands does, once, at the end.
        "Commands",
        "recall review apply tN         Writes tN's suggested fix",
        "recall review dismiss tN       Marks tN as history",
        "recall sync                    Sends a note you edited outside Claude Code.",
    ] {
        assert!(text.stdout.contains(wanted), "{wanted:?}: {}", text.stdout);
    }
    // The record's own text — "Not the old `hooks/recall-pull` script" —
    // must not appear on a line the report marks stale.
    assert!(
        !text.stdout.contains("stale") || !text.stdout.contains("Not the old"),
        "{}",
        text.stdout
    );
}

/// `recall review dismiss`: a stale claim that is history, set aside. It
/// stays set aside across runs, is counted rather than listed, comes back
/// with `--undo`, and comes back by itself once its text changes.
#[test]
fn a_dismissed_claim_stops_needing_you_until_its_text_changes() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    let note = |line: &str| format!("---\nname: n\n---\n\n- {line}\n");
    write_memory(
        repo.path(),
        &env,
        "project_hooks.md",
        &note("Run `lib.sh` to start the hooks."),
    );
    let first = run(&["review", "run"], repo.path(), &env, None);
    assert!(
        first.stdout.contains("1 claim(s) need you"),
        "{}",
        first.stdout
    );

    // Nothing to dismiss before a review, or for an id it does not hold.
    let unknown = run(&["review", "dismiss", "t9"], repo.path(), &env, None);
    assert_ne!(unknown.code, 0);
    assert!(unknown.stderr.contains("no claim t9"), "{}", unknown.stderr);

    let d = run(&["review", "dismiss", "t1"], repo.path(), &env, None);
    assert_eq!(d.code, 0, "{}", d.stderr);
    assert!(d.stdout.contains("Dismissed t1"), "{}", d.stdout);
    assert!(
        d.stdout.contains("recall review dismiss t1 --undo"),
        "{}",
        d.stdout
    );

    // `show` sees it at once, and a new run keeps it.
    for args in [&["review", "show"][..], &["review", "run"][..]] {
        let r = run(args, repo.path(), &env, None);
        assert!(
            r.stdout.contains("Nothing to fix"),
            "{args:?}: {}",
            r.stdout
        );
        assert!(
            r.stdout.contains("1 you dismissed as history"),
            "{args:?}: {}",
            r.stdout
        );
        assert!(r.stdout.contains("✗ 0 stale"), "{args:?}: {}", r.stdout);
        assert!(r.stdout.contains("Nothing."), "{args:?}: {}", r.stdout);
    }
    let json = run(&["review", "show", "--json"], repo.path(), &env, None);
    let report: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "Run `lib.sh`");
    assert_eq!(claim["dismissed"], true, "{claim}");
    assert_eq!(
        claim["verdict"], "stale",
        "the verdict itself is kept: {claim}"
    );
    let details = run(&["review", "show", "--details"], repo.path(), &env, None);
    assert!(
        details
            .stdout
            .contains("dismissed as history; recall review dismiss t1 --undo flags it again"),
        "{}",
        details.stdout
    );

    // Undo brings it back.
    let undo = run(
        &["review", "dismiss", "t1", "--undo"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(undo.code, 0, "{}", undo.stderr);
    let back = run(&["review", "show"], repo.path(), &env, None);
    assert!(
        back.stdout.contains("1 claim(s) need you"),
        "{}",
        back.stdout
    );
    let again = run(
        &["review", "dismiss", "t1", "--undo"],
        repo.path(),
        &env,
        None,
    );
    assert_ne!(again.code, 0);
    assert!(again.stderr.contains("not dismissed"), "{}", again.stderr);

    // A change to the words is a new claim, checked again.
    run(&["review", "dismiss", "t1"], repo.path(), &env, None);
    write_memory(
        repo.path(),
        &env,
        "project_hooks.md",
        &note("Run `lib.sh` to start every hook."),
    );
    let changed = run(&["review", "run"], repo.path(), &env, None);
    assert!(
        changed.stdout.contains("1 claim(s) need you"),
        "{}",
        changed.stdout
    );
    // Only a flagged claim can be dismissed.
    write_memory(
        repo.path(),
        &env,
        "project_hooks.md",
        &note("Run `docs/plan.md` first."),
    );
    run(&["review", "run"], repo.path(), &env, None);
    let not_stale = run(&["review", "dismiss", "t1"], repo.path(), &env, None);
    assert_ne!(not_stale.code, 0);
    assert!(
        not_stale.stderr.contains("not stale"),
        "{}",
        not_stale.stderr
    );
}

/// A path git renamed, not only deleted, says what it is called now, and
/// the fix names both: the owner's `cut-release.yml`, which became
/// `start-release.yml`.
#[test]
fn a_renamed_path_says_what_to_write_instead() {
    let repo = review_repo();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success());
    };
    let workflows = repo.path().join(".github").join("workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::write(
        workflows.join("cut-release.yml"),
        "name: Cut a release\non: workflow_dispatch\njobs:\n  tag:\n    runs-on: ubuntu-latest\n",
    )
    .unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "add cut-release.yml"]);
    git(&[
        "mv",
        ".github/workflows/cut-release.yml",
        ".github/workflows/start-release.yml",
    ]);
    git(&["commit", "-q", "-m", "rename it"]);

    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "project_release.md",
        "---\nname: r\n---\n\n- The release workflow lives in `cut-release.yml` on main.\n",
    );
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "The release workflow");
    assert_eq!(claim["verdict"], "stale", "{claim}");
    let renamed = &claim["evidence"][0]["renamed"];
    assert_eq!(renamed["from"], "cut-release.yml", "{claim}");
    assert_eq!(renamed["to"], "start-release.yml", "{claim}");

    let text = run(&["review", "show"], repo.path(), &env, None);
    assert!(
        text.stdout
            .contains("`cut-release.yml` was renamed to `start-release.yml`"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout
            .contains("edit L5 and replace `cut-release.yml` with `start-release.yml`"),
        "{}",
        text.stdout
    );
}

/// A present claim whose stated version matches the newest release tag
/// gets `still_true`, and — the point of the test — it is never silently
/// dropped: counted in the default text, and listed with `--details`.
/// Mutation (design's test table): dropping it from either would make an
/// `assert!` on human output fail.
#[test]
fn a_still_true_claim_appears_in_both_outputs() {
    let repo = review_repo();
    assert!(Command::new("git")
        .args(["tag", "v0.1.0"])
        .current_dir(repo.path())
        .status()
        .unwrap()
        .success());
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "plan.md",
        "- Recall is at version `0.1.0`; the plan lives in `docs/plan.md`.\n",
    );

    let json = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(json.code, 0, "{}", json.stderr);
    let report: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "docs/plan.md");
    assert_eq!(claim["verdict"], "still_true", "{claim}");

    let text = run(&["review", "show"], repo.path(), &env, None);
    assert_eq!(text.code, 0);
    assert!(text.stdout.contains("✓ 1 still true"), "{}", text.stdout);
    assert!(text.stdout.contains("Nothing to fix"), "{}", text.stdout);
    assert!(text.stdout.contains("1 still hold"), "{}", text.stdout);

    let text = run(&["review", "show", "--details"], repo.path(), &env, None);
    assert_eq!(text.code, 0);
    assert!(text.stdout.contains("docs/plan.md"), "{}", text.stdout);
}

/// `[FILE]...` restricts the review to exactly the files named.
#[test]
fn the_files_argument_restricts_which_files_are_reviewed() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The plan lives in `docs/plan.md`.\n",
    );
    write_memory(repo.path(), &env, "b.md", "- Run `lib.sh` to start it.\n");

    let r = run(
        &["review", "run", "a.md", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let files: std::collections::HashSet<&str> = report["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, std::collections::HashSet::from(["a.md"]), "{report}");
}

/// `recall review show` reprints the last run's report, unchanged, without
/// asking anything of the repository again.
#[test]
fn show_reprints_the_last_report() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "plan.md",
        "- The plan lives in `docs/plan.md`.\n",
    );

    let ran = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(ran.code, 0, "{}", ran.stderr);

    let shown = run(&["review", "show", "--json"], repo.path(), &env, None);
    assert_eq!(shown.code, 0, "{}", shown.stderr);
    assert_eq!(shown.stdout, ran.stdout);
}

/// `recall review show` before any run has happened is a quiet no-op, the
/// same shape of answer every other read-only command gives to "nothing
/// has happened here yet".
#[test]
fn show_before_any_run_is_a_quiet_no_op() {
    let repo = git_repo();
    let r = run(&["review", "show"], repo.path(), &[], None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("No review yet"), "{}", r.stdout);
}

/// The review never needs `RECALL_URL`/`RECALL_TOKEN` at all: this PR reads
/// the checkout and git only. `run()` already spawns with a clean
/// environment (no `RECALL_*` at all), so this is really asserting that
/// `recall review` does not go looking for them and fail for want of a
/// server, the way `recall eval` does.
#[test]
fn review_never_needs_a_server() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "plan.md",
        "- The plan lives in `docs/plan.md`.\n",
    );
    let r = run(&["review", "run"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!r.stderr.contains("RECALL_URL"), "{}", r.stderr);
}

/// Naming a `[FILE]` that is not a memory file in any scope that is on is
/// the one usage mistake this command refuses, per its own module doc.
#[test]
fn a_named_file_that_is_not_memory_errors() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The plan lives in `docs/plan.md`.\n",
    );
    let r = run(&["review", "run", "nope.md"], repo.path(), &env, None);
    assert_ne!(r.code, 0, "{}", r.stdout);
    assert!(r.stderr.contains("nope.md"), "{}", r.stderr);
}

/// A `[FILE]`-restricted run must not make other files' claims disappear
/// from the stored report: `show` (and a future `--all`) still need them.
#[test]
fn a_restricted_run_merges_into_the_stored_report_instead_of_replacing_it() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The plan lives in `docs/plan.md`.\n",
    );
    write_memory(repo.path(), &env, "b.md", "- Run `lib.sh` to start it.\n");

    let full = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(full.code, 0, "{}", full.stderr);

    // A second run restricted to a.md alone must still leave b.md's claim
    // in the stored report.
    let restricted = run(
        &["review", "run", "a.md", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(restricted.code, 0, "{}", restricted.stderr);
    let report: serde_json::Value = serde_json::from_str(&restricted.stdout).unwrap();
    let files: std::collections::HashSet<&str> = report["claims"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        std::collections::HashSet::from(["a.md", "b.md"]),
        "b.md's claim from the earlier run must survive a run restricted to a.md: {report}"
    );

    let shown = run(&["review", "show", "--json"], repo.path(), &env, None);
    let shown_report: serde_json::Value = serde_json::from_str(&shown.stdout).unwrap();
    assert_eq!(
        shown_report["claims"].as_array().unwrap().len(),
        report["claims"].as_array().unwrap().len()
    );
}

/// The corrected version of a note — every claim about the old paths now
/// phrased as history — gets no `stale` claim anywhere, mirroring the
/// design's own test table (row 2).
#[test]
fn the_corrected_note_gets_no_stale_claim() {
    let repo = review_repo();
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        repo.path(),
        &env,
        "project_phase1_deploy.md",
        "---\nname: project-phase1-deploy\n---\n\n\
         The server is at `recall-server.pimlabs.id`, behind Traefik on a VPS.\n\
         \n\
         ## History\n\
         \n\
         - Not the old `hooks/recall-*` scripts, and not `lib.sh`; both were deleted \
           in the Rust rewrite.\n",
    );
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    for claim in report["claims"].as_array().unwrap() {
        assert_ne!(
            claim["verdict"], "stale",
            "a corrected file must get no stale claim: {claim}"
        );
    }
    let history = claim_containing(&report["claims"], "Not the old");
    assert_eq!(history["class"], "record");
}

/// Design test row: the memory directory line is `still_true` in a cloud
/// environment and `cant_tell` on a laptop, end to end: the variable as a
/// hook sees it, and `CLAUDE_CODE_REMOTE` as the harness sets it.
/// Mutation: decide environment claims on any machine.
#[test]
fn a_cloud_memory_dir_claim_is_decided_only_in_a_cloud_session() {
    let repo = review_repo();
    let memory = tempfile::tempdir().unwrap();
    let memory_str = memory.path().to_string_lossy().to_string();
    // Written the way the design's worked example writes it: the
    // assignment alone, as a list item.
    let note = format!("- CLAUDE_CODE_REMOTE_MEMORY_DIR={memory_str}\n");

    let cloud = [
        ("CLAUDE_CODE_REMOTE", "true"),
        ("CLAUDE_CODE_REMOTE_MEMORY_DIR", memory_str.as_str()),
    ];
    write_memory(repo.path(), &cloud, "env.md", &note);
    let r = run(&["review", "run", "--json"], repo.path(), &cloud, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "CLAUDE_CODE_REMOTE_MEMORY_DIR");
    assert_eq!(claim["verdict"], "still_true", "{claim}");
    assert_eq!(claim["evidence"][0]["source"], "environment", "{claim}");

    // The same note, read on a laptop: the claim is about somewhere else.
    let laptop: [(&str, &str); 0] = [];
    write_memory(repo.path(), &laptop, "env.md", &note);
    let r = run(&["review", "run", "--json"], repo.path(), &laptop, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "CLAUDE_CODE_REMOTE_MEMORY_DIR");
    assert_eq!(claim["verdict"], "cant_tell", "{claim}");
}

/// The configured server, asked `/health` and its discovery document once:
/// a note naming it is `still_true` with the commit it runs, and the report
/// says which version and commit it read.
#[test]
fn a_claim_naming_the_configured_server_is_checked_against_it() {
    let server = live_server("t");
    let repo = review_repo();
    let env = [("RECALL_URL", server.url.as_str()), ("RECALL_TOKEN", "t")];
    write_memory(
        repo.path(),
        &env,
        "server.md",
        &format!("- The server is at {}.\n", server.url),
    );
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "The server is at");
    assert_eq!(claim["verdict"], "still_true", "{claim}");
    assert_eq!(claim["evidence"][0]["source"], "server", "{claim}");
    assert!(
        report["evidence"]["server_version"].is_string(),
        "{}",
        report["evidence"]
    );
    let unavailable = report["evidence"]["unavailable"].to_string();
    assert!(!unavailable.contains("\"server\""), "{unavailable}");
}

/// A server that does not answer is a source that could not be read, not
/// a failed review: exit 0, and the reason in `evidence.unavailable`.
#[test]
fn an_unreachable_server_is_reported_as_unavailable() {
    let repo = review_repo();
    let env = [("RECALL_URL", "http://127.0.0.1:9"), ("RECALL_TOKEN", "t")];
    write_memory(repo.path(), &env, "a.md", "- Run `lib.sh` first.\n");
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let server = report["evidence"]["unavailable"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["source"] == "server")
        .unwrap_or_else(|| panic!("{}", report["evidence"]));
    assert!(
        server["reason"]
            .as_str()
            .unwrap()
            .contains("did not answer"),
        "{server}"
    );
}

/// The project's compose files, read as YAML: an ingress a note names is
/// found because a compose file runs it, not because a word appears
/// somewhere in the tree. That it runs is not what the note claims of it,
/// so the claim is `cant_tell`, with the compose file as its evidence.
#[test]
fn a_claim_naming_a_compose_service_is_found_in_the_compose_file() {
    let repo = review_repo();
    std::fs::create_dir_all(repo.path().join("deploy")).unwrap();
    std::fs::write(
        repo.path().join("deploy").join("docker-compose.yml"),
        "services:\n  cloudflared:\n    image: cloudflare/cloudflared:latest\n",
    )
    .unwrap();
    let env: [(&str, &str); 0] = [];
    write_memory(
        repo.path(),
        &env,
        "ingress.md",
        "- The server is deployed via `cloudflared`.\n",
    );
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "cloudflared");
    assert_eq!(claim["verdict"], "cant_tell", "{claim}");
    assert_eq!(claim["evidence"][0]["source"], "compose", "{claim}");
    assert_eq!(
        report["evidence"]["compose_files"],
        serde_json::json!(["deploy/docker-compose.yml"])
    );
}

/// Without `--probe-hosts`, a host a note names is never asked anything,
/// and the report says how many were left unasked.
#[test]
fn hosts_a_note_names_are_not_probed_without_the_flag() {
    let repo = review_repo();
    let env: [(&str, &str); 0] = [];
    write_memory(
        repo.path(),
        &env,
        "hosts.md",
        "- The mirror is at `mirror.example.invalid`.\n",
    );
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert!(
        report["evidence"]["probed"].is_null(),
        "{}",
        report["evidence"]
    );
    let unavailable = report["evidence"]["unavailable"].to_string();
    assert!(unavailable.contains("--probe-hosts"), "{unavailable}");
}

/// A stand-in `claude`, prepended to `PATH` so it wins over any real one.
/// `claude auth status` says it is logged in; every other call keeps its
/// stdin as `stdin-<n>` in its directory and answers
/// with the CLI's JSON envelope around `answer`. Returns the directory and
/// the `PATH` to run with.
///
/// Unix only, for the reason [`hostname_shim`] gives.
#[cfg(unix)]
fn fake_claude(answer: &str) -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let envelope = serde_json::json!({"type": "result", "is_error": false, "result": answer});
    std::fs::write(dir.path().join("answer.json"), envelope.to_string()).unwrap();
    let script = dir.path().join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nd='{}'\nif [ \"$1\" = auth ]; then echo '{{\"loggedIn\":true}}'; exit 0; fi\nn=$(ls \"$d\" | grep -c '^stdin-')\ncat > \"$d/stdin-$n\"\ncat \"$d/answer.json\"\n",
            dir.path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (dir, path)
}

/// What each call to [`fake_claude`] was handed on stdin, in order.
#[cfg(unix)]
fn claude_calls(dir: &Path) -> Vec<String> {
    let mut calls: Vec<(usize, String)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().to_string();
            let n = name.strip_prefix("stdin-")?.parse().ok()?;
            Some((n, std::fs::read_to_string(dir.join(&name)).unwrap()))
        })
        .collect();
    calls.sort();
    calls.into_iter().map(|(_, s)| s).collect()
}

/// Layer 3 answers `stale` for C1, citing E1: the first fact on every
/// sheet (the configured server, or that none is).
#[cfg(unix)]
const C1_STALE: &str = r#"{"claims":[{"id":"C1","class":"present","verdict":"stale","cites":["E1"],"reason":"The facts say otherwise."}]}"#;

/// The design's "Layers 1 and 2 make no `claude` call, and none is made
/// without `--claude`": the fake counts every call, and the positive
/// control shows it is reachable.
#[cfg(unix)]
#[test]
fn no_claude_call_is_made_without_the_flag() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "plans.md",
        "- The deploy happens on Tuesdays.\n",
    );

    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(claude_calls(fake.path()).is_empty(), "claude was called");
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let unavailable = report["evidence"]["unavailable"].to_string();
    assert!(unavailable.contains("--claude"), "{unavailable}");
    assert!(
        report["evidence"]["claude"].is_null(),
        "{}",
        report["evidence"]
    );

    // `--max-calls` means nothing without `--claude`, and says so.
    let r = run(
        &["review", "run", "--max-calls", "3"],
        repo.path(),
        &env,
        None,
    );
    assert_ne!(r.code, 0, "{}", r.stdout);
    assert!(claude_calls(fake.path()).is_empty(), "claude was called");

    let r = run(&["review", "run", "--claude"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(claude_calls(fake.path()).len(), 1, "the control");
}

/// A verdict that cites a fact decides an `unsure` claim at layer 3; the
/// answer is kept, so an unchanged file is not asked again (with or without
/// `--claude`), and a changed one is.
#[cfg(unix)]
#[test]
fn a_claude_verdict_is_kept_until_the_file_changes() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    let note = "- The deploy happens on Tuesdays.\n";
    write_memory(repo.path(), &env, "plans.md", note);

    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "Tuesdays");
    assert_eq!(claim["class"], "present", "{claim}");
    assert_eq!(claim["verdict"], "stale", "{claim}");
    assert_eq!(claim["layer"], 3, "{claim}");
    assert_eq!(claim["evidence"][0]["source"], "claude", "{claim}");
    assert_eq!(
        report["evidence"]["claude"]["calls"], 1,
        "{}",
        report["evidence"]
    );
    let calls = claude_calls(fake.path());
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains("C1 (line 1): The deploy happens on Tuesdays."),
        "{}",
        calls[0]
    );
    assert!(calls[0].contains("E1: "), "{}", calls[0]);

    // Unchanged: asked about again by nobody, the verdict kept.
    for args in [
        &["review", "run", "--claude", "--json"][..],
        &["review", "run", "--json"][..],
    ] {
        let r = run(args, repo.path(), &env, None);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        let claim = claim_containing(&report["claims"], "Tuesdays");
        assert_eq!(claim["verdict"], "stale", "{args:?}: {claim}");
        assert_eq!(claude_calls(fake.path()).len(), 1, "{args:?}");
    }

    // `--all` asks again.
    let r = run(
        &["review", "run", "--claude", "--all"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(claude_calls(fake.path()).len(), 2);

    // Changed: asked again.
    write_memory(
        repo.path(),
        &env,
        "plans.md",
        &format!("{note}- Nothing else.\n"),
    );
    let r = run(&["review", "run", "--claude"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(claude_calls(fake.path()).len(), 3);
}

/// The design's "A planted token in a note never reaches the fake
/// `claude`'s stdin".
#[cfg(unix)]
#[test]
fn a_planted_token_never_reaches_claude() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    let token = format!("ghp_{}", "Zx9Yw8Vu7T".repeat(4));
    write_memory(
        repo.path(),
        &env,
        "creds.md",
        &format!("- The deploy key is {token} and it opens the vault.\n"),
    );

    let r = run(&["review", "run", "--claude"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let calls = claude_calls(fake.path());
    assert_eq!(calls.len(), 1);
    assert!(!calls[0].contains(&token), "{}", calls[0]);
    assert!(calls[0].contains("it opens the vault"), "{}", calls[0]);
}

/// `--max-calls` bounds the calls, and a file it leaves out is listed as
/// skipped, never silently dropped.
#[cfg(unix)]
#[test]
fn max_calls_bounds_the_calls_and_lists_what_it_skipped() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The deploy happens on Tuesdays.\n",
    );
    write_memory(
        repo.path(),
        &env,
        "b.md",
        "- The backup happens on Fridays.\n",
    );

    let r = run(
        &["review", "run", "--claude", "--max-calls", "1", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(claude_calls(fake.path()).len(), 1);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let skipped = &report["evidence"]["claude"]["skipped"];
    assert_eq!(skipped[0]["file"], "b.md", "{skipped}");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("--max-calls 1"),
        "{skipped}"
    );

    // The text form says so too: up front, and file by file with --details.
    let r = run(&["review", "show"], repo.path(), &env, None);
    assert!(
        r.stdout.contains("claude was not asked about 1 file(s)")
            && r.stdout.contains("--max-calls 1"),
        "{}",
        r.stdout
    );
    let r = run(&["review", "show", "--details"], repo.path(), &env, None);
    assert!(r.stdout.contains("! b.md: --max-calls 1"), "{}", r.stdout);
}

/// The design's "A `claude` verdict with no fact-sheet citation becomes
/// `cant_tell`", end to end.
#[cfg(unix)]
#[test]
fn a_claude_verdict_citing_nothing_is_cant_tell() {
    let repo = review_repo();
    let (_fake, path) = fake_claude(
        r#"{"claims":[{"id":"C1","class":"present","verdict":"stale","cites":[],"reason":"I just know."}]}"#,
    );
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "plans.md",
        "- The deploy happens on Tuesdays.\n",
    );
    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "Tuesdays");
    assert_eq!(claim["verdict"], "cant_tell", "{claim}");
    let detail = claim["evidence"][0]["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("claude's reading, not evidence"),
        "{detail}"
    );
}

/// Only `unsure` and undecided `present` claims are handed over: a record
/// and a rule never appear among the claims `claude` is asked to judge.
#[cfg(unix)]
#[test]
fn records_and_rules_are_never_asked_about() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "plans.md",
        "- The deploy happens on Tuesdays.\n- The deploy used to happen on Mondays.\n",
    );
    write_memory(
        repo.path(),
        &env,
        "feedback_style.md",
        "---\ntype: feedback\n---\n- Always answer in Indonesian.\n",
    );
    let r = run(&["review", "run", "--claude"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let calls = claude_calls(fake.path());
    assert_eq!(calls.len(), 1, "the feedback note has nothing to ask about");
    let claims = calls[0].split("=== Claims to judge ===").nth(1).unwrap();
    assert!(
        claims.contains("C1 (line 1): The deploy happens on Tuesdays."),
        "{claims}"
    );
    assert!(!claims.contains("Mondays"), "{claims}");
    assert!(!claims.contains("C2"), "{claims}");
}

/// A run that cannot ask never throws an answer away: while a fact it cited
/// is not observed the answer is not shown, and once it is again the answer
/// is, with no new call. `--claude` asks again while the fact is missing.
#[cfg(unix)]
#[test]
fn an_answer_whose_fact_went_missing_is_kept_not_shown() {
    let repo = review_repo();
    // Cites the first four facts, among which is the one about which
    // server this machine is configured for.
    let (fake, path) = fake_claude(
        r#"{"claims":[{"id":"C1","class":"present","verdict":"stale","cites":["E1","E2","E3","E4"],"reason":"The facts say otherwise."}]}"#,
    );
    let unset = [("PATH", path.as_str())];
    let moved = [
        ("PATH", path.as_str()),
        ("RECALL_URL", "http://127.0.0.1:9"),
    ];
    write_memory(
        repo.path(),
        &unset,
        "plans.md",
        "- The deploy happens on Tuesdays.\n",
    );
    let verdict = |env: &[(&str, &str)], args: &[&str]| {
        let r = run(args, repo.path(), env, None);
        assert_eq!(r.code, 0, "{}", r.stderr);
        let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        claim_containing(&report["claims"], "Tuesdays")["verdict"].clone()
    };

    assert_eq!(
        verdict(&unset, &["review", "run", "--claude", "--json"]),
        "stale"
    );
    assert_eq!(
        verdict(&moved, &["review", "run", "--json"]),
        serde_json::Value::Null
    );
    assert_eq!(verdict(&unset, &["review", "run", "--json"]), "stale");
    assert_eq!(claude_calls(fake.path()).len(), 1);

    assert_eq!(
        verdict(&moved, &["review", "run", "--claude", "--json"]),
        "stale"
    );
    assert_eq!(claude_calls(fake.path()).len(), 2);
}

/// An answer that leaves a claim out is shown, and asked about again.
#[cfg(unix)]
#[test]
fn a_partial_answer_is_shown_and_asked_again() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "plans.md",
        "- The deploy happens on Tuesdays.\n- The backup happens on Fridays.\n",
    );
    for expected in [1, 2] {
        let r = run(
            &["review", "run", "--claude", "--json"],
            repo.path(),
            &env,
            None,
        );
        assert_eq!(r.code, 0, "{}", r.stderr);
        let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        assert_eq!(
            claim_containing(&report["claims"], "Tuesdays")["verdict"],
            "stale"
        );
        assert_eq!(claude_calls(fake.path()).len(), expected);
    }
    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        claim_containing(&report["claims"], "Tuesdays")["verdict"],
        "stale"
    );
    assert_eq!(claude_calls(fake.path()).len(), 2);
}

/// A `[FILE]` run asks about that file only, and keeps every other file's
/// answer.
#[cfg(unix)]
#[test]
fn a_files_run_keeps_the_other_files_answers() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The deploy happens on Tuesdays.\n",
    );
    write_memory(
        repo.path(),
        &env,
        "b.md",
        "- The backup happens on Fridays.\n",
    );
    let r = run(&["review", "run", "--claude"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(claude_calls(fake.path()).len(), 2);

    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The deploy happens on Thursdays.\n",
    );
    let r = run(
        &["review", "run", "a.md", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(claude_calls(fake.path()).len(), 3);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        claim_containing(&report["claims"], "Fridays")["verdict"],
        "stale"
    );
    assert_eq!(
        claim_containing(&report["claims"], "Thursdays")["verdict"],
        "stale"
    );

    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        claim_containing(&report["claims"], "Fridays")["verdict"],
        "stale"
    );
    assert_eq!(claude_calls(fake.path()).len(), 3);
}

/// What layer 2 observed but could not decide on ("not found on this
/// machine") is context `claude` cannot cite, so it cannot turn a claim the
/// design keeps `cant_tell` into a verdict.
#[cfg(unix)]
#[test]
fn what_decided_nothing_is_context_not_a_citable_fact() {
    let repo = review_repo();
    let (fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "data.md",
        "- The archive lives in /srv/recall-nowhere/archive.\n",
    );
    let r = run(&["review", "run", "--claude"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let calls = claude_calls(fake.path());
    assert_eq!(calls.len(), 1);
    // The note itself is at the top of the prompt, path and all: only the
    // facts section between it and the context is looked at.
    let after_note = calls[0]
        .split_once("=== Facts observed")
        .unwrap_or_else(|| panic!("no facts section: {}", calls[0]))
        .1;
    let (facts, rest) = after_note
        .split_once("=== Context")
        .unwrap_or_else(|| panic!("no context section: {}", calls[0]));
    assert!(!facts.contains("/srv/recall-nowhere/archive"), "{facts}");
    let context = rest.split("=== Claims to judge ===").next().unwrap();
    assert!(context.contains("/srv/recall-nowhere/archive"), "{context}");
}

/// Layer 3 answers `stale` for C1, citing E1, with a rewrite into a record.
#[cfg(unix)]
const C1_STALE_REWRITTEN: &str = r#"{"claims":[{"id":"C1","class":"present","verdict":"stale","cites":["E1"],"reason":"The facts say otherwise.","rewrite":"Until 2026-09 the deploy happened on Tuesdays; it now happens on Wednesdays."}]}"#;

/// The design's `recall review apply`: a stale claim's rewrite into a
/// record replaces the claim's own words, keeps the record beside it, is
/// written and pushed, and is refused once the file has changed since the
/// review read it.
#[cfg(unix)]
#[test]
fn review_apply_rewrites_a_stale_claim_into_a_record_and_pushes_it() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let (_fake, path) = fake_claude(C1_STALE_REWRITTEN);
    let env = [("RECALL_HOME", home_str.as_str()), ("PATH", path.as_str())];
    // A paragraph: two claims on one line, the second a record the edit
    // must keep word for word.
    let content = "# Deploy\n\nThe deploy happens on Tuesdays. It used to happen on Mondays.\n";
    let pushed = push_memory(&repo, &env, "deploy.md", content);
    assert_eq!(pushed.code, 0, "{}", pushed.stderr);

    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "happens on Tuesdays");
    assert_eq!(claim["verdict"], "stale", "{claim}");
    let edit = &claim["suggested_edit"];
    assert_eq!(edit["file_path"], "deploy.md", "{claim}");
    assert_eq!(edit["lines"], serde_json::json!([3, 3]), "{claim}");
    let id = claim["id"].as_str().unwrap().to_string();
    let shown = run(&["review", "show"], repo.path(), &env, None);
    assert!(
        shown.stdout.contains(&format!("recall review apply {id}")),
        "{}",
        shown.stdout
    );

    let applied = run(&["review", "apply", &id, "--yes"], repo.path(), &env, None);
    assert_eq!(applied.code, 0, "{}", applied.stderr);
    let fixed = "# Deploy\n\nUntil 2026-09 the deploy happened on Tuesdays; it now happens on \
                 Wednesdays. It used to happen on Mondays.\n";
    let memory_dir = status_json(repo.path(), &env)["memory_dir"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        std::fs::read_to_string(Path::new(&memory_dir).join("deploy.md")).unwrap(),
        fixed
    );
    let on_server = stored(&server, &repo, &env);
    let file = on_server
        .iter()
        .find(|f| f.file_path == "deploy.md")
        .unwrap();
    assert_eq!(file.content.as_deref(), Some(fixed));

    // The report is from before the edit: applying it again is refused,
    // and says how to get a new one.
    let again = run(&["review", "apply", &id, "--yes"], repo.path(), &env, None);
    assert_eq!(again.code, 1, "{}", again.stdout);
    assert!(
        again.stderr.contains("recall review run --claude"),
        "{}",
        again.stderr
    );
}

/// A claim with no suggested edit, or one not in the report, is refused
/// with where to look; before any review there is nothing to apply.
#[cfg(unix)]
#[test]
fn review_apply_refuses_what_it_cannot_do() {
    let repo = review_repo();
    let (_fake, path) = fake_claude(C1_STALE);
    let env = [("PATH", path.as_str())];
    let none = run(&["review", "apply", "t1", "--yes"], repo.path(), &env, None);
    assert_eq!(none.code, 1);
    assert!(none.stderr.contains("recall review run"), "{}", none.stderr);

    write_memory(
        repo.path(),
        &env,
        "plans.md",
        "- The deploy happens on Tuesdays.\n",
    );
    // Stale, but claude gave no rewrite.
    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "Tuesdays");
    assert!(claim["suggested_edit"].is_null(), "{claim}");
    let id = claim["id"].as_str().unwrap().to_string();
    let r = run(&["review", "apply", &id, "--yes"], repo.path(), &env, None);
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("has no edit to make"), "{}", r.stderr);

    let r = run(
        &["review", "apply", "t99", "--yes"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("recall review show"), "{}", r.stderr);
}

/// A claim that holds a secret is never rewritten: claude saw it masked,
/// and its rewrite would put the mask in the note.
#[cfg(unix)]
#[test]
fn a_claim_holding_a_secret_gets_no_rewrite() {
    let repo = review_repo();
    let (_fake, path) = fake_claude(C1_STALE_REWRITTEN);
    let env = [("PATH", path.as_str())];
    let token = format!("ghp_{}", "Zx9Yw8Vu7T".repeat(4));
    write_memory(
        repo.path(),
        &env,
        "creds.md",
        &format!("- The deploy key is {token} and it opens the vault.\n"),
    );
    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let claim = claim_containing(&report["claims"], "opens the vault");
    assert_eq!(claim["verdict"], "stale", "{claim}");
    assert!(claim["suggested_edit"].is_null(), "{claim}");
}

/// `.recall-review.json` decides nothing about which bytes change: the
/// report's `suggested_edit` is rebuilt, not trusted, and a stored rewrite
/// is held to the rules again. Without a terminal to ask on and without
/// `--yes`, nothing is changed.
#[cfg(unix)]
#[test]
fn review_apply_builds_the_edit_again_and_trusts_no_stored_one() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let (_fake, path) = fake_claude(C1_STALE_REWRITTEN);
    let env = [("RECALL_HOME", home_str.as_str()), ("PATH", path.as_str())];
    let content = "# Deploy\n\nThe deploy happens on Tuesdays. It used to happen on Mondays.\n";
    assert_eq!(push_memory(&repo, &env, "deploy.md", content).code, 0);
    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let id = claim_containing(&report["claims"], "Tuesdays")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let memory_dir = PathBuf::from(
        status_json(repo.path(), &env)["memory_dir"]
            .as_str()
            .unwrap(),
    );
    let note = memory_dir.join("deploy.md");
    let state_file = memory_dir.parent().unwrap().join(".recall-review.json");
    let original = std::fs::read_to_string(&state_file).unwrap();

    // No terminal, no --yes: refused, nothing changed.
    let asked = run(&["review", "apply", &id], repo.path(), &env, None);
    assert_eq!(asked.code, 1, "{}", asked.stdout);
    assert!(
        asked.stderr.contains("needs a terminal"),
        "{}",
        asked.stderr
    );
    assert_eq!(std::fs::read_to_string(&note).unwrap(), content);

    // A stored rewrite that is no record is held to the rules again.
    let mut state: serde_json::Value = serde_json::from_str(&original).unwrap();
    state["files"]["deploy.md"]["claude"]["decided"][0]["rewrite"] =
        "The deploy happens on Wednesdays.".into();
    std::fs::write(&state_file, state.to_string()).unwrap();
    let r = run(&["review", "apply", &id, "--yes"], repo.path(), &env, None);
    assert_eq!(r.code, 1, "{}", r.stdout);
    assert!(r.stderr.contains("has no edit to make"), "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(&note).unwrap(), content);

    // A report whose suggested_edit was rewritten by hand: what is made is
    // the edit built from the file and the stored rewrite, nothing else.
    let mut state: serde_json::Value = serde_json::from_str(&original).unwrap();
    for claim in state["report"]["claims"].as_array_mut().unwrap() {
        if claim["id"] == id.as_str() {
            claim["suggested_edit"]["replacement"] = "HACKED\n".into();
            claim["suggested_edit"]["lines"] = serde_json::json!([1, 3]);
        }
    }
    std::fs::write(&state_file, state.to_string()).unwrap();
    let r = run(&["review", "apply", &id, "--yes"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        std::fs::read_to_string(&note).unwrap(),
        "# Deploy\n\nUntil 2026-09 the deploy happened on Tuesdays; it now happens on \
         Wednesdays. It used to happen on Mondays.\n"
    );
}

/// The design's "Beside the worker's reports": with a machine that can
/// read reports, each claim a finding of the newest finished one covers
/// names it, in `--json` and in the text; one that cannot says so as a
/// source it could not read, and the review goes on.
#[test]
fn a_reports_findings_are_shown_beside_the_claims_they_cover() {
    let server = live_server("right");
    let repo = git_repo();
    let home = recall_home_with(&[(&server.url, "right")], &server.url);
    let home_str = home.path().to_string_lossy().to_string();
    let env = [("RECALL_HOME", home_str.as_str())];
    let content = "# Deploy\n- key: abc123\n- ship it\n";
    assert_eq!(push_memory(&repo, &env, "deploy.md", content).code, 0);
    let key = status_json(repo.path(), &env)["project_key"]
        .as_str()
        .unwrap()
        .to_string();
    plant_report(&server, &key, content);

    let r = run(&["review", "run", "--json"], repo.path(), &env, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        report["evidence"]["evaluation"], "eval_cli",
        "{}",
        report["evidence"]
    );
    let covered = claim_containing(&report["claims"], "key: abc123");
    assert_eq!(covered["eval"][0]["evaluation"], "eval_cli", "{covered}");
    assert_eq!(covered["eval"][0]["finding"], "f1", "{covered}");
    assert_eq!(covered["eval"][0]["kind"], "secret", "{covered}");
    let beside = claim_containing(&report["claims"], "ship it");
    assert!(beside["eval"].is_null(), "{beside}");
    let shown = run(&["review", "show"], repo.path(), &env, None);
    assert!(
        shown.stdout.contains("eval_cli f1 secret (high), on t"),
        "{}",
        shown.stdout
    );

    // The same server, from a machine with no admin access: no report, and
    // the reason as an unread source.
    let url_only = [("RECALL_URL", server.url.as_str())];
    let r = run(&["review", "run", "--json"], repo.path(), &url_only, None);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert!(
        report["evidence"]["evaluation"].is_null(),
        "{}",
        report["evidence"]
    );
    let unavailable = report["evidence"]["unavailable"].to_string();
    assert!(unavailable.contains("\"evaluation\""), "{unavailable}");
}

/// A stand-in `claude` running `script` (a `sh` body) for every call.
/// Returns the directory and the `PATH` to run with.
#[cfg(unix)]
fn claude_script(script: &str) -> (tempfile::TempDir, String) {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nd='{}'\n{script}", dir.path().display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env_path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (dir, env_path)
}

/// A machine whose `claude` is not logged in is told so once, in words that
/// say what to do, and no file is asked about.
#[cfg(unix)]
#[test]
fn claude_not_logged_in_is_said_once_before_any_call() {
    let repo = review_repo();
    let (fake, path) = claude_script(
        "if [ \"$1\" = auth ]; then echo '{\"loggedIn\":false}'; exit 1; fi\n: > \"$d/called\"\n",
    );
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The deploy happens on Tuesdays.\n",
    );
    write_memory(
        repo.path(),
        &env,
        "b.md",
        "- The backup happens on Fridays.\n",
    );
    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(!fake.path().join("called").exists(), "a call was made");
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let skipped = report["evidence"]["claude"]["skipped"].to_string();
    assert!(skipped.contains("not logged in"), "{skipped}");
    assert_eq!(report["evidence"]["claude"]["calls"], 0);
}

/// When the CLI itself fails, the reason is what it said (on stdout, in its
/// JSON envelope), and the files after it are not asked.
#[cfg(unix)]
#[test]
fn a_claude_failure_says_why_and_stops_asking() {
    let repo = review_repo();
    let (fake, path) = claude_script(concat!(
        "if [ \"$1\" = auth ]; then echo '{\"loggedIn\":true}'; exit 0; fi\n",
        "cat > /dev/null; : >> \"$d/calls\"; echo x >> \"$d/calls\"\n",
        "echo '{\"type\":\"result\",\"is_error\":true,\"result\":\"Credit balance is too low\"}'; exit 1\n",
    ));
    let env = [("PATH", path.as_str())];
    write_memory(
        repo.path(),
        &env,
        "a.md",
        "- The deploy happens on Tuesdays.\n",
    );
    write_memory(
        repo.path(),
        &env,
        "b.md",
        "- The backup happens on Fridays.\n",
    );
    let r = run(
        &["review", "run", "--claude", "--json"],
        repo.path(),
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let calls = std::fs::read_to_string(fake.path().join("calls")).unwrap();
    assert_eq!(calls.lines().count(), 1, "asked again after a failure");
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    let skipped = report["evidence"]["claude"]["skipped"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("Credit balance is too low"),
        "{skipped:?}"
    );
    assert!(
        skipped[1]["reason"].as_str().unwrap().contains("not asked"),
        "{skipped:?}"
    );
    // The failure itself, and which claude it was, for the text to lead with.
    let claude = &report["evidence"]["claude"];
    assert!(
        claude["failure"]
            .as_str()
            .unwrap()
            .contains("Credit balance is too low"),
        "{claude}"
    );
    let binary = claude["binary"].as_str().unwrap();
    assert!(
        binary.starts_with(&fake.path().display().to_string()),
        "{binary}"
    );

    // Said before anything else in the text, with the binary and what to
    // try, not as the last lines under the claims.
    let text = run(&["review", "show"], repo.path(), &env, None);
    let warning = text
        .stdout
        .find("claude could not answer, so 2 file(s) got no answer")
        .unwrap_or_else(|| panic!("{}", text.stdout));
    let summary = text
        .stdout
        .find("still true")
        .unwrap_or_else(|| panic!("{}", text.stdout));
    assert!(warning < summary, "{}", text.stdout);
    assert!(text.stdout.contains(binary), "{}", text.stdout);
    assert!(
        text.stdout.contains("claude -p \"hello\""),
        "{}",
        text.stdout
    );
}

/// A checkout behind what it tracks is said first: every verdict is
/// checked against it. Seen live: a review run on a detached HEAD at an
/// old release reported files "not at HEAD" that main has.
#[test]
fn a_checkout_behind_its_upstream_is_said_first() {
    let upstream = review_repo();
    let clone = tempfile::tempdir().unwrap();
    let git = |dir: &Path, args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success());
    };
    git(
        clone.path(),
        &["clone", "-q", &upstream.path().display().to_string(), "."],
    );
    git(clone.path(), &["checkout", "-q", "--detach", "HEAD~1"]);
    let env: Vec<(&str, &str)> = vec![];
    write_memory(
        clone.path(),
        &env,
        "plan.md",
        "- The plan lives in `docs/plan.md`.\n",
    );

    let json = run(&["review", "run", "--json"], clone.path(), &env, None);
    assert_eq!(json.code, 0, "{}", json.stderr);
    let report: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    let ev = &report["evidence"];
    assert_eq!(ev["repository_detached"], true, "{ev}");
    assert_eq!(ev["repository_behind"], 1, "{ev}");

    let text = run(&["review", "show"], clone.path(), &env, None);
    assert!(
        text.stdout.contains("This checkout is a detached HEAD at")
            && text.stdout.contains("1 commit(s) behind"),
        "{}",
        text.stdout
    );
    assert!(text.stdout.contains("git switch "), "{}", text.stdout);

    // On the branch, and up to date, nothing is said.
    let text = run(&["review", "run"], upstream.path(), &env, None);
    assert!(!text.stdout.contains("This checkout is"), "{}", text.stdout);
}
