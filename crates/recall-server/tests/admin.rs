//! `recall-server admin`, driven as the owner drives it: the real binary,
//! against a real database file, sometimes with a real server serving that
//! same file.
//!
//! These edit the only copy of someone's memory, so most of what is here is
//! about what must *not* happen: a change nobody confirmed, a change with no
//! backup behind it, a partial change, a change to a key that was only
//! nearly named.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use recall_server::{Config, Server, Store};
use rusqlite::Connection;
use tempfile::TempDir;
use tower::ServiceExt;

const OLD: &str = "local:-Users-me-thing";
const T0: &str = "2026-09-20T10:00:00.000Z";
const T1: &str = "2026-09-21T10:00:00.000Z";
const TOKEN: &str = "admin-test-token";

/// Every column of a row: key, path, content, source, updated_at, deleted.
type DumpRow = (String, String, String, Option<String>, String, i64);

/// Every row, so "nothing changed" is checked on every column rather than
/// on a count.
type Dump = Vec<DumpRow>;

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    /// Three keys: the one a machine with no git remote wrote (a file, one
    /// with no source, a tombstone), the key it should have used, and an
    /// unrelated project that must never be touched.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let st = Store::open(dir.path().join("recall.db")).unwrap();
        st.upsert(OLD, "MEMORY.md", "- [notes](notes.md)\n", "laptop", T0)
            .unwrap();
        st.upsert(OLD, "notes.md", "a fact\n", "", T1).unwrap();
        st.upsert(OLD, "old.md", "kept after delete\n", "laptop", T0)
            .unwrap();
        st.tombstone(OLD, "old.md", "laptop", T1).unwrap();
        st.upsert("me/thing", "MEMORY.md", "- other\n", "cloud", T1)
            .unwrap();
        st.upsert("acme/app", "unrelated.md", "leave me\n", "cloud", T0)
            .unwrap();
        Self { dir }
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("recall.db")
    }

    fn backups(&self) -> PathBuf {
        self.dir.path().join("backups")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_recall-server"));
        cmd.arg("admin")
            .args(args)
            // No RECALL_TOKEN on purpose: an admin command needs none.
            .env_clear()
            .env("RECALL_DB_PATH", self.db())
            .env("RECALL_BACKUP_DIR", self.backups());
        cmd
    }

    fn admin(&self, args: &[&str]) -> Output {
        self.command(args)
            .stdin(Stdio::null())
            .output()
            .expect("recall-server runs")
    }

    /// Runs with `typed` on stdin, as if typed at the confirmation prompt.
    fn admin_typing(&self, args: &[&str], typed: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(typed.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn dump(&self) -> Dump {
        dump(&self.db())
    }

    /// The backups admin commands took, oldest first.
    fn snapshots(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.backups().join("admin")) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
        found.sort();
        found
    }

    fn sql(&self, sql: &str) {
        Connection::open(self.db())
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
}

fn dump(db: &Path) -> Dump {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT project_key, file_path, content, source_env, updated_at, deleted
             FROM memory_files ORDER BY project_key, file_path",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn keys(dump: &Dump) -> Vec<&str> {
    let mut keys: Vec<&str> = dump.iter().map(|r| r.0.as_str()).collect();
    keys.dedup();
    keys
}

fn under<'a>(dump: &'a Dump, key: &str) -> Vec<&'a DumpRow> {
    dump.iter().filter(|r| r.0 == key).collect()
}

fn text(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[track_caller]
fn assert_exit(out: &Output, code: i32) {
    let (stdout, stderr) = text(out);
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------- list

#[test]
fn list_shows_every_key_with_its_counts_and_needs_no_token() {
    let fx = Fixture::new();
    let out = fx.admin(&["list"]);
    assert_exit(&out, 0);
    let (stdout, _) = text(&out);
    let line = |key: &str| {
        stdout
            .lines()
            .find(|l| l.ends_with(&format!("  {key}")))
            .unwrap_or_else(|| panic!("no line for {key} in:\n{stdout}"))
            .split_whitespace()
            .collect::<Vec<_>>()
    };
    // FILES, TOMBSTONES, LAST UPDATE, KEY
    assert_eq!(line(OLD), vec!["2", "1", T1, OLD]);
    assert_eq!(line("me/thing"), vec!["1", "0", T1, "me/thing"]);
    assert_eq!(line("acme/app"), vec!["1", "0", T0, "acme/app"]);
    assert!(stdout.contains("3 project key(s), 4 file(s), 1 tombstone(s)."));
}

#[test]
fn list_with_a_key_shows_its_files() {
    let fx = Fixture::new();
    let out = fx.admin(&["list", OLD]);
    assert_exit(&out, 0);
    let (stdout, _) = text(&out);
    assert!(stdout.contains("2 file(s), 1 tombstone(s)"), "{stdout}");
    for path in ["MEMORY.md", "notes.md", "old.md"] {
        assert!(
            stdout.lines().any(|l| l.ends_with(path)),
            "{path}: {stdout}"
        );
    }
    assert!(
        stdout
            .lines()
            .any(|l| l.starts_with("tombstone") && l.ends_with("old.md")),
        "{stdout}"
    );
}

/// A mistyped path must not quietly become an empty database, which would
/// list as "no projects" to someone already worried about their memory.
#[test]
fn a_database_that_is_not_there_is_an_error_and_is_not_created() {
    let fx = Fixture::new();
    let missing = fx.dir.path().join("typo.db");
    let out = fx
        .command(&["list"])
        .env("RECALL_DB_PATH", &missing)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_exit(&out, 1);
    assert!(text(&out).1.contains("no database"), "{:?}", text(&out));
    assert!(!missing.exists());
}

/// The way to see what a backup holds before restoring from it.
#[test]
fn list_backup_shows_what_a_backup_holds() {
    let fx = Fixture::new();
    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    fx.sql(&format!(
        "DELETE FROM memory_files WHERE project_key = '{OLD}'"
    ));
    let before = std::fs::read(&snapshot).unwrap();

    let out = fx.admin(&["list", "--backup", snapshot.to_str().unwrap()]);
    assert_exit(&out, 0);
    assert!(text(&out).0.contains(OLD), "{:?}", text(&out));
    let out = fx.admin(&["list", "--backup", snapshot.to_str().unwrap(), OLD]);
    assert_exit(&out, 0);
    assert!(text(&out).0.contains("notes.md"), "{:?}", text(&out));

    assert_eq!(
        std::fs::read(&snapshot).unwrap(),
        before,
        "a backup is read, never written"
    );
}

// ---------------------------------------------------------------- rename

#[test]
fn rename_moves_every_row_after_taking_a_backup() {
    let fx = Fixture::new();
    let before = fx.dump();
    let out = fx.admin(&["rename", OLD, "me/new", "--yes"]);
    assert_exit(&out, 0);
    let (stdout, _) = text(&out);

    let after = fx.dump();
    assert!(under(&after, OLD).is_empty());
    let moved: Vec<_> = under(&after, "me/new")
        .into_iter()
        .map(|r| (&r.1, &r.2, &r.3, &r.4, r.5))
        .collect();
    let original: Vec<_> = under(&before, OLD)
        .into_iter()
        .map(|r| (&r.1, &r.2, &r.3, &r.4, r.5))
        .collect();
    assert_eq!(
        moved, original,
        "every column of every row survives the move"
    );
    assert_eq!(under(&after, "acme/app"), under(&before, "acme/app"));

    // The backup is printed, and it is the state *before* the change: the
    // only way it can be is if it was taken first.
    let snapshots = fx.snapshots();
    assert_eq!(snapshots.len(), 1, "{stdout}");
    let name = snapshots[0].file_name().unwrap().to_str().unwrap();
    assert!(
        stdout.contains(name),
        "the backup's path is printed: {stdout}"
    );
    assert_eq!(dump(&snapshots[0]), before);
    assert!(stdout.contains(&format!("Confirmed with --yes for project key \"{OLD}\"")));
}

/// The primary key is (project_key, file_path). Refused whether or not the
/// paths overlap, with `--yes`, and before anything is written, a backup
/// included.
#[test]
fn a_rename_onto_a_key_that_holds_rows_is_refused_and_changes_nothing() {
    let fx = Fixture::new();
    let before = fx.dump();
    // Overlapping: both have MEMORY.md.
    let out = fx.admin(&["rename", OLD, "me/thing", "--yes"]);
    assert_exit(&out, 1);
    let (_, stderr) = text(&out);
    assert!(stderr.contains("already holds 1 row(s)"), "{stderr}");
    assert!(stderr.contains("The database was not changed."), "{stderr}");
    // Disjoint: acme/app has only unrelated.md. Still refused.
    let out = fx.admin(&["rename", OLD, "acme/app", "--yes"]);
    assert_exit(&out, 1);

    assert_eq!(fx.dump(), before);
    assert!(
        fx.snapshots().is_empty(),
        "a refused change takes no backup"
    );
}

#[test]
fn a_rename_onto_a_key_no_client_could_use_is_refused() {
    let fx = Fixture::new();
    let before = fx.dump();
    for to in ["me/new ", "", "me/\tnew", OLD] {
        let out = fx.admin(&["rename", OLD, to, "--yes"]);
        assert_exit(&out, 1);
    }
    assert_eq!(fx.dump(), before);
}

// ---------------------------------------------------------------- remove

#[test]
fn remove_deletes_that_key_only_after_a_backup() {
    let fx = Fixture::new();
    let before = fx.dump();
    let out = fx.admin(&["remove", OLD, "--yes"]);
    assert_exit(&out, 0);

    let after = fx.dump();
    assert_eq!(keys(&after), vec!["acme/app", "me/thing"]);
    assert_eq!(under(&after, "me/thing"), under(&before, "me/thing"));
    let snapshots = fx.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        dump(&snapshots[0]),
        before,
        "the backup holds what was removed"
    );
}

// ---------------------------------------------------------------- restore

/// A remove and a restore from the backup it took are exact inverses:
/// content, source (NULL included), timestamp and tombstone flag.
#[test]
fn restore_puts_back_exactly_the_rows_a_backup_holds() {
    let fx = Fixture::new();
    let before = fx.dump();
    assert_exit(&fx.admin(&["remove", OLD, "--yes"]), 0);
    let snapshot = fx.snapshots().pop().unwrap();

    let out = fx.admin(&["restore", snapshot.to_str().unwrap(), OLD, "--yes"]);
    assert_exit(&out, 0);
    assert_eq!(fx.dump(), before);
    assert_eq!(
        fx.snapshots().len(),
        2,
        "the restore took its own backup first"
    );
}

#[test]
fn restore_does_not_overwrite_without_the_flag_and_says_what_would_change() {
    let fx = Fixture::new();
    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    // Since the backup: one file edited, one added. The edit collides with
    // the backup; the addition is not in it.
    fx.sql(&format!(
        "UPDATE memory_files SET content = 'edited since', updated_at = '2026-09-23T00:00:00.000Z'
             WHERE project_key = '{OLD}' AND file_path = 'notes.md';
         INSERT INTO memory_files VALUES ('{OLD}', 'later.md', 'new', 'laptop', '2026-09-23T00:00:00.000Z', 0);
         DELETE FROM memory_files WHERE project_key = '{OLD}' AND file_path = 'MEMORY.md';"
    ));
    let before = fx.dump();
    let snap = snapshot.to_str().unwrap();

    let out = fx.admin(&["restore", snap, OLD, "--yes"]);
    assert_exit(&out, 1);
    let (stdout, stderr) = text(&out);
    assert!(stdout.contains("1 added and 1 overwritten"), "{stdout}");
    let overwrite = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("overwrite"))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(overwrite.contains("notes.md"), "{overwrite}");
    assert!(overwrite.contains("content 12 -> 7 bytes"), "{overwrite}");
    assert!(
        overwrite.contains(&format!("updated 2026-09-23T00:00:00.000Z -> {T1}")),
        "{overwrite}"
    );
    assert!(stdout.contains("untouched  later.md"), "{stdout}");
    assert!(stderr.contains("--overwrite"), "{stderr}");
    assert_eq!(fx.dump(), before, "nothing written, not even the addition");
    assert!(fx.snapshots().is_empty());

    let out = fx.admin(&["restore", snap, OLD, "--yes", "--overwrite"]);
    assert_exit(&out, 0);
    let after = fx.dump();
    let notes = under(&after, OLD)
        .into_iter()
        .find(|r| r.1 == "notes.md")
        .unwrap();
    assert_eq!(notes.2, "a fact\n");
    assert!(
        under(&after, OLD).iter().any(|r| r.1 == "later.md"),
        "a restore never deletes what the backup does not have"
    );
    assert!(under(&after, OLD).iter().any(|r| r.1 == "MEMORY.md"));
}

#[test]
fn restore_refuses_the_live_database_and_a_key_the_backup_lacks() {
    let fx = Fixture::new();
    let before = fx.dump();
    let db = fx.db();
    let out = fx.admin(&["restore", db.to_str().unwrap(), OLD, "--yes"]);
    assert_exit(&out, 1);
    assert!(text(&out).1.contains("is the live database itself"));

    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    let out = fx.admin(&["restore", snapshot.to_str().unwrap(), "local", "--yes"]);
    assert_exit(&out, 1);
    assert!(text(&out).1.contains("The backup holds no project key"));
    assert_eq!(fx.dump(), before);
    assert!(fx.snapshots().is_empty());
}

// ---------------------------------------------------------------- every change

#[test]
fn a_dry_run_changes_nothing_and_takes_no_backup() {
    let fx = Fixture::new();
    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    fx.sql(&format!(
        "DELETE FROM memory_files WHERE project_key = '{OLD}'"
    ));
    let bytes = std::fs::read(fx.db()).unwrap();
    let snap = snapshot.to_str().unwrap();

    for (args, lists) in [
        (vec!["remove", "me/thing", "--dry-run"], "MEMORY.md"),
        (
            vec!["rename", "me/thing", "me/new", "--dry-run"],
            "MEMORY.md",
        ),
        (vec!["restore", snap, OLD, "--dry-run"], "notes.md"),
        // --yes changes nothing about a dry run.
        (
            vec!["remove", "acme/app", "--dry-run", "--yes"],
            "unrelated.md",
        ),
    ] {
        let out = fx.admin(&args);
        assert_exit(&out, 0);
        let (stdout, _) = text(&out);
        assert!(
            stdout.contains(lists),
            "{args:?} names what it would touch: {stdout}"
        );
        assert!(stdout.contains("Dry run: nothing was changed"), "{stdout}");
    }
    assert_eq!(
        std::fs::read(fx.db()).unwrap(),
        bytes,
        "not one byte of the file"
    );
    assert!(fx.snapshots().is_empty());
}

/// No globbing, no prefixes, no case folding, no SQL wildcards, no
/// trimming: `=` or nothing.
#[test]
fn keys_are_matched_exactly_and_never_by_pattern() {
    let fx = Fixture::new();
    let before = fx.dump();
    for near in [
        "local",
        "local:-Users-me-*",
        "local:-Users-me-thin_",
        "local:%",
        "%",
        "*",
        "LOCAL:-USERS-ME-THING",
        "local:-Users-me-thing ",
        " local:-Users-me-thing",
    ] {
        let out = fx.admin(&["remove", near, "--yes"]);
        assert_exit(&out, 1);
        let (_, stderr) = text(&out);
        assert!(stderr.contains("matched exactly"), "{near:?}: {stderr}");
        let out = fx.admin(&["rename", near, "me/new", "--yes"]);
        assert_exit(&out, 1);
    }
    assert_eq!(fx.dump(), before);
    assert!(fx.snapshots().is_empty());
}

/// Without `--yes`, the key has to be typed back exactly.
#[test]
fn a_change_goes_ahead_only_when_the_key_is_typed_back() {
    let fx = Fixture::new();
    let before = fx.dump();
    for typed in [
        "",
        "y\n",
        "yes\n",
        "local:-Users-me-thin\n",
        "LOCAL:-USERS-ME-THING\n",
    ] {
        let out = fx.admin_typing(&["remove", OLD], typed);
        assert_exit(&out, 1);
    }
    assert_eq!(fx.dump(), before);
    assert!(fx.snapshots().is_empty(), "no backup until it is confirmed");

    let out = fx.admin_typing(&["remove", OLD], &format!("{OLD}\n"));
    assert_exit(&out, 0);
    assert!(under(&fx.dump(), OLD).is_empty());
}

/// Every change is refused when there is nowhere to put its backup, or the
/// backup cannot be written: never "go ahead without one".
#[test]
fn no_backup_means_no_change() {
    let fx = Fixture::new();
    let before = fx.dump();

    let out = fx
        .command(&["remove", OLD, "--yes"])
        .env_remove("RECALL_BACKUP_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_exit(&out, 1);
    assert!(text(&out).1.contains("RECALL_BACKUP_DIR is not set"));

    let not_a_dir = fx.dir.path().join("a-file");
    std::fs::write(&not_a_dir, "").unwrap();
    for args in [
        vec!["remove", OLD, "--yes"],
        vec!["rename", OLD, "me/new", "--yes"],
    ] {
        let out = fx
            .command(&args)
            .env("RECALL_BACKUP_DIR", &not_a_dir)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_exit(&out, 1);
        assert!(
            text(&out).1.contains("taking the backup"),
            "{:?}",
            text(&out)
        );
    }
    assert_eq!(fx.dump(), before);
}

/// `changes()` disagreeing with the plan rolls the whole change back. A
/// trigger that silently skips one row stands in for whatever could make
/// that happen in real life; it is exactly the case the check exists for,
/// since every other row *was* changed inside the transaction.
#[test]
fn a_changes_mismatch_rolls_back_every_command() {
    let fx = Fixture::new();
    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    let snap = snapshot.to_str().unwrap();

    for (trigger, args) in [
        (
            "BEFORE UPDATE ON memory_files WHEN OLD.file_path = 'notes.md'",
            vec!["rename", OLD, "me/new", "--yes"],
        ),
        (
            "BEFORE DELETE ON memory_files WHEN OLD.file_path = 'notes.md'",
            vec!["remove", OLD, "--yes"],
        ),
        (
            "BEFORE INSERT ON memory_files WHEN NEW.file_path = 'notes.md'",
            vec!["restore", snap, OLD, "--yes"],
        ),
    ] {
        if args[0] == "restore" {
            // Something for the restore to add back.
            fx.sql(&format!(
                "DELETE FROM memory_files WHERE project_key = '{OLD}'"
            ));
        }
        let before = fx.dump();
        fx.sql(&format!(
            "CREATE TRIGGER skip_one {trigger} BEGIN SELECT RAISE(IGNORE); END;"
        ));
        let out = fx.admin(&args);
        fx.sql("DROP TRIGGER skip_one");

        assert_exit(&out, 1);
        let (_, stderr) = text(&out);
        assert!(stderr.contains("were expected"), "{args:?}: {stderr}");
        assert!(stderr.contains("rolled back"), "{args:?}: {stderr}");
        assert_eq!(fx.dump(), before, "{args:?} left a partial change");
    }
}

/// `docker compose exec` runs as root unless told otherwise; a change made
/// as anyone but the database's owner is refused before it starts. Only
/// checkable where the test itself can hand the file to someone else.
#[cfg(target_os = "linux")]
#[test]
fn a_change_as_someone_other_than_the_owner_is_refused() {
    use std::os::unix::fs::MetadataExt;
    if std::fs::metadata("/proc/self").map(|m| m.uid()).ok() != Some(0) {
        return;
    }
    let fx = Fixture::new();
    let before = fx.dump();
    std::os::unix::fs::chown(fx.db(), Some(1000), Some(1000)).unwrap();
    let out = fx.admin(&["remove", OLD, "--yes"]);
    assert_exit(&out, 1);
    assert!(text(&out).1.contains("-u node"), "{:?}", text(&out));
    assert_eq!(fx.dump(), before);
    // Reading is fine as anyone.
    assert_exit(&fx.admin(&["list"]), 0);
}

// ---------------------------------------------------------------- beside a server

/// A real server on a real socket, serving the fixture's database file.
struct Running {
    addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    // Held, never read: the server runs on it, and stops when it drops.
    _rt: tokio::runtime::Runtime,
}

impl Running {
    fn start(db: &Path) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(Store::open(db).unwrap());
        let server = Server::new(
            Config {
                token: TOKEN.into(),
                merge_enabled: false,
                claude_bin: "definitely-not-a-real-binary".into(),
                rate_limit_max: 100_000,
                trusted_ip_header: String::new(),
                ..Config::default()
            },
            store,
        );
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        rt.spawn(async move {
            let _ = server
                .serve_with_shutdown(listener, async {
                    let _ = stopped.await;
                })
                .await;
        });
        Self {
            addr,
            stop: Some(stop),
            _rt: rt,
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // Give graceful shutdown a moment; dropping the runtime does the
        // rest.
        thread::sleep(Duration::from_millis(50));
    }
}

/// One HTTP/1.1 request over a plain socket, returning status and body.
fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp[9..12].parse().unwrap();
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn push(addr: SocketAddr, key: &str, path: &str) -> u16 {
    let body = format!(
        r#"{{"project_key":"{key}","file_path":"{path}","content":"x","source_env":"test"}}"#
    );
    http(addr, "POST", "/sync", &body).0
}

fn pulled(addr: SocketAddr, key: &str) -> usize {
    let (status, body) = http(addr, "GET", &format!("/sync?project_key={key}"), "");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["files"].as_array().unwrap().len()
}

/// The ordinary case: the server is up and taking pushes, a write is in
/// flight when the command starts, and the command waits its turn instead
/// of failing; the server sees the result on its next request.
#[test]
fn a_change_works_while_a_server_is_serving_the_same_file() {
    let fx = Fixture::new();
    let server = Running::start(&fx.db());
    let addr = server.addr;

    // Someone else holding the write lock as the command starts, as the
    // server does for the length of a push.
    let holder = Connection::open(fx.db()).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(700));
        holder.execute_batch("COMMIT").unwrap();
    });
    let pusher = thread::spawn(move || {
        (0..25)
            .map(|i| {
                thread::sleep(Duration::from_millis(10));
                push(addr, "busy/project", &format!("f{i}.md"))
            })
            .collect::<Vec<_>>()
    });

    let out = fx.admin(&["rename", OLD, "me/new", "--yes"]);
    release.join().unwrap();
    let statuses = pusher.join().unwrap();

    assert_exit(&out, 0);
    assert!(statuses.iter().all(|s| *s == 200), "{statuses:?}");
    assert_eq!(pulled(addr, "me/new"), 3);
    assert_eq!(pulled(addr, OLD), 0);
    assert_eq!(pulled(addr, "busy/project"), 25);
    assert_eq!(push(addr, "me/new", "after.md"), 200);
}

/// The case that has to fail cleanly: a reader that never lets go, so the
/// commit cannot get its exclusive lock. The command waits out the busy
/// timeout, rolls back, says so, and leaves the file as it found it, with
/// no journal behind and the server still serving.
#[test]
fn a_lock_held_past_the_busy_timeout_fails_cleanly() {
    let fx = Fixture::new();
    let server = Running::start(&fx.db());
    let before = fx.dump();

    let reader = Connection::open(fx.db()).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT count(*) FROM memory_files", [], |r| r.get(0))
        .unwrap();

    let started = Instant::now();
    let out = fx.admin(&["remove", OLD, "--yes"]);
    let waited = started.elapsed();
    reader.execute_batch("COMMIT").unwrap();

    assert_exit(&out, 1);
    let (_, stderr) = text(&out);
    assert!(stderr.contains("stayed locked"), "{stderr}");
    assert!(stderr.contains("The database was not changed."), "{stderr}");
    assert!(
        waited >= Duration::from_secs(4),
        "it waited for the lock rather than giving up at once: {waited:?}"
    );

    assert_eq!(fx.dump(), before);
    let journal = fx.dir.path().join("recall.db-journal");
    assert!(!journal.exists(), "no hot journal left behind");
    let ok: String = Connection::open(fx.db())
        .unwrap()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");
    assert_eq!(push(server.addr, OLD, "after.md"), 200);
    assert_eq!(pulled(server.addr, OLD), 4);
}

// ---------------------------------------------------------------- not over HTTP

/// The property `ARCHITECTURE.md` names: no route on the public listener
/// renames, removes or restores anything, whatever the credential. These
/// commands exist only as a subcommand.
#[tokio::test]
async fn no_http_route_renames_removes_or_restores() {
    let fx = Fixture::new();
    let before = fx.dump();
    let server = Server::new(
        Config {
            token: TOKEN.into(),
            merge_enabled: false,
            rate_limit_max: 100_000,
            ..Config::default()
        },
        Arc::new(Store::open(fx.db()).unwrap()),
    );
    for (method, path) in [
        ("POST", "/admin/rename"),
        ("POST", "/admin/remove"),
        ("POST", "/admin/restore"),
        ("POST", "/admin/stats"),
        ("DELETE", "/admin/stats"),
        ("DELETE", "/admin/projects/me%2Fthing"),
        ("POST", "/admin/projects/me%2Fthing/rename"),
        ("DELETE", "/sync?project_key=me/thing"),
        ("PUT", "/sync"),
        ("PATCH", "/sync"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"from":"me/thing","to":"x","project_key":"me/thing"}"#,
            ))
            .unwrap();
        let resp = server.router().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }
    assert_eq!(fx.dump(), before);
}

// ---------------------------------------------------------------- arguments

#[test]
fn admin_arguments_are_checked_like_the_servers_own() {
    let fx = Fixture::new();
    for args in [
        vec![],
        vec!["bogus"],
        vec!["remove"],
        vec!["remove", OLD, "--force"],
        vec!["rename", OLD],
    ] {
        let out = fx.admin(&args);
        assert_exit(&out, 2);
        assert!(text(&out).1.contains("Usage:"), "{args:?}");
    }
    let out = fx.admin(&["help"]);
    assert_exit(&out, 0);
    assert!(text(&out).0.contains("recall-server admin restore"));
}
