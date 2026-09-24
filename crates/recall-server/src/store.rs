//! SQLite persistence.
//!
//! The schema here is deliberately identical to the one the Node server
//! created, because this server opens the *existing production database
//! file* rather than migrating to a new one. That is what makes the cutover
//! reversible: roll back by starting the old container against the same
//! untouched file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use recall_wire::{AdminTotals, File, ProjectStats};
use rusqlite::{Connection, OptionalExtension};

use crate::audit::merkle::Tree;
use crate::now;

mod audit;
mod devices;
mod jobs;
mod passkeys;

pub use audit::{AuditEntry, ConsistencyError, Outcome};
pub use devices::{
    plain_name, Created, Decision, Inserted, NewAuthkey, NewDevice, NewEnrollment, Poll, Waiting,
};
pub use jobs::{
    clip, Failure, Queued, Retried, Settled, Settlement, MAX_ATTEMPTS, MAX_ERROR_BYTES, MAX_LINKS,
    MAX_OPEN_JOBS,
};
pub use passkeys::{
    AddedCredential, AdminCredential, AdminSession, BootstrapCode, FirstPasskey,
    NewAdminCredential, RemovedCredential,
};

/// Frozen: an already-deployed database was created with exactly this.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS memory_files (
        project_key TEXT NOT NULL,
        file_path   TEXT NOT NULL,
        content     TEXT NOT NULL,
        source_env  TEXT,
        updated_at  TEXT NOT NULL,
        deleted     INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (project_key, file_path)
    );
";

/// The stored state of one file, used to decide whether a push needs
/// merging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Existing {
    /// The stored bytes. Present even for a tombstone — the server keeps the
    /// last known content, it just refuses to hand it back over the wire.
    pub content: String,
    /// Whether the row is a tombstone.
    pub deleted: bool,
    /// The machine that wrote it; empty when none was recorded.
    pub source_env: String,
    /// When it was written.
    pub updated_at: String,
}

/// The connection, plus the in-memory state built from it: the audit log's
/// [`Tree`], rebuilt at open from `audit_log`, so an append, a checkpoint
/// and a consistency proof each cost a few hashes rather than a pass over
/// the whole table; and the newest leaf's `at`, which the next may not go
/// below.
///
/// [`std::ops::Deref`] and [`std::ops::DerefMut`] to [`Connection`] mean
/// every existing call site — `conn.execute(...)`, `conn.transaction()` —
/// keeps compiling unchanged; only the audit-specific code reaches `audit`
/// directly.
struct StoreState {
    conn: Connection,
    audit: Tree,
    audit_at: String,
}

impl std::ops::Deref for StoreState {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl std::ops::DerefMut for StoreState {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

/// The SQLite database, and every query the server makes against it.
pub struct Store {
    // A single connection behind a mutex. This is a single-owner server
    // against a local file; a pool would buy nothing and SQLite would
    // serialize the writes anyway. The audit log's append-then-commit
    // relies on this too: every write already goes through this one lock,
    // so a leaf and the state change it records are never interleaved with
    // another request's.
    state: Mutex<StoreState>,
}

impl Store {
    /// Opens (creating if needed) the database at `path`, switching it to
    /// SQLite's WAL journal with every commit synced before it returns (see
    /// `use_durable_wal` in this module for why both). A file an older
    /// server wrote with the rollback journal is converted here, once; the
    /// mode is stored in the file. Fails, rather than serving in another
    /// mode, if the switch cannot be made: the file is held by another
    /// process for longer than the busy timeout, or SQLite cannot keep the
    /// WAL's index beside it. A network filesystem may well not fail here
    /// and still not work; `deploy/README.md` says not to use one.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        use_durable_wal(&conn).with_context(|| {
            format!(
                "switching {} to SQLite's WAL journal. It needs a local filesystem, and a \
                 moment with no other process holding the file (sqlite-web mid-read, an admin \
                 command): start the server again",
                path.display()
            )
        })?;
        Self::with_connection(conn)
    }

    /// An in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    fn with_connection(conn: Connection) -> Result<Self> {
        let store = Self {
            state: Mutex::new(StoreState {
                conn,
                audit: Tree::new(),
                audit_at: String::new(),
            }),
        };
        store.migrate()?;
        Ok(store)
    }

    // A panic in one request must not render the whole store unusable, and
    // nothing here leaves the database in a half-written state, so a
    // poisoned mutex is recovered rather than propagated.
    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn migrate(&self) -> Result<()> {
        let mut state = self.lock();
        state.conn.execute_batch(SCHEMA)?;

        // Databases created before tombstones existed have no `deleted`
        // column. Adding it is safe and idempotent when guarded like this.
        let has_deleted = {
            let mut stmt = state.conn.prepare("PRAGMA table_info(memory_files)")?;
            let mut rows = stmt.query([])?;
            let mut found = false;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == "deleted" {
                    found = true;
                }
            }
            found
        };
        if !has_deleted {
            state.conn.execute(
                "ALTER TABLE memory_files ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }

        // Tables of their own, beside memory_files rather than in it, so a
        // server from before devices existed still opens this file and
        // simply never looks at them: rolling back stays a matter of
        // starting the older image. Same for audit_log.
        state.conn.execute_batch(devices::SCHEMA)?;
        state.conn.execute_batch(passkeys::SCHEMA)?;
        // The one change to an existing table the merge queue needs:
        // devices may now have the worker scope, which SQLite can only
        // allow by rebuilding the table. Then the queue's own table, beside
        // the others for the same reason they are.
        devices::allow_worker_scope(&state.conn)?;
        state.conn.execute_batch(jobs::SCHEMA)?;
        state.conn.execute_batch(audit::SCHEMA)?;

        let loaded = audit::load(&state.conn)?;
        state.audit = loaded.tree;
        state.audit_at = loaded.last_at;
        Ok(())
    }

    /// Reads one row.
    pub fn get(&self, project_key: &str, file_path: &str) -> Result<Option<Existing>> {
        read_file(&self.lock(), project_key, file_path)
    }

    /// Writes content, clearing any tombstone, and appends the leaf
    /// `build_leaf` makes from its `seq` and `at` in the same transaction.
    /// Answers the time it was written, which is that `at`: the row's
    /// `updated_at` and its leaf's `at` are one timestamp.
    pub fn upsert_audited(
        &self,
        project_key: &str,
        file_path: &str,
        content: &str,
        source_env: &str,
        build_leaf: impl FnOnce(u64, &str) -> Vec<u8>,
    ) -> Result<String> {
        self.audited(
            |tx, at| {
                write_file(tx, project_key, file_path, content, source_env, at)?;
                Ok(Outcome::Commit(at.to_string()))
            },
            |seq, at, _| build_leaf(seq, at),
        )
    }

    /// Marks a file deleted while deliberately leaving its content in
    /// place: a mistaken delete stays recoverable at the database level,
    /// even though nothing in the app surfaces an undo yet. [`Store::list`]
    /// withholds the content so a pull can't resurrect it.
    ///
    /// In the same transaction it closes the file's open merge jobs, so
    /// nothing merges the deleted notes back into a file pushed after it,
    /// and appends its leaf, answering the time, as
    /// [`Store::upsert_audited`] does.
    pub fn tombstone_audited(
        &self,
        project_key: &str,
        file_path: &str,
        source_env: &str,
        build_leaf: impl FnOnce(u64, &str) -> Vec<u8>,
    ) -> Result<String> {
        self.audited(
            |tx, at| {
                tx.execute(
                    "INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
                     VALUES (?1, ?2, '', ?3, ?4, 1)
                     ON CONFLICT(project_key, file_path) DO UPDATE SET
                         source_env = excluded.source_env,
                         updated_at = excluded.updated_at,
                         deleted = 1",
                    (project_key, file_path, nullable(source_env), at),
                )?;
                jobs::close_for_delete(tx, project_key, file_path, at)?;
                Ok(Outcome::Commit(at.to_string()))
            },
            |seq, at, _| build_leaf(seq, at),
        )
    }

    /// Every file for a project, tombstones included so a pulling client
    /// knows what to remove locally — but with the deleted content
    /// withheld.
    pub fn list(&self, project_key: &str) -> Result<Vec<File>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT file_path, content, COALESCE(source_env, ''), updated_at, deleted
             FROM memory_files WHERE project_key = ?1 ORDER BY file_path",
        )?;
        let rows = stmt.query_map((project_key,), |r| {
            let content: String = r.get(1)?;
            let deleted = r.get::<_, i64>(4)? != 0;
            Ok(File {
                file_path: r.get(0)?,
                content: if deleted { None } else { Some(content) },
                source_env: r.get(2)?,
                updated_at: r.get(3)?,
                deleted,
            })
        })?;
        let mut files = Vec::new();
        for row in rows {
            files.push(row?);
        }
        Ok(files)
    }

    /// The most recent write across all projects, for `/health`. Empty when
    /// nothing has ever been synced.
    pub fn last_sync_at(&self) -> Result<String> {
        let conn = self.lock();
        let v: Option<String> =
            conn.query_row("SELECT MAX(updated_at) FROM memory_files", [], |r| r.get(0))?;
        Ok(v.unwrap_or_default())
    }

    /// Aggregates per project for the admin page.
    pub fn admin_stats(&self) -> Result<(Vec<ProjectStats>, AdminTotals)> {
        let conn = self.lock();

        let mut projects = Vec::new();
        let mut totals = AdminTotals::default();
        {
            let mut stmt = conn.prepare(
                "SELECT project_key,
                        SUM(CASE WHEN deleted = 0 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN deleted = 1 THEN 1 ELSE 0 END),
                        MAX(updated_at)
                 FROM memory_files GROUP BY project_key ORDER BY MAX(updated_at) DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(ProjectStats {
                    project_key: r.get(0)?,
                    file_count: r.get(1)?,
                    deleted_count: r.get(2)?,
                    sources: Vec::new(),
                    last_updated_at: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                })
            })?;
            for row in rows {
                let p = row?;
                totals.file_count += p.file_count;
                totals.deleted_count += p.deleted_count;
                projects.push(p);
            }
        }
        totals.project_count = projects.len() as i64;

        // Sources are collected separately rather than with GROUP_CONCAT:
        // SQLite won't take a custom separator together with DISTINCT, and
        // source_env is client-supplied, so a value containing a comma
        // would silently split into bogus entries.
        {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT project_key, source_env FROM memory_files WHERE source_env IS NOT NULL",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (key, src) = row?;
                if src.is_empty() {
                    continue;
                }
                if let Some(p) = projects.iter_mut().find(|p| p.project_key == key) {
                    p.sources.push(src);
                }
            }
        }
        for p in &mut projects {
            p.sources.sort();
        }
        Ok((projects, totals))
    }

    /// Copies the commits the WAL holds into the database file, as far as no
    /// reader still needs them, waiting on nothing.
    ///
    /// SQLite already does this by itself once the WAL passes 1000 pages,
    /// which on a personal server can be days of pushes. The server also
    /// does it every sweep, so that `recall.db` on its own, all a reader
    /// that cannot see `recall.db-wal` has (sqlite-web's single-file mount
    /// in `deploy/docker-compose.direct.yml`), is never far behind. Nothing
    /// relies on it for correctness: the WAL is part of the database.
    pub fn checkpoint(&self) -> Result<()> {
        self.lock()
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
        Ok(())
    }

    /// Copies every commit the WAL holds into the database file and empties
    /// the WAL, waiting up to the busy timeout for a reader in the way.
    /// Answers whether it got all the way.
    ///
    /// The last thing a server does as it stops. Closing the connection
    /// does the same, but only when no other process has the file open, and
    /// sqlite-web keeps it open. Emptied here, the WAL left beside the file
    /// holds nothing, so a stopped server's `recall.db` is the whole
    /// database. Not something to rely on after a crash, which is why every
    /// procedure in `deploy/README.md` moves or copies the WAL with the file.
    pub fn checkpoint_all(&self) -> Result<bool> {
        let busy: i64 = self
            .lock()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        Ok(busy == 0)
    }

    /// Writes a consistent snapshot via `VACUUM INTO`, then prunes the
    /// oldest snapshots beyond `keep`.
    ///
    /// `VACUUM INTO` reads through this connection, so the snapshot holds
    /// every commit, those still only in the WAL included, as of one
    /// moment, whatever is writing meanwhile. Copying `recall.db` instead
    /// would miss whatever the WAL has not yet handed back to it. And what
    /// it writes is one self-contained file in the rollback journal's mode,
    /// with no `-wal` of its own, so a snapshot can be copied, uploaded,
    /// opened read-only, or copied over `recall.db` in a restore, as it is.
    pub fn backup(&self, dir: impl AsRef<Path>, keep: usize) -> Result<PathBuf> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        // Mirrors the Node server's naming, which was
        // toISOString().replace(/[:.]/g, "-"). Millisecond precision
        // matters twice over: without it two snapshots in the same second
        // collide (and VACUUM INTO refuses to overwrite an existing file),
        // and the prune below sorts these names lexicographically alongside
        // any snapshots the Node server already wrote into the directory.
        let stamp = now().replace([':', '.'], "-");
        let dest = dir.join(format!("recall-{stamp}.db"));
        let dest_str = dest
            .to_str()
            .context("backup path is not valid UTF-8")?
            .to_owned();

        // VACUUM INTO refuses a file that is already there, so one that is
        // there now is not this call's to delete if the vacuum fails.
        let existed = dest.exists();
        let vacuumed = {
            let conn = self.lock();
            conn.execute("VACUUM INTO ?1", (&dest_str,))
                .with_context(|| format!("VACUUM INTO {dest_str}"))
        };
        if let Err(err) = vacuumed {
            // A vacuum that fails part way (a full disk, a file size limit)
            // leaves the part it wrote. Left there, it is named like every
            // good snapshot, sorts among them, and counts toward `keep`,
            // so the prune would delete a good one to make room for it,
            // and anyone restoring would find a file that does not open.
            if !existed {
                let _ = fs::remove_file(&dest);
            }
            return Err(err);
        }

        let mut snapshots: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("recall-") && n.ends_with(".db"))
            })
            .collect();
        snapshots.sort();
        for stale in snapshots.iter().take(snapshots.len().saturating_sub(keep)) {
            let _ = fs::remove_file(stale);
        }
        Ok(dest)
    }
}

#[cfg(test)]
impl Store {
    /// Test-only: runs `f` against the raw connection. Used to assert things
    /// no public method goes anywhere near on purpose, such as the audit
    /// log's append-only triggers refusing a raw `UPDATE` or `DELETE`.
    pub(crate) fn with_raw<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        f(&self.lock())
    }
}

/// Test-only: a leaf for the store's own tests, which are about rows rather
/// than what a leaf says. The store has no way to write without one.
#[cfg(test)]
pub(crate) fn test_leaf(seq: u64, at: &str) -> Vec<u8> {
    use crate::audit::leaf;
    leaf::encode(
        seq,
        at,
        leaf::action::START,
        &leaf::Actor::Server,
        leaf::subject_start("test"),
        None,
    )
}

/// Puts the server's connection in WAL mode with `synchronous=FULL`, and
/// states its busy timeout.
///
/// WAL for two reasons. A commit appends its pages to `recall.db-wal` and
/// syncs that one file, where the rollback journal wrote and synced a
/// journal, then the database, then deleted the journal; since the audit
/// log, a pull is a write transaction too, so every sync request pays for
/// one. And a reader no longer holds up a writer: sqlite-web mid-read, or
/// an admin command reading its backup, used to stall a push's commit for
/// up to the busy timeout and fail it past that. Readers see the last
/// commit before they began, and the one writer at a time is unchanged:
/// every write here still goes through the store's lock, and an admin
/// command's through `BEGIN IMMEDIATE`, so the audit log's tree and its
/// triggers see exactly what they did.
///
/// FULL rather than NORMAL, WAL's usual partner. Under NORMAL a commit is
/// not synced until the next checkpoint, so a power cut or a crash of the
/// host (not of this process, whose writes the kernel already has) can
/// take back the last pushes after their client was told 200, and that
/// client may be a cloud session that no longer exists, whose memory this
/// server was the only copy of. FULL syncs the WAL at every commit. The
/// ignored `push_latency_by_journal_mode` test measures what that costs.
/// On the VM this was written on (a release build, through the router,
/// medians), a push took 1.5 ms under the rollback journal, 0.12 ms under
/// WAL with NORMAL and 0.58 ms under WAL with FULL, and a pull 1.2, 0.07
/// and 0.29 ms. The one sync is the difference between the last two, and
/// grows with a slower disk, as the rollback journal's several did; a 200
/// that means stored is worth it.
///
/// Both settings are the connection's, and the journal mode is also
/// stored in the file, so every other connection (an admin command,
/// sqlite-web, a `sqlite3` shell, an older recall-server) opens it in WAL
/// too; an admin command states FULL for itself. What WAL asks of anything
/// that copies or replaces the file is in `deploy/README.md`: `recall.db`
/// alone is not the whole database while `recall.db-wal` holds commits not
/// yet copied back into it, and a WAL left beside a replaced `recall.db`
/// is replayed into it.
fn use_durable_wal(conn: &Connection) -> Result<()> {
    conn.busy_timeout(admin::BUSY_TIMEOUT)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        anyhow::bail!("SQLite kept the {mode} journal");
    }
    conn.execute_batch("PRAGMA synchronous = FULL")?;
    Ok(())
}

/// What [`existing_from`] reads, in its order.
const EXISTING_COLUMNS: &str = "content, deleted, COALESCE(source_env, ''), updated_at";

fn existing_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Existing> {
    Ok(Existing {
        content: r.get(0)?,
        deleted: r.get::<_, i64>(1)? != 0,
        source_env: r.get(2)?,
        updated_at: r.get(3)?,
    })
}

/// One row, on a connection or transaction the caller already holds.
fn read_file(conn: &Connection, project_key: &str, file_path: &str) -> Result<Option<Existing>> {
    Ok(conn
        .query_row(
            &format!("SELECT {EXISTING_COLUMNS} FROM memory_files WHERE project_key = ?1 AND file_path = ?2"),
            (project_key, file_path),
            existing_from,
        )
        .optional()?)
}

/// Writes content, clearing any tombstone, on a connection or transaction
/// the caller already holds: the one statement behind
/// [`Store::upsert_audited`] and a merged result being applied, each in a
/// transaction that appends its leaf.
fn write_file(
    conn: &Connection,
    project_key: &str,
    file_path: &str,
    content: &str,
    source_env: &str,
    updated_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
         VALUES (?1, ?2, ?3, ?4, ?5, 0)
         ON CONFLICT(project_key, file_path) DO UPDATE SET
             content = excluded.content,
             source_env = excluded.source_env,
             updated_at = excluded.updated_at,
             deleted = 0",
        (project_key, file_path, content, nullable(source_env), updated_at),
    )?;
    Ok(())
}

/// An absent `source_env` is stored as NULL, not `''` — `admin_stats`
/// distinguishes the two.
fn nullable(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// What `recall-server admin` does to a database. A child module so it can
// share the one connection type and `Store::backup` without widening what
// `Store` exposes; crate-private, because nothing but that subcommand, and
// nothing reachable from the HTTP router, may call it.
pub(crate) mod admin;

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn put(st: &Store, project_key: &str, file_path: &str, content: &str, source_env: &str) {
        st.upsert_audited(project_key, file_path, content, source_env, test_leaf)
            .unwrap();
    }

    fn del(st: &Store, project_key: &str, file_path: &str, source_env: &str) {
        st.tombstone_audited(project_key, file_path, source_env, test_leaf)
            .unwrap();
    }

    /// The row's `updated_at` is its leaf's `at`: one moment, taken under
    /// the lock the write holds, answered to the caller.
    #[test]
    fn a_write_is_stamped_with_its_leafs_at() {
        let st = store();
        let mut leaf_at = String::new();
        let updated_at = st
            .upsert_audited("acme/app", "a.md", "x", "laptop", |seq, at| {
                leaf_at = at.to_string();
                test_leaf(seq, at)
            })
            .unwrap();
        assert_eq!(updated_at, leaf_at);
        assert_eq!(st.list("acme/app").unwrap()[0].updated_at, updated_at);
        assert_eq!(st.audit_checkpoint().0, 1);
    }

    #[test]
    fn upsert_get_and_list_round_trip() {
        let st = store();
        put(&st, "acme/app", "MEMORY.md", "hello", "laptop");

        let got = st.get("acme/app", "MEMORY.md").unwrap().unwrap();
        assert_eq!(got.content, "hello");
        assert!(!got.deleted);

        let files = st.list("acme/app").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content.as_deref(), Some("hello"));
        assert_eq!(files[0].source_env, "laptop");
        assert!(st.get("acme/app", "missing.md").unwrap().is_none());
    }

    /// Both halves of the tombstone contract in one place: the row keeps
    /// its content, the listing does not hand it back.
    #[test]
    fn tombstone_preserves_content_but_list_withholds_it() {
        let st = store();
        put(&st, "acme/app", "gone.md", "secret", "laptop");
        del(&st, "acme/app", "gone.md", "laptop");

        let row = st.get("acme/app", "gone.md").unwrap().unwrap();
        assert_eq!(row.content, "secret", "content must stay recoverable");
        assert!(row.deleted);

        let files = st.list("acme/app").unwrap();
        assert_eq!(
            files.len(),
            1,
            "tombstones are listed so clients can delete locally"
        );
        assert!(files[0].deleted);
        assert_eq!(files[0].content, None, "a pull must not resurrect it");
    }

    /// A push after a delete revives the row and clears the tombstone.
    #[test]
    fn upsert_clears_a_tombstone() {
        let st = store();
        del(&st, "acme/app", "f.md", "laptop");
        put(&st, "acme/app", "f.md", "back", "laptop");
        let row = st.get("acme/app", "f.md").unwrap().unwrap();
        assert!(!row.deleted);
        assert_eq!(row.content, "back");
    }

    #[test]
    fn last_sync_at_is_empty_on_a_fresh_database() {
        assert_eq!(store().last_sync_at().unwrap(), "");
    }

    /// A `source_env` containing a comma must survive as one value — the
    /// reason sources aren't gathered with GROUP_CONCAT.
    #[test]
    fn admin_stats_keeps_commas_inside_a_source_env() {
        let st = store();
        put(&st, "acme/app", "a.md", "x", "laptop,evil");
        let (projects, _) = st.admin_stats().unwrap();
        assert_eq!(projects[0].sources, vec!["laptop,evil".to_string()]);
    }

    /// The `deleted` column is added to databases that predate tombstones,
    /// without touching their rows.
    #[test]
    fn migrates_a_database_that_predates_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE memory_files (
                    project_key TEXT NOT NULL,
                    file_path   TEXT NOT NULL,
                    content     TEXT NOT NULL,
                    source_env  TEXT,
                    updated_at  TEXT NOT NULL,
                    PRIMARY KEY (project_key, file_path)
                );
                INSERT INTO memory_files VALUES ('acme/app','old.md','kept','node-era','2026-09-03T21:49:55.191Z');",
            )
            .unwrap();
        }
        let st = Store::open(&path).unwrap();
        let files = st.list("acme/app").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content.as_deref(), Some("kept"));
        assert!(!files[0].deleted);
    }

    #[test]
    fn backup_names_carry_milliseconds() {
        let dir = tempfile::tempdir().unwrap();
        let st = store();
        let dest = st.backup(dir.path(), 7).unwrap();
        let name = dest.file_name().unwrap().to_str().unwrap();
        // recall-2026-09-03T21-49-55-191Z.db
        assert!(
            name.starts_with("recall-") && name.ends_with("Z.db"),
            "got {name}"
        );
        let stamp = &name["recall-".len()..name.len() - ".db".len()];
        assert_eq!(stamp.len(), 24, "got {stamp}");
        // The three characters before the Z are the milliseconds, which
        // keep two snapshots in the same second from colliding.
        assert!(
            stamp[20..23].chars().all(|c| c.is_ascii_digit()),
            "no millisecond field in {stamp}"
        );
    }

    // ------------------------------------------------------------ journal

    fn journal_mode(conn: &Connection) -> String {
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    }

    /// Bytes 18 and 19 of the header: 1 for the rollback journal, 2 for
    /// WAL. What SQLite, any version, reads the mode from.
    fn header_mode(path: &Path) -> (u8, u8) {
        let head = fs::read(path).unwrap();
        (head[18], head[19])
    }

    fn wal_len(db: &Path) -> u64 {
        let mut wal = db.as_os_str().to_owned();
        wal.push("-wal");
        fs::metadata(wal).map(|m| m.len()).unwrap_or(0)
    }

    /// The server's connection is in WAL, syncs every commit (FULL, not
    /// NORMAL: see `use_durable_wal`) and waits the admin commands' busy
    /// timeout; and the mode is the file's, so any other connection opens
    /// it in WAL too.
    #[test]
    fn the_store_keeps_the_file_in_wal_and_syncs_every_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        let st = Store::open(&path).unwrap();
        put(&st, "acme/app", "a.md", "x", "laptop");
        let (mode, sync, busy) = st
            .with_raw(|c| {
                Ok((
                    journal_mode(c),
                    c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))?,
                    c.query_row("PRAGMA busy_timeout", [], |r| r.get::<_, i64>(0))?,
                ))
            })
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(sync, 2, "synchronous=FULL");
        assert_eq!(busy, admin::BUSY_TIMEOUT.as_millis() as i64);

        assert_eq!(header_mode(&path), (2, 2));
        assert_eq!(journal_mode(&Connection::open(&path).unwrap()), "wal");
        assert!(wal_len(&path) > 0, "the commit went to the WAL");
    }

    /// The upgrade: a file a server before WAL wrote, rows, audit log and
    /// all, in the rollback journal it always had. The first open converts
    /// it, loses nothing, and the log reads back to the same tree.
    #[test]
    fn a_rollback_journal_database_from_an_older_server_is_converted_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        let before = {
            let st = Store::open(&path).unwrap();
            put(&st, "acme/app", "MEMORY.md", "kept\n", "laptop");
            del(&st, "acme/app", "gone.md", "laptop");
            st.audit_checkpoint()
        };
        // As 0.4.2 left it: the rollback journal, which is what a server
        // that never asked for WAL got.
        Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
            .unwrap();
        assert_eq!(header_mode(&path), (1, 1));

        let st = Store::open(&path).unwrap();
        assert_eq!(header_mode(&path), (2, 2));
        let files = st.list("acme/app").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].content.as_deref(), Some("kept\n"));
        assert!(files[1].deleted);
        assert_eq!(st.audit_checkpoint(), before);
        put(&st, "acme/app", "after.md", "y", "laptop");
        drop(st);

        // And back: a connection that asks for nothing, as an older server
        // rolled back to, reads the converted file, every commit included.
        let plain = Connection::open(&path).unwrap();
        let n: i64 = plain
            .query_row("SELECT count(*) FROM memory_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// The upgrade's one way to fail: another process reading the old
    /// file (sqlite-web, say) through the whole busy timeout, which under
    /// the rollback journal keeps the switch from committing. The server
    /// refuses to start, saying why and what to do, rather than serving in
    /// the old mode; the file is left exactly as it was, and the next
    /// start, with the reader gone, converts it.
    #[test]
    fn a_switch_held_up_past_the_busy_timeout_refuses_to_start_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
            .unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM memory_files", [], |r| r.get(0))
            .unwrap();

        let err = match Store::open(&path) {
            Ok(_) => panic!("switched to WAL under a reader holding the file"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("switching"), "{err}");
        assert!(err.contains("start the server again"), "{err}");
        assert!(err.contains("locked"), "{err}");
        assert_eq!(header_mode(&path), (1, 1), "still the rollback journal");

        reader.execute_batch("COMMIT").unwrap();
        drop(reader);
        drop(Store::open(&path).unwrap());
        assert_eq!(header_mode(&path), (2, 2));
    }

    /// The same, from the oldest file there is: the one the Node server
    /// wrote, which production's rows came from (`scripts/compat-check.sh`
    /// drives a release binary against it). Every row reads back the same
    /// after the switch.
    #[test]
    fn the_node_servers_database_is_converted_with_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/node-written.db"
            ),
            &path,
        )
        .unwrap();
        assert_eq!(header_mode(&path), (1, 1));
        let dump = || -> Vec<(String, String, String, Option<String>, String, i64)> {
            let conn = Connection::open(&path).unwrap();
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
        };
        let before = dump();
        assert!(!before.is_empty());

        let st = Store::open(&path).unwrap();
        assert_eq!(header_mode(&path), (2, 2));
        assert_eq!(dump(), before);
        drop(st);
        assert_eq!(dump(), before);
    }

    /// What a restore copies over `recall.db`: one file, in the rollback
    /// journal's mode, with no WAL of its own, holding the commits the live
    /// file does not have yet because they are still only in its WAL.
    #[test]
    fn a_snapshot_is_one_self_contained_file_with_what_the_wal_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        let st = Store::open(&path).unwrap();
        for i in 0..20 {
            put(&st, "acme/app", &format!("f{i}.md"), "x", "laptop");
        }
        assert!(wal_len(&path) > 0, "the pushes are still in the WAL");

        let snap = st.backup(dir.path().join("backups"), 7).unwrap();
        let names: Vec<_> = fs::read_dir(snap.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "no -wal or -shm beside it: {names:?}");
        assert_eq!(header_mode(&snap), (1, 1));

        let read =
            Connection::open_with_flags(&snap, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(journal_mode(&read), "delete");
        let n: i64 = read
            .query_row("SELECT count(*) FROM memory_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 20);
    }

    /// `recall.db` on its own, as a reader that cannot see the WAL has it
    /// (sqlite-web's single-file mount; here, a hard link in another
    /// directory): behind while the WAL holds commits, caught up by a
    /// checkpoint, and whole after the one a server takes as it stops,
    /// which also empties the WAL.
    #[test]
    fn a_checkpoint_brings_the_file_on_its_own_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recall.db");
        let st = Store::open(&path).unwrap();
        put(&st, "acme/app", "a.md", "x", "laptop");
        let alone = dir.path().join("alone");
        fs::create_dir(&alone).unwrap();
        fs::hard_link(&path, alone.join("recall.db")).unwrap();
        let count = || -> i64 {
            Connection::open_with_flags(
                alone.join("recall.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .and_then(|c| c.query_row("SELECT count(*) FROM memory_files", [], |r| r.get(0)))
            .unwrap_or(-1)
        };

        let behind = count();
        assert!(
            behind < 1,
            "the file alone lacks the WAL's commit: {behind}"
        );
        st.checkpoint().unwrap();
        assert_eq!(count(), 1);

        put(&st, "acme/app", "b.md", "x", "laptop");
        assert!(st.checkpoint_all().unwrap());
        assert_eq!(wal_len(&path), 0, "emptied");
        assert_eq!(count(), 2);
    }

    /// Push latency under the rollback journal, WAL with NORMAL and WAL
    /// with FULL, through the real router against a file in a temporary
    /// directory. The numbers quoted beside `use_durable_wal` come from
    /// here:
    ///
    /// ```text
    /// cargo test -p recall-server --release --lib -- --ignored --nocapture push_latency
    /// ```
    #[tokio::test]
    #[ignore = "a measurement, not a check: run it by hand"]
    async fn push_latency_by_journal_mode() {
        use axum::body::Body;
        use axum::http::Request;
        use std::time::{Duration, Instant};
        use tower::ServiceExt;

        const PUSHES: usize = 400;
        const TOKEN: &str = "bench-token";
        let content = "- a remembered fact, about as long as one usually is\n".repeat(20);
        println!("| journal | push p50 | push p90 | push mean | pull p50 | pull p90 |");
        println!("|---|---|---|---|---|---|");
        for (label, journal, sync) in [
            ("rollback (DELETE), FULL", "DELETE", "FULL"),
            ("WAL, NORMAL", "WAL", "NORMAL"),
            ("WAL, FULL", "WAL", "FULL"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = std::sync::Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
            store
                .with_raw(|c| {
                    c.query_row(&format!("PRAGMA journal_mode = {journal}"), [], |_| Ok(()))?;
                    c.execute_batch(&format!("PRAGMA synchronous = {sync}"))
                })
                .unwrap();
            let server = crate::Server::new(
                crate::Config {
                    token: TOKEN.into(),
                    merge_enabled: false,
                    rate_limit_max: 1_000_000,
                    ..crate::Config::default()
                },
                store,
            );
            let router = server.router();
            let push = |i: usize| {
                let body = serde_json::json!({
                    "project_key": "bench/app",
                    "file_path": format!("f{i}.md"),
                    "content": content,
                    "source_env": "bench",
                });
                Request::post("/sync")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap()
            };
            let pull = || {
                Request::get("/sync?project_key=small/app")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap()
            };
            let time = |mut samples: Vec<Duration>| {
                samples.sort();
                let ms = |d: Duration| d.as_secs_f64() * 1000.0;
                let mean = ms(samples.iter().sum::<Duration>()) / samples.len() as f64;
                (
                    ms(samples[samples.len() / 2]),
                    ms(samples[samples.len() * 9 / 10]),
                    mean,
                )
            };
            for i in 0..20 {
                let resp = router.clone().oneshot(push(PUSHES + i)).await.unwrap();
                assert_eq!(resp.status(), 200);
            }
            let mut pushes = Vec::with_capacity(PUSHES);
            for i in 0..PUSHES {
                let started = Instant::now();
                let resp = router.clone().oneshot(push(i)).await.unwrap();
                pushes.push(started.elapsed());
                assert_eq!(resp.status(), 200);
            }
            let mut pulls = Vec::with_capacity(PUSHES);
            for _ in 0..PUSHES {
                let started = Instant::now();
                let resp = router.clone().oneshot(pull()).await.unwrap();
                pulls.push(started.elapsed());
                assert_eq!(resp.status(), 200);
            }
            let (p50, p90, mean) = time(pushes);
            let (l50, l90, _) = time(pulls);
            println!(
                "| {label} | {p50:.2} ms | {p90:.2} ms | {mean:.2} ms | {l50:.2} ms | {l90:.2} ms |"
            );
        }
    }
}
