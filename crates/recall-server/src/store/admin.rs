//! The queries behind `recall-server admin`: reading any database, a backup
//! included, and the three changes the owner can make to stored memory.
//!
//! Every change follows one shape, so no command can skip a step. A [`Plan`]
//! is computed and shown to the owner. Then [`Store::apply`] opens a write
//! transaction, computes the plan again *inside* it, and refuses unless the
//! two are identical, which closes the gap between what was confirmed and
//! what is about to happen (the server keeps accepting pushes meanwhile).
//! It runs the change, and commits only if SQLite's `changes()` equals the
//! number of rows the plan names; anything else rolls back.
//!
//! The backup is the caller's job, because `VACUUM INTO` cannot run inside a
//! transaction.
//!
//! None of this is reachable from the HTTP router. That is the point of it
//! living behind a subcommand; see "Admin commands" in `ARCHITECTURE.md`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior};

use super::Store;

/// How long a statement waits on a lock someone else holds (the server
/// mid-write, its own `VACUUM INTO` backup, sqlite-web mid-read) before
/// giving up with `SQLITE_BUSY`.
///
/// The server's connection waits the same 5 seconds: rusqlite sets that on
/// every connection it opens, and `Store::open` does not change it. Stated
/// here rather than inherited, so a change to that default cannot quietly
/// change how an admin command behaves against a running server.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Journal modes under which SQLite can roll a transaction back, including
/// after a crash half way through. `off` and `memory` cannot, and a change
/// to the only copy of someone's memory is not made without that.
const SAFE_JOURNAL_MODES: [&str; 4] = ["delete", "truncate", "persist", "wal"];

/// Whether an admin command may write to the database it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Listing, dry runs, and every backup file, which is never written to.
    Read,
    /// A change to the live database.
    Write,
}

/// One stored row, exactly as it is on disk.
///
/// `source_env` keeps the difference between NULL and `''`, and a
/// tombstone keeps its content, because a restore has to put back the row
/// that was there, not an approximation of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) file_path: String,
    pub(crate) content: String,
    pub(crate) source_env: Option<String>,
    pub(crate) updated_at: String,
    pub(crate) deleted: bool,
}

/// One project key, as `admin list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) project_key: String,
    pub(crate) files: i64,
    pub(crate) tombstones: i64,
    pub(crate) last_updated_at: String,
}

/// A change, worked out in full before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Move every row of one key to another, unoccupied, key.
    Rename {
        from: String,
        to: String,
        /// Every row under `from`: what moves.
        rows: Vec<Row>,
        /// Every row already under `to`. Must be empty.
        occupied: Vec<Row>,
    },
    /// Delete every row of one key.
    Remove { key: String, rows: Vec<Row> },
    /// Copy one key's rows from a backup into the live database.
    Restore(Restore),
}

/// What restoring one key from a backup would do, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Restore {
    pub(crate) key: String,
    /// Every row the backup holds for the key.
    pub(crate) backup: Vec<Row>,
    /// Backup rows with no live row at the same path: inserted.
    pub(crate) add: Vec<Row>,
    /// Live rows that differ from the backup's: `(live, backup)`. Replaced
    /// only when `allow_overwrite` is set.
    pub(crate) overwrite: Vec<(Row, Row)>,
    /// Paths whose live row is already identical to the backup's.
    pub(crate) unchanged: Vec<String>,
    /// Live paths the backup does not have. A restore never deletes, so
    /// these stay as they are.
    pub(crate) live_only: Vec<String>,
    /// `--overwrite`.
    pub(crate) allow_overwrite: bool,
}

impl Plan {
    /// How many rows the change must touch. [`Store::apply`] commits only
    /// if `changes()` agrees.
    pub(crate) fn expected_changes(&self) -> usize {
        match self {
            Plan::Rename { rows, .. } | Plan::Remove { rows, .. } => rows.len(),
            Plan::Restore(r) => r.add.len() + r.overwrite.len(),
        }
    }

    /// Why this change must not happen, if it must not.
    ///
    /// Checked twice: before the owner is asked to confirm, and again inside
    /// the transaction against the plan recomputed there. One function for
    /// both, so the two can never disagree about what is safe.
    pub(crate) fn refusal(&self) -> Option<String> {
        match self {
            Plan::Rename {
                from,
                to,
                rows,
                occupied,
            } => {
                if from == to {
                    return Some(format!("{from:?} and {to:?} are the same key"));
                }
                if let Some(why) = unusable_key(to) {
                    return Some(format!("{to:?} cannot be a project key: it {why}"));
                }
                if rows.is_empty() {
                    return Some(no_such_key(from));
                }
                if !occupied.is_empty() {
                    // Refused even when no path overlaps. The primary key is
                    // (project_key, file_path), so an overlap would fail
                    // outright, and a partial fold of one project into
                    // another has no single right answer for the paths that
                    // do overlap: the semantic merge is the server's job,
                    // through POST /sync, not this command's.
                    return Some(format!(
                        "{to:?} already holds {} row(s), and a rename only moves rows onto a \
                         key that holds none, since the primary key is (project_key, file_path). \
                         To fold {from:?} into {to:?}, set RECALL_PROJECT_KEY={to} on the machine \
                         that wrote {from:?} and let it push, which merges through the server; \
                         then remove {from:?}",
                        occupied.len()
                    ));
                }
                None
            }
            Plan::Remove { key, rows } => rows.is_empty().then(|| no_such_key(key)),
            Plan::Restore(r) => {
                if r.backup.is_empty() {
                    return Some(format!("the backup holds no rows for {:?}", r.key));
                }
                if !r.overwrite.is_empty() && !r.allow_overwrite {
                    return Some(format!(
                        "{} row(s) under {:?} already exist in the live database and differ \
                         from the backup (listed above as \"overwrite\"). Pass --overwrite to \
                         replace them with the backup's version",
                        r.overwrite.len(),
                        r.key
                    ));
                }
                None
            }
        }
    }

    /// The same plan, recomputed from what the database holds now.
    fn recompute(&self, conn: &Connection) -> Result<Plan> {
        match self {
            Plan::Rename { from, to, .. } => rename_plan(conn, from, to),
            Plan::Remove { key, .. } => remove_plan(conn, key),
            // The backup's rows are what they were when read: the file is
            // never written to, and re-reading it would compare it with
            // itself.
            Plan::Restore(r) => {
                restore_plan(conn, &r.key, r.backup.clone(), r.allow_overwrite).map(Plan::Restore)
            }
        }
    }

    /// Runs the change, returning the sum of `changes()` over every
    /// statement it ran.
    fn execute(&self, conn: &Connection) -> Result<usize> {
        Ok(match self {
            Plan::Rename { from, to, .. } => conn.execute(
                "UPDATE memory_files SET project_key = ?1 WHERE project_key = ?2",
                (to, from),
            )?,
            Plan::Remove { key, .. } => {
                conn.execute("DELETE FROM memory_files WHERE project_key = ?1", (key,))?
            }
            Plan::Restore(r) => {
                let mut changed = 0;
                // A plain INSERT, not an upsert: the plan says no row is
                // there, and if one is, failing is the right answer.
                for row in &r.add {
                    changed += conn.execute(
                        "INSERT INTO memory_files
                             (project_key, file_path, content, source_env, updated_at, deleted)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        (
                            &r.key,
                            &row.file_path,
                            &row.content,
                            &row.source_env,
                            &row.updated_at,
                            i64::from(row.deleted),
                        ),
                    )?;
                }
                for (_, row) in &r.overwrite {
                    changed += conn.execute(
                        "UPDATE memory_files
                         SET content = ?3, source_env = ?4, updated_at = ?5, deleted = ?6
                         WHERE project_key = ?1 AND file_path = ?2",
                        (
                            &r.key,
                            &row.file_path,
                            &row.content,
                            &row.source_env,
                            &row.updated_at,
                            i64::from(row.deleted),
                        ),
                    )?;
                }
                changed
            }
        })
    }
}

impl Store {
    /// Opens a database that must already exist, without creating or
    /// migrating anything.
    ///
    /// [`Store::open`] creates a missing file, which is right for a server
    /// starting on an empty volume and wrong here: a mistyped path would
    /// list as an empty database, which is the worst thing to show someone
    /// already cleaning up a mistake. And a backup is evidence; opening one
    /// must not change it, so [`Access::Read`] opens read-only.
    pub(crate) fn open_existing(path: &Path, access: Access) -> Result<Self> {
        if !path.is_file() {
            bail!("there is no database at {}", path.display());
        }
        let flags = match access {
            Access::Read => OpenFlags::SQLITE_OPEN_READ_ONLY,
            Access::Write => OpenFlags::SQLITE_OPEN_READ_WRITE,
        } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)
            .with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // SQLITE_OPEN_READ_WRITE quietly falls back to read-only when the
        // file is not writable, and the first write would then fail half
        // way through a command that has already taken its backup.
        if access == Access::Write && conn.is_readonly(rusqlite::DatabaseName::Main)? {
            bail!("{} is not writable by this user", path.display());
        }

        let columns =
            memory_files_columns(&conn).with_context(|| format!("reading {}", path.display()))?;
        if columns.is_empty() {
            bail!(
                "{} has no memory_files table, so it is not a Recall database",
                path.display()
            );
        }
        let mut required = vec![
            "project_key",
            "file_path",
            "content",
            "source_env",
            "updated_at",
        ];
        // A backup from before tombstones existed has no `deleted` column,
        // and every row in it is live; reading one is fine. The live
        // database always has it, because the server adds it on start.
        if access == Access::Write {
            required.push("deleted");
        }
        if let Some(missing) = required.iter().find(|c| !columns.iter().any(|x| x == *c)) {
            bail!(
                "{}'s memory_files has no {missing} column. If this is the live database, \
                 start the server once so it brings the schema up to date",
                path.display()
            );
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Refuses a journal mode under which a transaction cannot be rolled
    /// back. Changing the mode is not this command's business: switching the
    /// file to WAL, say, would break sqlite-web, whose volume is mounted
    /// read-only and so cannot create the `-shm` file WAL needs.
    pub(crate) fn check_journal_mode(&self) -> Result<String> {
        let conn = self.lock();
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let mode = mode.to_ascii_lowercase();
        if !SAFE_JOURNAL_MODES.contains(&mode.as_str()) {
            bail!(
                "the database's journal_mode is {mode}, under which SQLite cannot roll back a \
                 transaction that fails half way"
            );
        }
        Ok(mode)
    }

    /// Every project key with its counts, ordered by key.
    pub(crate) fn summaries(&self) -> Result<Vec<Summary>> {
        let conn = self.lock();
        let deleted = deleted_column(&conn)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT project_key,
                    SUM(CASE WHEN {deleted} = 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN {deleted} = 0 THEN 0 ELSE 1 END),
                    MAX(updated_at)
             FROM memory_files GROUP BY project_key ORDER BY project_key"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(Summary {
                project_key: r.get(0)?,
                files: r.get(1)?,
                tombstones: r.get(2)?,
                last_updated_at: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            })
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Every row stored under exactly `key`, ordered by path.
    pub(crate) fn rows(&self, key: &str) -> Result<Vec<Row>> {
        rows_under(&self.lock(), key)
    }

    /// What renaming `from` to `to` would do.
    pub(crate) fn plan_rename(&self, from: &str, to: &str) -> Result<Plan> {
        rename_plan(&self.lock(), from, to)
    }

    /// What removing `key` would do.
    pub(crate) fn plan_remove(&self, key: &str) -> Result<Plan> {
        remove_plan(&self.lock(), key)
    }

    /// What copying `backup` (the rows a backup holds for `key`) into this
    /// database would do.
    pub(crate) fn plan_restore(
        &self,
        key: &str,
        backup: Vec<Row>,
        allow_overwrite: bool,
    ) -> Result<Plan> {
        restore_plan(&self.lock(), key, backup, allow_overwrite).map(Plan::Restore)
    }

    /// Makes the change `shown` describes, in one transaction, or not at
    /// all. Returns how many rows changed.
    ///
    /// `BEGIN IMMEDIATE` takes the write lock up front, so this waits (for
    /// [`BUSY_TIMEOUT`]) behind a server write rather than failing part way
    /// through; the server only ever runs single-statement transactions, so
    /// it waits behind this one the same way and nothing can deadlock.
    pub(crate) fn apply(&self, shown: &Plan) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| busy(e, "starting the change"))?;

        // From here, any early return drops `tx`, and rusqlite rolls back a
        // transaction that is dropped without being committed.
        let now = shown.recompute(&tx)?;
        if now != *shown {
            bail!(
                "the rows involved changed after they were shown (a push arrived meanwhile?). \
                 Rolled back. Run the command again to see them as they are now"
            );
        }
        if let Some(why) = now.refusal() {
            bail!("refusing: {why}");
        }

        let changed = shown.execute(&tx)?;
        let expected = shown.expected_changes();
        if changed != expected {
            tx.rollback()?;
            bail!(
                "SQLite changed {changed} row(s) where {expected} were expected, so the \
                 transaction was rolled back. Something other than this command is acting on \
                 these rows (a trigger?); look before trying again"
            );
        }
        tx.commit().map_err(|e| busy(e, "committing"))?;
        Ok(changed)
    }
}

/// Why `key` is no good as the target of a rename, if it is not.
///
/// The wire only asks for a non-empty key. These are stricter because no
/// client derives such a key, and a key that differs from another only by a
/// trailing space would be invisible in every listing: a rename onto one is
/// a typo that strands a project where no machine will ever pull it.
fn unusable_key(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        Some("is empty")
    } else if key.trim() != key {
        Some("starts or ends with whitespace")
    } else if key.chars().any(char::is_control) {
        Some("contains a control character")
    } else {
        None
    }
}

fn no_such_key(key: &str) -> String {
    format!(
        "no project key is exactly {key:?} (keys are matched exactly, never by prefix or pattern)"
    )
}

fn rename_plan(conn: &Connection, from: &str, to: &str) -> Result<Plan> {
    Ok(Plan::Rename {
        from: from.to_owned(),
        to: to.to_owned(),
        rows: rows_under(conn, from)?,
        occupied: rows_under(conn, to)?,
    })
}

fn remove_plan(conn: &Connection, key: &str) -> Result<Plan> {
    Ok(Plan::Remove {
        key: key.to_owned(),
        rows: rows_under(conn, key)?,
    })
}

fn restore_plan(
    conn: &Connection,
    key: &str,
    backup: Vec<Row>,
    allow_overwrite: bool,
) -> Result<Restore> {
    let live = rows_under(conn, key)?;
    let live_by_path: HashMap<&str, &Row> =
        live.iter().map(|r| (r.file_path.as_str(), r)).collect();

    let mut add = Vec::new();
    let mut overwrite = Vec::new();
    let mut unchanged = Vec::new();
    for row in &backup {
        match live_by_path.get(row.file_path.as_str()) {
            None => add.push(row.clone()),
            Some(current) if *current == row => unchanged.push(row.file_path.clone()),
            Some(current) => overwrite.push(((*current).clone(), row.clone())),
        }
    }
    let live_only = live
        .iter()
        .filter(|r| !backup.iter().any(|b| b.file_path == r.file_path))
        .map(|r| r.file_path.clone())
        .collect();
    Ok(Restore {
        key: key.to_owned(),
        backup,
        add,
        overwrite,
        unchanged,
        live_only,
        allow_overwrite,
    })
}

/// Exact match only: `=`, never `LIKE` or `GLOB`, so a key containing `%`,
/// `_` or `*` means those characters and nothing else.
fn rows_under(conn: &Connection, key: &str) -> Result<Vec<Row>> {
    let deleted = deleted_column(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT file_path, content, source_env, updated_at, {deleted}
         FROM memory_files WHERE project_key = ?1 ORDER BY file_path"
    ))?;
    let rows = stmt.query_map((key,), |r| {
        Ok(Row {
            file_path: r.get(0)?,
            content: r.get(1)?,
            source_env: r.get(2)?,
            updated_at: r.get(3)?,
            deleted: r.get::<_, i64>(4)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

fn memory_files_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA table_info(memory_files)")?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    names.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

/// The expression that reads a row's tombstone flag: the column, or `0` in
/// a backup that predates it. A fixed string either way, never user input,
/// so it is safe to put into SQL text.
fn deleted_column(conn: &Connection) -> Result<&'static str> {
    Ok(
        if memory_files_columns(conn)?.iter().any(|c| c == "deleted") {
            "deleted"
        } else {
            "0"
        },
    )
}

/// Says plainly that the database stayed locked, which is the one failure
/// here the owner can do something about (wait, and run it again).
fn busy(err: rusqlite::Error, doing: &str) -> anyhow::Error {
    match err.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => anyhow!(
            "the database stayed locked for {} seconds while {doing} (the server mid-write or \
             mid-backup, or sqlite-web mid-read). Rolled back. Run the command again",
            BUSY_TIMEOUT.as_secs()
        ),
        _ => anyhow::Error::from(err).context(format!("{doing} failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-09-01T00:00:00.000Z";

    fn live(dir: &tempfile::TempDir) -> (std::path::PathBuf, Store) {
        let path = dir.path().join("recall.db");
        let st = Store::open(&path).unwrap();
        st.upsert("old/key", "a.md", "alpha", "laptop", T0).unwrap();
        st.upsert("old/key", "b.md", "beta", "", T0).unwrap();
        st.tombstone("old/key", "c.md", "cloud", T0).unwrap();
        st.upsert("other", "a.md", "other alpha", "laptop", T0)
            .unwrap();
        drop(st);
        let st = Store::open_existing(&path, Access::Write).unwrap();
        (path, st)
    }

    /// The window between showing a plan and applying it is real: the
    /// server keeps taking pushes. A row that arrives in it must stop the
    /// change, not ride along unconfirmed.
    #[test]
    fn a_row_that_arrives_after_the_plan_was_shown_stops_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let (path, st) = live(&dir);
        let plan = st.plan_remove("old/key").unwrap();
        assert_eq!(plan.expected_changes(), 3);

        Store::open(&path)
            .unwrap()
            .upsert("old/key", "late.md", "arrived", "laptop", T0)
            .unwrap();

        let err = st.apply(&plan).unwrap_err().to_string();
        assert!(err.contains("changed after they were shown"), "{err}");
        assert_eq!(st.rows("old/key").unwrap().len(), 4, "nothing removed");
    }

    /// Two layers ask the same question: a plan that would overwrite, built
    /// without permission, is refused by `apply` itself and not only by the
    /// command that built it.
    #[test]
    fn apply_refuses_an_overwrite_that_was_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, st) = live(&dir);
        let mut backup = st.rows("old/key").unwrap();
        backup[0].content = "older alpha".into();

        let plan = st.plan_restore("old/key", backup.clone(), false).unwrap();
        let Plan::Restore(r) = &plan else {
            unreachable!()
        };
        assert_eq!(r.overwrite.len(), 1);
        assert_eq!(r.unchanged.len(), 2);
        let err = st.apply(&plan).unwrap_err().to_string();
        assert!(err.contains("--overwrite"), "{err}");
        assert_eq!(st.rows("old/key").unwrap()[0].content, "alpha");

        let plan = st.plan_restore("old/key", backup, true).unwrap();
        assert_eq!(st.apply(&plan).unwrap(), 1);
        assert_eq!(st.rows("old/key").unwrap()[0].content, "older alpha");
    }

    #[test]
    fn restore_sorts_every_backup_row_into_exactly_one_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let (_, st) = live(&dir);
        let mut backup = st.rows("old/key").unwrap();
        backup.remove(1); // b.md: live only
        backup[1].deleted = false; // c.md: differs
        backup.push(Row {
            file_path: "new.md".into(),
            content: "new".into(),
            source_env: None,
            updated_at: T0.into(),
            deleted: false,
        });
        let Plan::Restore(r) = st.plan_restore("old/key", backup, false).unwrap() else {
            unreachable!()
        };
        assert_eq!(r.unchanged, vec!["a.md"]);
        assert_eq!(r.overwrite.len(), 1);
        assert_eq!(r.overwrite[0].1.file_path, "c.md");
        assert_eq!(r.add.len(), 1);
        assert_eq!(r.add[0].file_path, "new.md");
        assert_eq!(r.live_only, vec!["b.md"]);
    }

    #[test]
    fn opening_a_missing_database_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("typo.db");
        for access in [Access::Read, Access::Write] {
            let err = Store::open_existing(&path, access).err().unwrap();
            assert!(err.to_string().contains("no database"), "{err}");
        }
        assert!(!path.exists());
    }

    #[test]
    fn a_backup_from_before_tombstones_reads_as_all_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE memory_files (
                    project_key TEXT NOT NULL, file_path TEXT NOT NULL,
                    content TEXT NOT NULL, source_env TEXT, updated_at TEXT NOT NULL,
                    PRIMARY KEY (project_key, file_path));
                 INSERT INTO memory_files VALUES ('k', 'a.md', 'x', NULL, '2026-01-01T00:00:00.000Z');",
            )
            .unwrap();
        let st = Store::open_existing(&path, Access::Read).unwrap();
        let rows = st.rows("k").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].deleted);
        assert_eq!(st.summaries().unwrap()[0].files, 1);
        // Never as the live database, though: the server always has the
        // column, so its absence there means something is wrong.
        assert!(Store::open_existing(&path, Access::Write).is_err());
    }

    #[test]
    fn a_rename_target_no_client_could_derive_is_refused() {
        for (to, why) in [
            ("", "empty"),
            (" me/thing", "whitespace"),
            ("me/thing\n", "whitespace"),
            ("me/\u{7}thing", "control"),
        ] {
            let plan = Plan::Rename {
                from: "a".into(),
                to: to.into(),
                rows: vec![],
                occupied: vec![],
            };
            let refusal = plan.refusal().unwrap();
            assert!(refusal.contains(why), "{to:?}: {refusal}");
        }
    }
}
