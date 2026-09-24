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

pub use audit::{AuditEntry, ConsistencyError, Outcome};
pub use devices::{
    plain_name, Created, Decision, Inserted, NewAuthkey, NewDevice, NewEnrollment, Poll, Waiting,
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
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
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
        // starting the older image. Same for audit_log, added here.
        state.conn.execute_batch(devices::SCHEMA)?;
        state.conn.execute_batch(audit::SCHEMA)?;

        let loaded = audit::load(&state.conn)?;
        state.audit = loaded.tree;
        state.audit_at = loaded.last_at;
        Ok(())
    }

    /// Reads one row.
    pub fn get(&self, project_key: &str, file_path: &str) -> Result<Option<Existing>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT content, deleted FROM memory_files WHERE project_key = ?1 AND file_path = ?2",
                (project_key, file_path),
                |r| {
                    Ok(Existing {
                        content: r.get(0)?,
                        deleted: r.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(row)
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
                tx.execute(
                    "INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at, deleted)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)
                     ON CONFLICT(project_key, file_path) DO UPDATE SET
                         content = excluded.content,
                         source_env = excluded.source_env,
                         updated_at = excluded.updated_at,
                         deleted = 0",
                    (project_key, file_path, content, nullable(source_env), at),
                )?;
                Ok(Outcome::Commit(at.to_string()))
            },
            |seq, at, _| build_leaf(seq, at),
        )
    }

    /// Marks a file deleted while deliberately leaving its content in
    /// place: a mistaken delete stays recoverable at the database level,
    /// even though nothing in the app surfaces an undo yet. [`Store::list`]
    /// withholds the content so a pull can't resurrect it. The leaf, and
    /// the time answered, as [`Store::upsert_audited`].
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

    /// Writes a consistent snapshot via `VACUUM INTO`, which is safe
    /// against a live database — unlike copying the file — then prunes the
    /// oldest snapshots beyond `keep`.
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
}
