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
use recall_server::merge::Status;
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
        // The server creates the schema; the rows go in by hand, since
        // their exact timestamps are what the listings below are checked
        // against, and the store stamps what it writes with the time.
        drop(Store::open(dir.path().join("recall.db")).unwrap());
        let conn = Connection::open(dir.path().join("recall.db")).unwrap();
        conn.execute_batch(&format!(
            "INSERT INTO memory_files VALUES ('{OLD}', 'MEMORY.md', '- [notes](notes.md)\n', 'laptop', '{T0}', 0);
             INSERT INTO memory_files VALUES ('{OLD}', 'notes.md', 'a fact\n', NULL, '{T1}', 0);
             INSERT INTO memory_files VALUES ('{OLD}', 'old.md', 'kept after delete\n', 'laptop', '{T1}', 1);
             INSERT INTO memory_files VALUES ('me/thing', 'MEMORY.md', '- other\n', 'cloud', '{T1}', 0);
             INSERT INTO memory_files VALUES ('acme/app', 'unrelated.md', 'leave me\n', 'cloud', '{T0}', 0);"
        ))
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
            .env("RECALL_BACKUP_DIR", self.backups())
            // Every change waits out the server's merge window after it
            // commits. A millisecond here leaves the fixed margin, about a
            // second, instead of the 45 a real server's default costs; the
            // race test sets its own.
            .env("RECALL_MERGE_TIMEOUT_MS", "1");
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
    assert!(
        stdout.contains(&format!(
            "Confirmed with --yes for project key to rename \"{OLD}\", and key to rename it to \
             \"me/new\"."
        )),
        "{stdout}"
    );
    assert!(stdout.contains("Checked: the change held."), "{stdout}");
}

/// A typo in the target is the costlier one: it strands the project under
/// a key no machine asks for. So the target is typed back too.
#[test]
fn a_rename_goes_ahead_only_when_both_keys_are_typed_back() {
    let fx = Fixture::new();
    let before = fx.dump();
    for typed in [
        format!("{OLD}\n"),
        format!("{OLD}\nme/nwe\n"),
        format!("{OLD}\nme/new \n"),
        "me/new\nme/new\n".to_string(),
    ] {
        let out = fx.admin_typing(&["rename", OLD, "me/new"], &typed);
        assert_exit(&out, 1);
        let (stdout, _) = text(&out);
        assert_eq!(
            stdout.contains("Then type the key to rename it to, \"me/new\""),
            typed.starts_with(OLD),
            "asked for the target once the source was right: {stdout}"
        );
    }
    let out = fx.admin_typing(&["rename", OLD, "me/new"], &format!("{OLD}\nme/nwe\n"));
    assert!(
        text(&out).1.contains("\"me/nwe\" is not \"me/new\""),
        "{:?}",
        text(&out)
    );
    assert_eq!(fx.dump(), before);
    assert!(fx.snapshots().is_empty(), "no backup until it is confirmed");

    let out = fx.admin_typing(&["rename", OLD, "me/new"], &format!("{OLD}\nme/new\n"));
    assert_exit(&out, 0);
    assert_eq!(under(&fx.dump(), "me/new").len(), 3);
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
    // The advice is the safe fold, not "point the machine at it and let it
    // push", whose first pull overwrites that machine's files.
    assert!(stderr.contains("Do not fold"), "{stderr}");
    assert!(stderr.contains("memory directory aside first"), "{stderr}");
    assert!(stderr.contains(&format!("\"{OLD}.archived-20")), "{stderr}");
    assert!(!stderr.contains("then remove"), "{stderr}");
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

/// Removing a key whose notes were meant to have been folded elsewhere is
/// the moment content is lost without anyone noticing. The plan says how
/// many files hold content no other key has, before the confirmation.
#[test]
fn remove_warns_before_confirming_about_content_no_other_key_has() {
    let fx = Fixture::new();
    // MEMORY.md and notes.md are nowhere else; old.md is a tombstone.
    let out = fx.admin_typing(&["remove", OLD], "no\n");
    assert_exit(&out, 1);
    let (stdout, _) = text(&out);
    let warning = stdout
        .find("Warning: 2 of these file(s) hold content that no live file under any other key")
        .unwrap_or_else(|| panic!("{stdout}"));
    let prompt = stdout.find("To go ahead").unwrap();
    assert!(warning < prompt, "warned before asking: {stdout}");
    assert!(stdout.contains(&format!("\"{OLD}.archived-20")), "{stdout}");

    // Once notes.md's content is under another key too, at any path, only
    // MEMORY.md is left to warn about.
    fx.sql(&format!(
        "INSERT INTO memory_files VALUES ('me/thing', 'folded/notes.md', 'a fact\n', 'x', '{T1}', 0)"
    ));
    let out = fx.admin(&["remove", OLD, "--dry-run"]);
    assert_exit(&out, 0);
    let (stdout, _) = text(&out);
    assert!(stdout.contains("Warning: 1 of these file(s)"), "{stdout}");

    // A key whose every file is somewhere else gets no warning at all.
    fx.sql(&format!(
        "INSERT INTO memory_files VALUES ('copy', 'notes.md', 'a fact\n', 'x', '{T1}', 0)"
    ));
    let out = fx.admin(&["remove", "copy", "--dry-run"]);
    assert_exit(&out, 0);
    assert!(!text(&out).0.contains("Warning"), "{:?}", text(&out));
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

/// A backup's tombstone for a file that is live now is an old deletion,
/// and putting it back deletes the file on every machine at its next pull.
/// `--overwrite` alone does not do that; `--restore-deletions` must be
/// asked for as well.
#[test]
fn restore_turns_a_live_file_into_a_tombstone_only_with_restore_deletions() {
    let fx = Fixture::new();
    // The backup has old.md as a tombstone and notes.md as "a fact".
    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    let snap = snapshot.to_str().unwrap();
    // Since: old.md written again, on purpose, and notes.md edited.
    fx.sql(&format!(
        "UPDATE memory_files SET content = 'written again', deleted = 0,
             updated_at = '2026-09-23T00:00:00.000Z'
             WHERE project_key = '{OLD}' AND file_path = 'old.md';
         UPDATE memory_files SET content = 'edited since', updated_at = '2026-09-23T00:00:00.000Z'
             WHERE project_key = '{OLD}' AND file_path = 'notes.md';"
    ));
    let old_md = |fx: &Fixture| {
        under(&fx.dump(), OLD)
            .into_iter()
            .find(|r| r.1 == "old.md")
            .cloned()
            .unwrap()
    };

    let out = fx.admin(&["restore", snap, OLD, "--yes", "--overwrite"]);
    assert_exit(&out, 0);
    let (stdout, _) = text(&out);
    let skipped = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("skipped"))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(skipped.contains("old.md"), "{skipped}");
    assert!(skipped.contains("--restore-deletions"), "{skipped}");
    assert!(
        stdout.contains("1 live file(s) the backup has as deleted were left as they are"),
        "{stdout}"
    );
    assert!(!stdout.contains("tombstones, which deletes"), "{stdout}");
    let kept = old_md(&fx);
    assert_eq!((kept.2.as_str(), kept.5), ("written again", 0));
    let notes = under(&fx.dump(), OLD)
        .into_iter()
        .find(|r| r.1 == "notes.md")
        .cloned()
        .unwrap();
    assert_eq!(notes.2, "a fact\n", "the ordinary overwrite still happened");

    let out = fx.admin(&[
        "restore",
        snap,
        OLD,
        "--yes",
        "--overwrite",
        "--restore-deletions",
    ]);
    assert_exit(&out, 0);
    let (stdout, _) = text(&out);
    let delete = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("delete "))
        .unwrap_or_else(|| panic!("{stdout}"));
    assert!(delete.contains("old.md"), "{delete}");
    assert!(delete.contains("file -> tombstone"), "{delete}");
    assert!(
        stdout
            .contains("1 live file(s) turned into tombstones, which deletes them on every machine"),
        "{stdout}"
    );
    assert_eq!(old_md(&fx).5, 1, "a tombstone now, as asked");
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

/// A backup that fails part way leaves nothing behind: a partial file named
/// like a good snapshot is worse than none, since it sorts among them and
/// does not open. A file size limit cuts `VACUUM INTO` short for real; the
/// shell ignores SIGXFSZ first so the write fails with EFBIG instead of the
/// signal killing the process.
#[cfg(target_os = "linux")]
#[test]
fn a_backup_cut_short_leaves_no_partial_file_and_changes_nothing() {
    let fx = Fixture::new();
    let before = fx.dump();
    let out = Command::new("/bin/sh")
        .arg("-c")
        // A few 512-byte blocks: less than one database page, so the very
        // first page the backup writes is cut short.
        .arg(r#"trap '' XFSZ; ulimit -f 4 && exec "$0" admin remove "$1" --yes"#)
        .arg(env!("CARGO_BIN_EXE_recall-server"))
        .arg(OLD)
        .env_clear()
        .env("RECALL_DB_PATH", fx.db())
        .env("RECALL_BACKUP_DIR", fx.backups())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_exit(&out, 1);
    assert!(
        text(&out).1.contains("taking the backup"),
        "{:?}",
        text(&out)
    );
    assert!(
        fx.backups().join("admin").is_dir(),
        "the backup was started, so this is the case that matters"
    );
    assert_eq!(fx.snapshots(), Vec::<PathBuf>::new(), "no partial file");
    assert_eq!(fx.dump(), before);
}

/// A push that lands while the owner is reading the plan: the backup taken
/// after the confirmation does not hold what was shown, so nothing changes,
/// and that backup is deleted rather than left to pile up, one per retry.
#[test]
fn a_backup_that_does_not_hold_what_was_shown_stops_the_change_and_is_deleted() {
    let fx = Fixture::new();
    let mut child = fx
        .command(&["remove", OLD])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut shown = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&shown).contains("without the quotes: ") {
        let n = stdout.read(&mut buf).unwrap();
        assert!(
            n > 0,
            "ended before asking: {}",
            String::from_utf8_lossy(&shown)
        );
        shown.extend_from_slice(&buf[..n]);
    }
    fx.sql(&format!(
        "INSERT INTO memory_files VALUES ('{OLD}', 'late.md', 'arrived', 'x', '{T1}', 0)"
    ));
    let before = fx.dump();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{OLD}\n").as_bytes())
        .unwrap();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).unwrap();
    let out = child.wait_with_output().unwrap();

    assert_eq!(out.status.code(), Some(1), "{rest}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not hold the rows shown above"),
        "{stderr}"
    );
    assert!(stderr.contains("The database was not changed."), "{stderr}");
    assert!(rest.contains("Deleted that backup again"), "{rest}");
    assert!(fx.snapshots().is_empty(), "{:?}", fx.snapshots());
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

/// `docker exec` runs as root unless told otherwise; a change made as
/// anyone but the database's owner is refused before it starts. End to end
/// only where the test itself can hand the file to someone else, which is
/// as root; the decision itself is unit-tested in `admin.rs` everywhere.
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
        Self::start_with(db, None)
    }

    /// With `claude`, merge is on and runs that stand-in CLI, which the
    /// server is told is logged in.
    fn start_with(db: &Path, claude: Option<&Path>) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let store = Arc::new(Store::open(db).unwrap());
        let server = Server::new(
            Config {
                token: TOKEN.into(),
                merge_enabled: claude.is_some(),
                claude_bin: claude.map_or("definitely-not-a-real-binary".into(), |p| {
                    p.to_str().unwrap().to_string()
                }),
                merge_timeout: Duration::from_secs(20),
                rate_limit_max: 100_000,
                trusted_ip_header: String::new(),
                ..Config::default()
            },
            store,
        );
        server.set_claude_status(Status {
            checked_at: recall_server::now(),
            available: claude.is_some(),
            logged_in: claude.is_some(),
            error: String::new(),
        });
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        // The router, not `serve_with_shutdown`: that also starts the
        // background status probe, which would run the stand-in as `claude
        // auth status` and overwrite the status set above.
        let app = server
            .router()
            .into_make_service_with_connect_info::<SocketAddr>();
        rt.spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
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

/// A stand-in `claude` that takes `seconds` to merge, which is how long a
/// push holds a row it has read without holding any lock.
#[cfg(unix)]
fn slow_claude(dir: &Path, seconds: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("slow-claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\ncat > /dev/null\nsleep {seconds}\n\
             printf '%s' '{{\"is_error\":false,\"result\":\"merged late\"}}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The race a lock cannot close: a push reads a row, merges for a while
/// holding no lock, and writes after the change has committed, partly
/// undoing it. The command cannot prevent that from its side, so it waits
/// out the merge window and says exactly what came back and what to do.
#[cfg(unix)]
#[test]
fn a_push_being_merged_as_a_change_commits_is_reported_afterwards() {
    let fx = Fixture::new();
    let claude = slow_claude(fx.dir.path(), 2);
    let server = Running::start_with(&fx.db(), Some(&claude));
    let addr = server.addr;
    let snapshot = Store::open(fx.db())
        .unwrap()
        .backup(fx.dir.path().join("periodic"), 7)
        .unwrap();
    let snap = snapshot.to_str().unwrap().to_string();

    // A remove, with a push to notes.md already merging as it commits.
    let pushing = thread::spawn(move || push(addr, OLD, "notes.md"));
    thread::sleep(Duration::from_millis(500));
    let out = fx
        .command(&["remove", OLD, "--yes"])
        .env("RECALL_MERGE_TIMEOUT_MS", "4000")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(pushing.join().unwrap(), 200);
    assert_exit(&out, 3);
    let (stdout, stderr) = text(&out);
    assert!(stdout.contains("Done: removed 3 row(s)"), "{stdout}");
    assert!(stdout.contains("Waiting 5.0s"), "{stdout}");
    assert!(
        stderr.contains(&format!(
            "1 row(s) are under \"{OLD}\" again:\n  notes.md\n"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("run `recall-server admin remove {OLD}` again")),
        "{stderr}"
    );
    assert!(!stderr.contains("was not changed"), "{stderr}");
    let back = under(&fx.dump(), OLD)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].2, "merged late", "what the push wrote after");

    // A restore --overwrite, with a push to notes.md merging the version
    // the restore replaces.
    fx.sql(&format!(
        "UPDATE memory_files SET content = 'edited since' WHERE project_key = '{OLD}'"
    ));
    let pushing = thread::spawn(move || push(addr, OLD, "notes.md"));
    thread::sleep(Duration::from_millis(500));
    let out = fx
        .command(&["restore", &snap, OLD, "--yes", "--overwrite"])
        .env("RECALL_MERGE_TIMEOUT_MS", "4000")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(pushing.join().unwrap(), 200);
    assert_exit(&out, 3);
    let (_, stderr) = text(&out);
    assert!(
        stderr.contains(&format!(
            "1 restored row(s) under \"{OLD}\" are no longer what it wrote:\n  notes.md\n"
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "run `recall-server admin restore --overwrite {snap} {OLD}` again"
        )),
        "{stderr}"
    );

    // And with nothing in flight, the same check passes quietly.
    let out = fx.admin(&["restore", &snap, OLD, "--yes", "--overwrite"]);
    assert_exit(&out, 0);
    assert!(text(&out).0.contains("Checked: the change held."));
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
