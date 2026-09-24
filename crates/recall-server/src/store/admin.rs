//! The queries behind `recall-server admin`: reading any database, a backup
//! included, and the three changes the owner can make to stored memory.
//!
//! Every change follows one shape, so no command can skip a step. A [`Plan`]
//! is computed and shown to the owner. Then [`Store::apply`] opens a write
//! transaction, computes the plan again *inside* it, and refuses unless the
//! two are identical, which closes the gap between what was confirmed and
//! what is about to happen (the server keeps accepting pushes meanwhile).
//! It runs the change, closes the open merge jobs for the rows it touched,
//! appends the change's audit leaf, and commits only if SQLite's
//! `changes()` equals the number of rows and jobs the plan names; anything
//! else rolls back, the leaf included.
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
use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::jobs::{close_for_admin, AdminClose};
use super::{Outcome, Store};

/// How long a statement waits on a lock someone else holds before giving
/// up with `SQLITE_BUSY`. In WAL, which the server keeps the file in, that
/// is only another writer: the server mid-write, or another command. A
/// reader (sqlite-web, a backup being taken) no longer holds up a write;
/// under the rollback journal of a file no current server has opened yet,
/// it still does.
///
/// The server's connection waits the same 5 seconds: `Store::open` sets
/// this same constant. Stated rather than inherited from rusqlite's
/// default, so a change to that default cannot quietly change how either
/// side behaves beside the other.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// A job still queued or leased for a row a change touches, which the
/// change closes (see `close_for_admin` in the store's jobs module).
///
/// Two are equal when they are the same job for the same file, whatever
/// state each was read in: a worker leasing a queued job between showing a
/// plan and applying it changes nothing about what the change does to it.
#[derive(Debug, Clone, Eq)]
pub(crate) struct OpenJob {
    pub(crate) id: String,
    pub(crate) file_path: String,
    /// `queued` or `leased`, as last read.
    pub(crate) state: String,
}

impl PartialEq for OpenJob {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.file_path == other.file_path
    }
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
        /// Every open job under `from`: closed.
        jobs: Vec<OpenJob>,
    },
    /// Delete every row of one key.
    Remove {
        key: String,
        rows: Vec<Row>,
        /// Every open job under `key`: closed.
        jobs: Vec<OpenJob>,
    },
    /// Copy one key's rows from a backup into the live database.
    Restore(Restore),
}

/// What [`Store::apply`] did: the rows it changed, and the jobs it closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) rows: usize,
    pub(crate) jobs: Vec<String>,
}

/// What a committed change's check afterwards found back on the rows it
/// touched: see [`Plan::undone`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Undone {
    /// Paths whose row no longer shows the change.
    pub(crate) paths: Vec<String>,
    /// Jobs open for those rows again: each was queued by a push that
    /// landed after the change committed.
    pub(crate) jobs: Vec<OpenJob>,
}

impl Undone {
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.jobs.is_empty()
    }
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
    /// only when `allow_overwrite` is set. Never a live file the backup has
    /// as a tombstone: those are `deletions`.
    pub(crate) overwrite: Vec<(Row, Row)>,
    /// Live files the backup has as tombstones: `(live, backup)`.
    ///
    /// Kept apart from `overwrite` because replacing one is a deletion, and
    /// not only on the server: every machine removes the file at its next
    /// pull. A backup's tombstone is an old decision, and the file may have
    /// been written again since on purpose, so these are skipped unless
    /// `allow_deletions` is set, and listed either way.
    pub(crate) deletions: Vec<(Row, Row)>,
    /// Paths whose live row is already identical to the backup's.
    pub(crate) unchanged: Vec<String>,
    /// Live paths the backup does not have. A restore never removes a row,
    /// so these stay as they are.
    pub(crate) live_only: Vec<String>,
    /// `--overwrite`.
    pub(crate) allow_overwrite: bool,
    /// `--restore-deletions`, which the command accepts only together with
    /// `--overwrite`.
    pub(crate) allow_deletions: bool,
    /// Every open job under the key for a path this restore writes: closed.
    /// A job for a path it leaves alone (unchanged, live only, a skipped
    /// deletion) stays open, since its row is as the job left it.
    pub(crate) jobs: Vec<OpenJob>,
}

impl Restore {
    /// The deletions this restore makes, which is none unless allowed.
    pub(crate) fn applied_deletions(&self) -> &[(Row, Row)] {
        if self.allow_deletions {
            &self.deletions
        } else {
            &[]
        }
    }

    /// The deletions this restore leaves as they are.
    pub(crate) fn skipped_deletions(&self) -> &[(Row, Row)] {
        if self.allow_deletions {
            &[]
        } else {
            &self.deletions
        }
    }

    /// Whether it replaces any live row, rather than only adding rows where
    /// there were none. Only a replacement can be undone by a push that was
    /// already in flight; see [`Plan::undone`].
    pub(crate) fn replaces_live_rows(&self) -> bool {
        !self.overwrite.is_empty() || !self.applied_deletions().is_empty()
    }

    /// Every row this restore writes, as it will be written.
    pub(crate) fn written(&self) -> impl Iterator<Item = &Row> {
        self.add.iter().chain(
            self.overwrite
                .iter()
                .chain(self.applied_deletions())
                .map(|(_, backup)| backup),
        )
    }
}

/// Why a change was abandoned before it wrote anything: the write lock never
/// came, or what the database held once it did was no longer what the owner
/// had been shown.
///
/// A type of its own so the command can tell these apart from every other
/// failure. They are the ones in which the backup the command took is known
/// to be of no use, having been taken for a change that never began, so the
/// command deletes it rather than leaving one behind per retry.
#[derive(Debug)]
pub(crate) struct Abandoned(pub(crate) String);

impl std::fmt::Display for Abandoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Abandoned {}

impl Plan {
    /// How many rows the change must touch. [`Store::apply`] commits only
    /// if `changes()` agrees.
    pub(crate) fn expected_changes(&self) -> usize {
        match self {
            Plan::Rename { rows, .. } | Plan::Remove { rows, .. } => rows.len(),
            Plan::Restore(r) => r.add.len() + r.overwrite.len() + r.applied_deletions().len(),
        }
    }

    /// The open jobs the change closes, in the same transaction.
    pub(crate) fn jobs(&self) -> &[OpenJob] {
        match self {
            Plan::Rename { jobs, .. } | Plan::Remove { jobs, .. } => jobs,
            Plan::Restore(r) => &r.jobs,
        }
    }

    /// The key whose rows the change touches: a rename's source.
    fn key(&self) -> &str {
        match self {
            Plan::Rename { from, .. } => from,
            Plan::Remove { key, .. } => key,
            Plan::Restore(r) => &r.key,
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
                ..
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
                    // do overlap.
                    //
                    // The advice matters as much as the refusal. The obvious
                    // fold, pointing the old machine at `to` and letting it
                    // push, loses data: its first pull writes `to`'s version
                    // of every file both keys have over its own before
                    // anything is pushed, and backfill skips a file that
                    // differs. So the machine's files are copied aside
                    // first, and the old key is kept under another name
                    // rather than removed.
                    let archive = archive_key(from);
                    return Some(format!(
                        "{to:?} already holds {} row(s), and a rename only moves rows onto a \
                         key that holds none, since the primary key is (project_key, file_path). \
                         Do not fold {from:?} into {to:?} by pointing its machine at {to:?} and \
                         letting it push: that machine's first pull writes {to:?}'s version of \
                         every file both keys have over its own before it pushes anything. \
                         Instead, copy that machine's memory directory aside first (`recall \
                         status` prints where it is), rename {from:?} to an archive key such as \
                         {archive:?} rather than removing it, set RECALL_PROJECT_KEY={to} on \
                         that machine, and bring back by hand what the copy has that {to:?} \
                         lacks. \"Folding one key into another\" in deploy/README.md has the \
                         steps",
                        occupied.len()
                    ));
                }
                None
            }
            Plan::Remove { key, rows, .. } => rows.is_empty().then(|| no_such_key(key)),
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
            Plan::Restore(r) => restore_plan(
                conn,
                &r.key,
                r.backup.clone(),
                r.allow_overwrite,
                r.allow_deletions,
            )
            .map(Plan::Restore),
        }
    }

    /// What is wrong with `snapshot` as the backup of this change, if
    /// anything: it must hold, row for row, what the change replaces.
    ///
    /// `VACUUM INTO` reporting success is not the same as the file holding
    /// what the owner was shown. The file could be cut short by a full disk
    /// that SQLite did not notice, or a push could land between showing the
    /// plan and taking the backup, and then the backup is of a state nobody
    /// looked at. Either way the change must not go ahead on its strength.
    /// A rename and a remove compare every row of the keys involved; a
    /// restore compares the rows it replaces, and counts the rest.
    pub(crate) fn snapshot_mismatch(&self, snapshot: &Store) -> Result<Option<String>> {
        let differs = |key: &str, held: &[Row], shown: usize| {
            format!(
                "it holds {} row(s) under {key:?} where {shown} were shown",
                held.len()
            )
        };
        Ok(match self {
            Plan::Rename {
                from,
                to,
                rows,
                occupied,
                ..
            } => {
                let held = snapshot.rows(from)?;
                let target = snapshot.rows(to)?;
                if held != *rows {
                    Some(differs(from, &held, rows.len()))
                } else if target != *occupied {
                    Some(differs(to, &target, occupied.len()))
                } else {
                    None
                }
            }
            Plan::Remove { key, rows, .. } => {
                let held = snapshot.rows(key)?;
                (held != *rows).then(|| differs(key, &held, rows.len()))
            }
            Plan::Restore(r) => {
                let held = snapshot.rows(&r.key)?;
                let shown =
                    r.overwrite.len() + r.deletions.len() + r.unchanged.len() + r.live_only.len();
                let replaced_held = r
                    .overwrite
                    .iter()
                    .chain(&r.deletions)
                    .all(|(live, _)| held.contains(live));
                (held.len() != shown || !replaced_held).then(|| differs(&r.key, &held, shown))
            }
        })
    }

    /// Paths where, some time after this change committed, the database no
    /// longer shows it: rows under the key a rename or remove emptied, or a
    /// restored row that is no longer what the restore wrote; and the jobs
    /// open for those rows again, which only a push since can have queued,
    /// since the change closed every one there was.
    ///
    /// The one cause the admin command waits for is a push that was in
    /// flight: the server reads the stored row, may merge for up to
    /// `RECALL_MERGE_TIMEOUT_MS` holding no lock, and then writes, so a push
    /// that read before the change committed writes after it, under the old
    /// key or over the restored row. The other is a machine still syncing
    /// under the old key, or editing a restored file, which the same check
    /// sees just as well.
    pub(crate) fn undone(&self, store: &Store) -> Result<Undone> {
        let conn = store.lock();
        let paths = |rows: Vec<Row>| rows.into_iter().map(|r| r.file_path).collect();
        let open = open_jobs(&conn, self.key())?;
        Ok(match self {
            Plan::Rename { from, .. } => Undone {
                paths: paths(rows_under(&conn, from)?),
                jobs: open,
            },
            Plan::Remove { key, .. } => Undone {
                paths: paths(rows_under(&conn, key)?),
                jobs: open,
            },
            Plan::Restore(r) => {
                let live = rows_under(&conn, &r.key)?;
                Undone {
                    paths: r
                        .written()
                        .filter(|w| !live.contains(w))
                        .map(|w| w.file_path.clone())
                        .collect(),
                    jobs: open
                        .into_iter()
                        .filter(|j| r.written().any(|w| w.file_path == j.file_path))
                        .collect(),
                }
            }
        })
    }

    /// Closes the open jobs for the rows the change touches, stamped `now`,
    /// returning the sum of `changes()` over every statement it ran.
    fn close_jobs(&self, conn: &Connection, now: &str) -> Result<usize> {
        Ok(match self {
            Plan::Rename { from, to, .. } => {
                close_for_admin(conn, from, None, &AdminClose::Renamed { to }, now)?
            }
            Plan::Remove { key, .. } => {
                close_for_admin(conn, key, None, &AdminClose::Removed, now)?
            }
            Plan::Restore(r) => {
                let mut paths: Vec<&str> = r.jobs.iter().map(|j| j.file_path.as_str()).collect();
                paths.dedup();
                let mut closed = 0;
                for path in paths {
                    closed +=
                        close_for_admin(conn, &r.key, Some(path), &AdminClose::Restored, now)?;
                }
                closed
            }
        })
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
                for (_, row) in r.overwrite.iter().chain(r.applied_deletions()) {
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
        // The journal mode is the file's own and is left alone: WAL once a
        // current server has opened it. How durable a commit is, though, is
        // this connection's setting, and a change here is acknowledged to
        // the owner just as a push is to a client, so it is synced as the
        // server's are (see `use_durable_wal` in the store).
        conn.execute_batch("PRAGMA synchronous = FULL")?;

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
        // A change closes the jobs open for its rows and appends its leaf,
        // so the live database needs both tables. The server creates them
        // on start; a file without them is one no current server has
        // opened.
        if access == Access::Write {
            for table in ["jobs", "audit_log"] {
                if !has_table(&conn, table)? {
                    bail!(
                        "{} has no {table} table. If this is the live database, start the \
                         server once so it brings the schema up to date",
                        path.display()
                    );
                }
            }
        }
        // The audit tree starts empty rather than read here: listing and a
        // dry run never append, and a backup's log is not this one. A change
        // reads the log in, checked, before it appends: most of it ahead of
        // its transaction (`Store::audit_read_ahead`), the rest under the
        // write lock, as `Store::audited_each` catches up with the table.
        Ok(Self {
            state: Mutex::new(super::StoreState {
                conn,
                audit: crate::audit::merkle::Tree::new(),
                audit_at: String::new(),
            }),
        })
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
        allow_deletions: bool,
    ) -> Result<Plan> {
        restore_plan(&self.lock(), key, backup, allow_overwrite, allow_deletions).map(Plan::Restore)
    }

    /// The live files under `key` whose content no live row under any other
    /// key has, by path.
    ///
    /// Removing these is removing that content from the live database
    /// altogether, which is what `remove` warns about. Content is compared
    /// whole and byte for byte, at any path: this answers "is it anywhere
    /// else", not "did it move". A tombstone elsewhere does not count, since
    /// no machine will ever pull its content.
    pub(crate) fn unique_live_paths(&self, key: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT a.file_path FROM memory_files a
             WHERE a.project_key = ?1 AND a.deleted = 0
               AND NOT EXISTS (
                   SELECT 1 FROM memory_files b
                   WHERE b.project_key != ?1 AND b.deleted = 0 AND b.content = a.content)
             ORDER BY a.file_path",
        )?;
        let paths = stmt.query_map((key,), |r| r.get(0))?;
        paths.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Makes the change `shown` describes, in one transaction, or not at
    /// all: the rows, the open jobs for them closed, and the leaf
    /// `build_leaf` makes from what it did. Returns how many rows changed
    /// and which jobs it closed.
    ///
    /// `BEGIN IMMEDIATE` takes the write lock up front, so this waits (for
    /// [`BUSY_TIMEOUT`]) behind a server write rather than failing part way
    /// through; the server's statements are each their own transaction, so
    /// each waits behind this one the same way and nothing can deadlock.
    ///
    /// What the lock cannot do is order this change against a push as a
    /// whole. A push is several statements: the server reads the stored
    /// row, may merge for up to `RECALL_MERGE_TIMEOUT_MS` holding no lock,
    /// then writes. A push that read before this commits writes after it,
    /// and so partly undoes it: it puts a row back under the key a rename or
    /// remove emptied, or writes a merge of the old row over a restored one.
    /// This function cannot see that, since it happens after it returns; the
    /// command waits out the merge window and checks with [`Plan::undone`].
    ///
    /// A job is different: one open for these rows is closed here, in the
    /// same transaction, so a worker's result that arrives after the commit
    /// finds its job settled and changes nothing.
    ///
    /// The leaf goes through [`Store::audited_each`], as `reset-passkeys`'s
    /// does: the transaction first reads in every leaf a running server
    /// appended since this store last looked, so this one takes the next
    /// `seq`, and the server reads it in before its own next append. The
    /// log is read ahead of the transaction first, so the write lock is not
    /// held for the length of a long one.
    pub(crate) fn apply(
        &self,
        shown: &Plan,
        build_leaf: impl FnOnce(u64, &str, &Applied) -> Vec<u8>,
    ) -> Result<Applied> {
        self.audit_read_ahead()
            .context("reading the audit log in, before appending this change's leaf")?;
        self.audited_each_as(
            |e| Abandoned(format!("{:#}", busy(e, "starting the change"))).into(),
            |e| busy(e, "committing"),
            // Any error returned from here drops the transaction, which
            // rolls it back: rows, jobs and leaf alike.
            |tx, at| {
                let now = shown.recompute(tx)?;
                if now != *shown {
                    return Err(Abandoned(
                        "the rows or merge jobs involved changed after they were shown (a push \
                         arrived meanwhile?). Rolled back. Run the command again to see them as \
                         they are now"
                            .into(),
                    )
                    .into());
                }
                if let Some(why) = now.refusal() {
                    bail!("refusing: {why}");
                }

                let changed = shown.execute(tx)?;
                let expected = shown.expected_changes();
                if changed != expected {
                    bail!(
                        "SQLite changed {changed} row(s) where {expected} were expected, so the \
                         transaction was rolled back. Something other than this command is \
                         acting on these rows (a trigger?); look before trying again"
                    );
                }
                let closed = shown.close_jobs(tx, at)?;
                let open = shown.jobs().len();
                if closed != open {
                    bail!(
                        "SQLite closed {closed} merge job(s) where {open} were expected, so the \
                         transaction was rolled back. Something other than this command is \
                         acting on these jobs (a trigger?); look before trying again"
                    );
                }
                Ok(Outcome::Commit(Applied {
                    rows: changed,
                    jobs: shown.jobs().iter().map(|j| j.id.clone()).collect(),
                }))
            },
            |seq, at, applied| vec![build_leaf(seq, at, applied)],
        )
    }
}

/// Why `key` is no good as the target of a rename, if it is not.
///
/// The wire only asks for a non-empty key. These are stricter because no
/// client derives such a key, and a key that differs from another only by a
/// trailing space would be invisible in every listing: a rename onto one is
/// a typo that strands a project where no machine will ever pull it. The
/// same goes for a character that prints as nothing at all.
fn unusable_key(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        Some("is empty")
    } else if key.trim() != key {
        Some("starts or ends with whitespace")
    } else if key.chars().any(char::is_control) {
        Some("contains a control character")
    } else if key.chars().any(invisible) {
        Some("contains an invisible or formatting character")
    } else {
        None
    }
}

/// Whether `c` prints as nothing, or changes how the text around it
/// prints, rather than showing as a glyph of its own: a line or paragraph
/// separator (Zl, Zp), a format character (Cf, such as a zero-width space,
/// a joiner or a bidi override), or any other Default_Ignorable_Code_Point
/// (fillers, variation selectors, tags). Control characters (Cc) are
/// `char::is_control`'s, and not repeated here.
///
/// Written out rather than taken from a crate, because this is the one
/// place the server needs Unicode properties. The ranges are Unicode 15's
/// General_Category Cf, Zl and Zp, and DerivedCoreProperties'
/// Default_Ignorable_Code_Point, merged; a character added to those in a
/// later version is at worst shown unquoted, never acted on as a different
/// key, since keys are always compared exactly.
pub(crate) fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{0600}'..='\u{0605}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// A key to rename `key` to when it is being retired but kept, dated so a
/// second one on another day does not collide with the first.
pub(crate) fn archive_key(key: &str) -> String {
    format!("{key}.archived-{}", &crate::now()[..10])
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
        jobs: open_jobs(conn, from)?,
    })
}

fn remove_plan(conn: &Connection, key: &str) -> Result<Plan> {
    Ok(Plan::Remove {
        key: key.to_owned(),
        rows: rows_under(conn, key)?,
        jobs: open_jobs(conn, key)?,
    })
}

fn restore_plan(
    conn: &Connection,
    key: &str,
    backup: Vec<Row>,
    allow_overwrite: bool,
    allow_deletions: bool,
) -> Result<Restore> {
    let live = rows_under(conn, key)?;
    let live_by_path: HashMap<&str, &Row> =
        live.iter().map(|r| (r.file_path.as_str(), r)).collect();

    let mut add = Vec::new();
    let mut overwrite = Vec::new();
    let mut deletions = Vec::new();
    let mut unchanged = Vec::new();
    for row in &backup {
        match live_by_path.get(row.file_path.as_str()) {
            None => add.push(row.clone()),
            Some(current) if *current == row => unchanged.push(row.file_path.clone()),
            Some(current) if !current.deleted && row.deleted => {
                deletions.push(((*current).clone(), row.clone()))
            }
            Some(current) => overwrite.push(((*current).clone(), row.clone())),
        }
    }
    let live_only = live
        .iter()
        .filter(|r| !backup.iter().any(|b| b.file_path == r.file_path))
        .map(|r| r.file_path.clone())
        .collect();
    let mut restore = Restore {
        key: key.to_owned(),
        backup,
        add,
        overwrite,
        deletions,
        unchanged,
        live_only,
        allow_overwrite,
        allow_deletions,
        jobs: Vec::new(),
    };
    restore.jobs = open_jobs(conn, key)?
        .into_iter()
        .filter(|j| restore.written().any(|w| w.file_path == j.file_path))
        .collect();
    Ok(restore)
}

/// Every job still queued or leased under exactly `key`, by path, then
/// oldest first. None in a database with no jobs table: a dry run against
/// a live database no current server has opened yet.
fn open_jobs(conn: &Connection, key: &str) -> Result<Vec<OpenJob>> {
    if !has_table(conn, "jobs")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, file_path, state FROM jobs
         WHERE project_key = ?1 AND state IN ('queued', 'leased')
         ORDER BY file_path, created_at, id",
    )?;
    let jobs = stmt.query_map((key,), |r| {
        Ok(OpenJob {
            id: r.get(0)?,
            file_path: r.get(1)?,
            state: r.get(2)?,
        })
    })?;
    jobs.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        (name,),
        |r| r.get(0),
    )?)
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
            "the database stayed locked for {} seconds while {doing} (another process \
             mid-write: the server, or another admin command). Rolled back. Run the command \
             again",
            BUSY_TIMEOUT.as_secs()
        ),
        _ => anyhow::Error::from(err).context(format!("{doing} failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_leaf;

    const T0: &str = "2026-09-01T00:00:00.000Z";

    /// A leaf these tests do not look at, for [`Store::apply`].
    fn leaf(seq: u64, at: &str, _: &Applied) -> Vec<u8> {
        test_leaf(seq, at)
    }

    /// A stale push of `content` to `key`/`path`, queued for a worker as
    /// job `id`.
    fn queue(st: &Store, key: &str, path: &str, content: &str, id: &str) {
        let incoming = recall_wire::MergeSide {
            sha256: recall_wire::content_sha256(content),
            content: content.into(),
            source_env: "laptop".into(),
            updated_at: T0.into(),
        };
        let (queued, _) = st
            .write_and_queue_merge_audited(
                key,
                path,
                &incoming,
                id,
                time::OffsetDateTime::now_utc(),
                |seq, at, _| test_leaf(seq, at),
            )
            .unwrap();
        assert_eq!(queued, crate::store::Queued::Queued(id.into()));
    }

    /// The jobs a change closes are part of what was shown: one that
    /// appears after the plan stops the change like a row does, while a
    /// worker leasing one it showed changes nothing it does.
    #[test]
    fn a_job_that_arrives_after_the_plan_was_shown_stops_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let (path, st) = live(&dir);
        let writer = Store::open(&path).unwrap();
        queue(&writer, "old/key", "a.md", "alpha, edited", "job_1");
        let plan = st.plan_remove("old/key").unwrap();
        assert_eq!(plan.jobs().len(), 1);
        assert_eq!(plan.jobs()[0].state, "queued");

        // A job with no push behind it, so the rows are as they were shown;
        // with no input either, so not claimable before 2999.
        writer
            .with_raw(|c| {
                c.execute(
                    "INSERT INTO jobs (id, kind, state, project_key, file_path, payload,
                                       not_before, created_at, updated_at)
                     VALUES ('job_2', 'merge', 'queued', 'old/key', 'b.md', '{}', ?1, ?1, ?1)",
                    ("2999-01-01T00:00:00.000Z",),
                )
            })
            .unwrap();
        let err = st.apply(&plan, leaf).unwrap_err();
        assert!(err.is::<Abandoned>(), "{err:#}");
        assert!(err.to_string().contains("merge jobs"), "{err:#}");
        assert_eq!(st.rows("old/key").unwrap().len(), 3, "nothing removed");

        let plan = st.plan_remove("old/key").unwrap();
        writer
            .claim_job_audited(
                &["merge".into()],
                "lease_1",
                std::time::Duration::from_secs(60),
                time::OffsetDateTime::now_utc(),
                |seq, at, _| test_leaf(seq, at),
            )
            .unwrap()
            .unwrap();
        let applied = st.apply(&plan, leaf).unwrap();
        assert_eq!(applied.jobs, ["job_1", "job_2"]);
        let states: Vec<String> = writer
            .jobs(None, 10)
            .unwrap()
            .into_iter()
            .map(|j| j.state)
            .collect();
        assert_eq!(states, ["done", "done"]);
    }

    fn live(dir: &tempfile::TempDir) -> (std::path::PathBuf, Store) {
        let path = dir.path().join("recall.db");
        let st = Store::open(&path).unwrap();
        st.upsert_audited("old/key", "a.md", "alpha", "laptop", test_leaf)
            .unwrap();
        st.upsert_audited("old/key", "b.md", "beta", "", test_leaf)
            .unwrap();
        st.tombstone_audited("old/key", "c.md", "cloud", test_leaf)
            .unwrap();
        st.upsert_audited("other", "a.md", "other alpha", "laptop", test_leaf)
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
            .upsert_audited("old/key", "late.md", "arrived", "laptop", test_leaf)
            .unwrap();

        let err = st.apply(&plan, leaf).unwrap_err().to_string();
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

        let plan = st
            .plan_restore("old/key", backup.clone(), false, false)
            .unwrap();
        let Plan::Restore(r) = &plan else {
            unreachable!()
        };
        assert_eq!(r.overwrite.len(), 1);
        assert_eq!(r.unchanged.len(), 2);
        let err = st.apply(&plan, leaf).unwrap_err().to_string();
        assert!(err.contains("--overwrite"), "{err}");
        assert_eq!(st.rows("old/key").unwrap()[0].content, "alpha");

        let plan = st.plan_restore("old/key", backup, true, false).unwrap();
        assert_eq!(st.apply(&plan, leaf).unwrap().rows, 1);
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
        let Plan::Restore(r) = st.plan_restore("old/key", backup, false, false).unwrap() else {
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
            // Each prints as nothing, so the key would look like "me/thing"
            // in every listing while no machine could ever ask for it.
            ("me/\u{200B}thing", "invisible"),
            ("\u{FEFF}me/thing", "invisible"),
            ("me/thing\u{2060}", "invisible"),
            ("me/\u{202E}gniht", "invisible"),
            ("me/\u{2028}thing", "invisible"),
            ("me/\u{2029}thing", "invisible"),
            ("me/\u{00AD}thing", "invisible"),
            ("me/\u{3164}thing", "invisible"),
            ("me/thing\u{FE0F}", "invisible"),
            ("me/thing\u{E0041}", "invisible"),
        ] {
            let plan = Plan::Rename {
                from: "a".into(),
                to: to.into(),
                rows: vec![],
                occupied: vec![],
                jobs: vec![],
            };
            let refusal = plan.refusal().unwrap();
            assert!(refusal.contains(why), "{to:?}: {refusal}");
        }
        // Visible characters of any script, and a space inside, stay usable.
        for fine in ["me/thing", "local:-Users-me-my project", "我/项目", "é/ü"] {
            assert_eq!(unusable_key(fine), None, "{fine:?}");
        }
    }

    /// The failure that deletes its own backup has to be recognisable as
    /// such, and only it: the command decides on the type, not the words.
    #[test]
    fn a_plan_that_went_stale_is_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let (path, st) = live(&dir);
        let plan = st.plan_rename("old/key", "new/key").unwrap();
        Store::open(&path)
            .unwrap()
            .upsert_audited("old/key", "late.md", "arrived", "laptop", test_leaf)
            .unwrap();
        assert!(st.apply(&plan, leaf).unwrap_err().is::<Abandoned>());

        let plan = st.plan_rename("old/key", "other").unwrap();
        assert!(
            !st.apply(&plan, leaf).unwrap_err().is::<Abandoned>(),
            "a refusal"
        );
    }

    /// A backup's tombstone does not turn a live file into one, which would
    /// delete it on every machine, unless that is asked for by name.
    #[test]
    fn restore_turns_a_live_file_into_a_tombstone_only_when_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let (_, st) = live(&dir);
        let mut backup = st.rows("old/key").unwrap();
        backup[0].deleted = true; // a.md: live now, deleted in the backup
        backup[1].content = "older beta".into(); // b.md: an ordinary overwrite

        let plan = st
            .plan_restore("old/key", backup.clone(), true, false)
            .unwrap();
        let Plan::Restore(r) = &plan else {
            unreachable!()
        };
        assert_eq!(r.deletions.len(), 1);
        assert_eq!(r.deletions[0].0.file_path, "a.md");
        assert_eq!(r.skipped_deletions().len(), 1);
        assert_eq!(r.overwrite.len(), 1, "only b.md");
        assert_eq!(plan.expected_changes(), 1);
        assert_eq!(st.apply(&plan, leaf).unwrap().rows, 1);
        let rows = st.rows("old/key").unwrap();
        assert!(!rows[0].deleted, "a.md is still a file");
        assert_eq!(rows[1].content, "older beta");

        let plan = st.plan_restore("old/key", backup, true, true).unwrap();
        assert_eq!(plan.expected_changes(), 1);
        assert_eq!(st.apply(&plan, leaf).unwrap().rows, 1);
        assert!(st.rows("old/key").unwrap()[0].deleted, "now it is not");
    }

    #[test]
    fn unique_live_paths_are_the_content_found_under_no_other_key() {
        let dir = tempfile::tempdir().unwrap();
        let (_, st) = live(&dir);
        // Nothing of old/key is anywhere else. c.md is a tombstone, so it
        // is not live content to begin with.
        assert_eq!(st.unique_live_paths("old/key").unwrap(), ["a.md", "b.md"]);
        // The same content at another path, under another key, counts.
        Store::open(dir.path().join("recall.db"))
            .unwrap()
            .upsert_audited("copy", "moved/b.md", "beta", "laptop", test_leaf)
            .unwrap();
        assert_eq!(st.unique_live_paths("old/key").unwrap(), ["a.md"]);
        // A tombstone holding it does not.
        let other = Store::open(dir.path().join("recall.db")).unwrap();
        other
            .upsert_audited("gone", "a.md", "alpha", "laptop", test_leaf)
            .unwrap();
        other
            .tombstone_audited("gone", "a.md", "laptop", test_leaf)
            .unwrap();
        assert_eq!(st.unique_live_paths("old/key").unwrap(), ["a.md"]);
    }

    /// The backup is checked against the plan, not trusted because
    /// `VACUUM INTO` returned.
    #[test]
    fn a_snapshot_that_does_not_hold_the_plans_rows_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let (path, st) = live(&dir);
        let remove = st.plan_remove("old/key").unwrap();
        let rename = st.plan_rename("old/key", "new/key").unwrap();
        let mut backup = st.rows("old/key").unwrap();
        backup[1].content = "older beta".into();
        let restore = st.plan_restore("old/key", backup, true, false).unwrap();

        let good = Store::open_existing(&st.backup(dir.path().join("a"), 9).unwrap(), Access::Read)
            .unwrap();
        for plan in [&remove, &rename, &restore] {
            assert_eq!(plan.snapshot_mismatch(&good).unwrap(), None, "{plan:?}");
        }

        let writer = Store::open(&path).unwrap();
        writer
            .upsert_audited("old/key", "b.md", "edited", "x", test_leaf)
            .unwrap();
        let bad = Store::open_existing(&st.backup(dir.path().join("b"), 9).unwrap(), Access::Read)
            .unwrap();
        for plan in [&remove, &rename, &restore] {
            let why = plan.snapshot_mismatch(&bad).unwrap().unwrap();
            assert!(why.contains("\"old/key\""), "{why}");
        }
    }

    /// What a push in flight does after a change commits, done by hand
    /// here, is what `undone` reports.
    #[test]
    fn undone_names_what_came_back_after_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let (path, st) = live(&dir);
        let writer = Store::open(&path).unwrap();

        let rename = st.plan_rename("old/key", "new/key").unwrap();
        st.apply(&rename, leaf).unwrap();
        assert!(rename.undone(&st).unwrap().is_empty());
        writer
            .upsert_audited("old/key", "b.md", "late", "x", test_leaf)
            .unwrap();
        assert_eq!(rename.undone(&st).unwrap().paths, ["b.md"]);
        assert!(rename.undone(&st).unwrap().jobs.is_empty());
        // A stale push landing after it queues a job under the old key,
        // whose result would merge there: named as well as its path.
        queue(&writer, "old/key", "b.md", "later still", "job_late");
        let undone = rename.undone(&st).unwrap();
        assert_eq!(undone.paths, ["b.md"]);
        assert_eq!(
            undone
                .jobs
                .iter()
                .map(|j| j.id.as_str())
                .collect::<Vec<_>>(),
            ["job_late"]
        );

        let remove = st.plan_remove("new/key").unwrap();
        st.apply(&remove, leaf).unwrap();
        assert!(remove.undone(&st).unwrap().is_empty());
        writer
            .upsert_audited("new/key", "a.md", "late", "x", test_leaf)
            .unwrap();
        assert_eq!(remove.undone(&st).unwrap().paths, ["a.md"]);

        let mut backup = st.rows("other").unwrap();
        backup[0].content = "older".into();
        let restore = st.plan_restore("other", backup, true, false).unwrap();
        st.apply(&restore, leaf).unwrap();
        assert!(restore.undone(&st).unwrap().is_empty());
        writer
            .upsert_audited(
                "other",
                "a.md",
                "older, merged with an edit",
                "x",
                test_leaf,
            )
            .unwrap();
        assert_eq!(restore.undone(&st).unwrap().paths, ["a.md"]);
    }
}
