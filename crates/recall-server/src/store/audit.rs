//! The `audit_log` table: append-only storage for the Merkle tree in
//! [`crate::audit::merkle`], and the one transactional primitive
//! ([`Store::audited`]) every authenticated state change goes through so
//! its leaf commits with it.
//!
//! Every method on [`Store`] the server uses to change a file, a device or
//! an authkey takes the leaf it appends as an argument; none can change one
//! without it. The writes left without a leaf are deliberate, and none is a
//! change the server makes for a request or on its own: an enrolment
//! waiting for approval (anyone may ask, and unauthenticated routes append
//! nothing), a machine's poll for it, a device's `last_seen`, the sweep of
//! long-expired enrolments, and `recall-server admin`'s changes on the host
//! (`store/admin.rs`), which run beside the server on the database file.

use anyhow::{bail, Result};
use rusqlite::Connection;

use super::{Store, StoreState};
use crate::audit::merkle::{self, Hash, Tree};

/// Created alongside `memory_files` and the device tables, every time the
/// store opens.
///
/// `leaf` and `leaf_hash` are both stored: `leaf` is the exact bytes a
/// verifier hashes and exports, and `leaf_hash` is what the leaf hashed to
/// when it was written, which [`load`] checks it still does.
///
/// The triggers are what makes this append-only in the database itself,
/// not only by convention: even a bug — or a future migration reaching for
/// `UPDATE` out of habit — cannot rewrite, remove or skip a leaf. The
/// insert trigger is the one that stops `INSERT OR REPLACE`, whose
/// replacing delete does not fire a delete trigger, and an insert that
/// leaves a gap.
///
/// None of it stops someone holding the database file: they can drop a
/// trigger, or rewrite the table and every hash in it consistently. That
/// is what a checkpoint an owner saved elsewhere is for — a rewritten log
/// no longer extends it.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS audit_log (
        seq       INTEGER PRIMARY KEY,
        leaf      BLOB NOT NULL,
        leaf_hash BLOB NOT NULL
    );
    CREATE TRIGGER IF NOT EXISTS audit_log_no_update
        BEFORE UPDATE ON audit_log
    BEGIN
        SELECT RAISE(ABORT, 'audit_log is append-only');
    END;
    CREATE TRIGGER IF NOT EXISTS audit_log_no_delete
        BEFORE DELETE ON audit_log
    BEGIN
        SELECT RAISE(ABORT, 'audit_log is append-only');
    END;
    CREATE TRIGGER IF NOT EXISTS audit_log_next_seq
        BEFORE INSERT ON audit_log
        WHEN NEW.seq IS NOT (SELECT COALESCE(MAX(seq), -1) + 1 FROM audit_log)
    BEGIN
        SELECT RAISE(ABORT, 'audit_log is append-only: a leaf takes the next seq');
    END;
";

/// What [`load`] read back.
pub(super) struct Loaded {
    /// Every leaf's hash, as a tree.
    pub(super) tree: Tree,
    /// The newest leaf's `at`, or empty for an empty log.
    pub(super) last_at: String,
}

/// Reads every leaf back at open and rebuilds the tree from them, refusing
/// a log that is not what this server wrote: a `seq` missing or out of
/// place, or a leaf whose bytes no longer hash to its stored `leaf_hash`.
/// The triggers keep both from happening through SQL; this is for the
/// file changed some other way, by a disk or by hand.
///
/// The hashes are recomputed from the leaves rather than trusted, which
/// is most of what opening costs: a few seconds for a million leaves, a
/// gigabyte of them. A server that started anyway would sign every later
/// checkpoint over a tree it had already lost, so it does not start; the
/// error says to restore the database from a backup.
pub(super) fn load(conn: &Connection) -> Result<Loaded> {
    let mut stmt = conn.prepare("SELECT seq, leaf, leaf_hash FROM audit_log ORDER BY seq")?;
    let mut rows = stmt.query([])?;
    let mut tree = Tree::new();
    while let Some(row) = rows.next()? {
        let want = tree.size();
        let seq: i64 = row.get(0)?;
        if seq != want as i64 {
            bail!(
                "the audit log is damaged: leaf {want} is missing (the next one stored is {seq}); \
                 restore the database from a backup"
            );
        }
        let leaf = row.get_ref(1)?.as_blob()?;
        let hash = merkle::hash_leaf(leaf);
        if row.get_ref(2)?.as_blob()? != hash.as_slice() {
            bail!(
                "the audit log is damaged: leaf {seq} no longer hashes to its stored leaf_hash; \
                 restore the database from a backup"
            );
        }
        tree.append(hash);
    }
    let last_at = match tree.size() {
        0 => String::new(),
        n => {
            let leaf: Vec<u8> = conn.query_row(
                "SELECT leaf FROM audit_log WHERE seq = ?1",
                (n as i64 - 1,),
                |r| r.get(0),
            )?;
            // Every leaf `leaf::encode` wrote has one; one that somehow does
            // not only means the next `at` is not held to it.
            serde_json::from_slice::<serde_json::Value>(&leaf)
                .ok()
                .and_then(|v| v.get("at")?.as_str().map(str::to_string))
                .unwrap_or_default()
        }
    };
    Ok(Loaded { tree, last_at })
}

/// What [`Store::audited`]'s write closure hands back.
pub enum Outcome<T> {
    /// The write happened; its leaf is appended in the same transaction.
    Commit(T),
    /// Nothing happened — a refused request, such as an unknown code or one
    /// already decided. The transaction rolls back and no leaf is
    /// appended, matching "unauthenticated routes and refused requests
    /// append nothing" for the requests that do carry a credential but are
    /// refused for another reason.
    Refuse(T),
}

/// One stored leaf, as [`Store::audit_entries`] returns it: its position and
/// its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Its index in the tree, from 0.
    pub seq: u64,
    /// The leaf exactly as written — never re-serialized.
    pub leaf: Vec<u8>,
}

/// Why a consistency proof could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConsistencyError {
    /// `first` is 0, or greater than `second`.
    #[error("first must be at least 1 and at most second")]
    BadRange,
    /// `second` is past the tree's current size.
    #[error("second is past the end of the log")]
    SecondBeyondTreeSize,
}

impl StoreState {
    /// The `at` the next leaf carries: now, or the newest leaf's `at` if
    /// the clock has gone back since — so `at` never decreases along `seq`.
    /// Read under the store's lock, like the `seq` it goes with. Being one
    /// fixed-width format, two of these compare as strings.
    fn next_at(&self) -> String {
        let now = crate::now();
        if now < self.audit_at {
            self.audit_at.clone()
        } else {
            now
        }
    }
}

impl Store {
    /// Runs `write` inside one transaction and, only if it commits,
    /// appends the leaf `build_leaf` makes from the position that leaf
    /// will hold (its `seq`), its `at`, and `write`'s own result — then,
    /// and only then, the in-memory tree learns about it.
    ///
    /// `write` is given the same `at`, so a row it stamps and the leaf
    /// that records it carry one time. Both are taken under the lock the
    /// whole transaction holds, which is what makes `at` rise with `seq`.
    ///
    /// This and [`Store::audited_each`] are the only places a leaf is ever
    /// appended, so "every authenticated state change appends its leaf, in
    /// the same transaction as the change" is true by construction: nothing
    /// calls `INSERT INTO audit_log` any other way, and a write that never
    /// commits — because `write` returned [`Outcome::Refuse`] or an error —
    /// leaves neither the state change nor a leaf behind.
    pub fn audited<T>(
        &self,
        write: impl FnOnce(&rusqlite::Transaction, &str) -> Result<Outcome<T>>,
        build_leaf: impl FnOnce(u64, &str, &T) -> Vec<u8>,
    ) -> Result<T> {
        self.audited_each(write, |seq, at, value| vec![build_leaf(seq, at, value)])
    }

    /// [`Store::audited`], for a write that records any number of changes
    /// at once: `build_leaves` returns one leaf per change, the first
    /// taking `seq` and each after it the next, all appended in `write`'s
    /// transaction. The sweep is the one user: every device it removes has
    /// a leaf, and they commit together with the removals.
    pub fn audited_each<T>(
        &self,
        write: impl FnOnce(&rusqlite::Transaction, &str) -> Result<Outcome<T>>,
        build_leaves: impl FnOnce(u64, &str, &T) -> Vec<Vec<u8>>,
    ) -> Result<T> {
        let mut state = self.lock();
        // The position the first leaf will hold if this commits: the
        // tree's current size, read under the same lock the transaction
        // below holds for its whole duration, so no other request can
        // observe or claim this seq first.
        let seq = state.audit.size();
        let at = state.next_at();
        let tx = state.conn.transaction()?;
        let value = match write(&tx, &at)? {
            Outcome::Commit(value) => value,
            Outcome::Refuse(value) => return Ok(value), // `tx` drops here: rolled back.
        };
        let leaves = build_leaves(seq, &at, &value);
        let mut hashes = Vec::with_capacity(leaves.len());
        for (i, leaf) in leaves.iter().enumerate() {
            let leaf_hash = merkle::hash_leaf(leaf);
            tx.execute(
                "INSERT INTO audit_log (seq, leaf, leaf_hash) VALUES (?1, ?2, ?3)",
                (seq as i64 + i as i64, leaf, leaf_hash.as_slice()),
            )?;
            hashes.push(leaf_hash);
        }
        tx.commit()?;
        for leaf_hash in hashes {
            state.audit.append(leaf_hash);
        }
        if !leaves.is_empty() {
            state.audit_at = at;
        }
        Ok(value)
    }

    /// Appends a leaf with no other state to change: a pull, the server's
    /// own `start`. Still one transaction (of one statement), so it shares
    /// [`Store::audited`]'s all-or-nothing behaviour rather than being a
    /// special case.
    ///
    /// Returns the leaf's `seq`, read out of the same closure `audited`
    /// calls under its lock — not by asking the tree its size again
    /// afterwards, which another append could have moved on by then.
    pub fn audit_append(&self, build_leaf: impl FnOnce(u64, &str) -> Vec<u8>) -> Result<u64> {
        let assigned = std::cell::Cell::new(0u64);
        self.audited(
            |_tx, _at| Ok(Outcome::Commit(())),
            |seq, at, ()| {
                assigned.set(seq);
                build_leaf(seq, at)
            },
        )?;
        Ok(assigned.get())
    }

    /// The tree's size and root, for `GET /v1/audit/checkpoint` and the
    /// `Recall-Audit-Checkpoint` header every pull carries.
    pub fn audit_checkpoint(&self) -> (u64, Hash) {
        let state = self.lock();
        (state.audit.size(), state.audit.root())
    }

    /// Leaves `start` to `end - 1`, stopping early once they come to more
    /// than `max_bytes` together — though never before the first, however
    /// large, so a caller paging on from the last `seq` it got always
    /// moves. The caller (the route handler) is responsible for
    /// `end <= tree_size` and the 1,000-row page limit.
    pub fn audit_entries(&self, start: u64, end: u64, max_bytes: usize) -> Result<Vec<AuditEntry>> {
        let state = self.lock();
        let mut stmt = state
            .conn
            .prepare("SELECT seq, leaf FROM audit_log WHERE seq >= ?1 AND seq < ?2 ORDER BY seq")?;
        let mut rows = stmt.query((start as i64, end as i64))?;
        let (mut out, mut bytes) = (Vec::new(), 0usize);
        while let Some(row) = rows.next()? {
            let leaf: Vec<u8> = row.get(1)?;
            bytes += leaf.len();
            if bytes > max_bytes && !out.is_empty() {
                break;
            }
            out.push(AuditEntry {
                seq: row.get::<_, i64>(0)? as u64,
                leaf,
            });
        }
        Ok(out)
    }

    /// The RFC 9162 §2.1.4 proof that the tree at `second` extends the one
    /// at `first`, from the tree in memory: a few hundred hashes at most,
    /// and no read of the database, however long the log.
    pub fn audit_consistency(
        &self,
        first: u64,
        second: u64,
    ) -> Result<Result<Vec<Hash>, ConsistencyError>> {
        let state = self.lock();
        if first == 0 || first > second {
            return Ok(Err(ConsistencyError::BadRange));
        }
        if second > state.audit.size() {
            return Ok(Err(ConsistencyError::SecondBeyondTreeSize));
        }
        Ok(Ok(state.audit.consistency(first, second)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::leaf;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn push_leaf(seq: u64) -> Vec<u8> {
        leaf::encode(
            seq,
            "2026-01-01T00:00:00.000Z",
            leaf::action::PUSH,
            &leaf::Actor::Operator,
            leaf::subject_file(&leaf::FileChange {
                project_key: "acme/app",
                file_path: "a.md",
                deleted: false,
                stored_sha256: "abc",
                base_sha256: None,
                merged: false,
            }),
            None,
        )
    }

    fn append(st: &Store) {
        st.audited(
            |_tx, _| Ok(Outcome::Commit(())),
            |seq, _, ()| push_leaf(seq),
        )
        .unwrap();
    }

    const INSERT_FILE: &str = "INSERT INTO memory_files \
         (project_key, file_path, content, source_env, updated_at) VALUES ('a','b','c','d','e')";

    #[test]
    fn a_committed_write_appends_exactly_one_leaf() {
        let st = store();
        st.audited(
            |tx, _| {
                tx.execute(INSERT_FILE, [])?;
                Ok(Outcome::Commit(()))
            },
            |seq, _, ()| push_leaf(seq),
        )
        .unwrap();
        let (size, _) = st.audit_checkpoint();
        assert_eq!(size, 1);
        assert_eq!(st.audit_entries(0, 1, usize::MAX).unwrap().len(), 1);
    }

    /// The atomicity the mutation table asks for: a write that returns an
    /// error after already changing something rolls the whole transaction
    /// back, leaf included.
    #[test]
    fn a_failing_write_appends_no_leaf_and_keeps_no_change() {
        let st = store();
        let result = st.audited(
            |tx, _| {
                tx.execute(INSERT_FILE, [])?;
                Err(anyhow::Error::from(rusqlite::Error::ExecuteReturnedResults))
            },
            |seq, _, ()| push_leaf(seq),
        );
        assert!(result.is_err());
        assert_eq!(
            st.audit_checkpoint().0,
            0,
            "no leaf from a rolled-back write"
        );
        assert!(st.get("a", "b").unwrap().is_none(), "no row either");
    }

    /// And the other way round: a leaf that cannot be written takes the
    /// change down with it. The leaf goes in inside the change's own
    /// transaction, before it commits, so there is never a moment when the
    /// change is stored and its leaf is not.
    #[test]
    fn a_leaf_that_cannot_be_written_undoes_the_change() {
        let st = store();
        st.with_raw(|c| {
            c.execute_batch(
                "CREATE TEMP TRIGGER no_leaves BEFORE INSERT ON audit_log
                 BEGIN SELECT RAISE(ABORT, 'no leaves today'); END;",
            )
        })
        .unwrap();
        let result = st.audited(
            |tx, _| {
                tx.execute(INSERT_FILE, [])?;
                Ok(Outcome::Commit(()))
            },
            |seq, _, ()| push_leaf(seq),
        );
        assert!(result.is_err(), "the leaf's insert failed");
        assert!(
            st.get("a", "b").unwrap().is_none(),
            "so the row is not there"
        );
        assert_eq!(st.audit_checkpoint().0, 0);
    }

    /// A refused request — nothing wrong at the database level, just a
    /// decision not to proceed — is the same all-or-nothing story: no leaf.
    #[test]
    fn a_refused_write_appends_no_leaf() {
        let st = store();
        let refusal: &str = st
            .audited(
                |_tx, _| Ok(Outcome::Refuse("no such code")),
                |seq, _, _| push_leaf(seq),
            )
            .unwrap();
        assert_eq!(refusal, "no such code");
        assert_eq!(st.audit_checkpoint().0, 0);
    }

    /// Several leaves from one write take consecutive seqs, and commit or
    /// roll back together.
    #[test]
    fn one_write_can_append_several_leaves_in_order() {
        let st = store();
        append(&st);
        st.audited_each(
            |_tx, _| Ok(Outcome::Commit(3u64)),
            |seq, _, n| (seq..seq + n).map(push_leaf).collect(),
        )
        .unwrap();
        let seqs: Vec<u64> = st
            .audit_entries(0, 4, usize::MAX)
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, vec![0, 1, 2, 3]);
        assert_eq!(
            st.audit_entries(1, 2, usize::MAX).unwrap()[0].leaf,
            push_leaf(1)
        );
    }

    /// `at` is taken under the lock, and never goes back: a leaf written
    /// after the clock has moved backwards carries the newest `at` already
    /// in the log, not an earlier one.
    #[test]
    fn at_never_decreases_along_seq() {
        let st = store();
        let mut ats = Vec::new();
        for _ in 0..3 {
            st.audit_append(|seq, at| {
                ats.push(at.to_string());
                push_leaf(seq)
            })
            .unwrap();
        }
        assert!(ats.windows(2).all(|w| w[0] <= w[1]), "{ats:?}");
        assert_eq!(ats[0].len(), 24, "the API's timestamp format");

        // A newest leaf from the future: the clock went back after it.
        st.lock().audit_at = "2999-01-01T00:00:00.000Z".into();
        let mut got = String::new();
        st.audit_append(|seq, at| {
            got = at.to_string();
            push_leaf(seq)
        })
        .unwrap();
        assert_eq!(got, "2999-01-01T00:00:00.000Z");
    }

    #[test]
    fn checkpoint_matches_the_merkle_root_of_every_leaf() {
        let st = store();
        for _ in 0..5 {
            append(&st);
        }
        let (size, root) = st.audit_checkpoint();
        assert_eq!(size, 5);
        let leaves: Vec<Hash> = (0..5)
            .map(push_leaf)
            .map(|l| merkle::hash_leaf(&l))
            .collect();
        assert_eq!(root, merkle::root(&leaves));
    }

    #[test]
    fn entries_pages_by_seq_and_by_bytes() {
        let st = store();
        for _ in 0..3 {
            append(&st);
        }
        let got = st.audit_entries(1, 3, usize::MAX).unwrap();
        assert_eq!(got.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(got[0].leaf, push_leaf(1));

        let one = push_leaf(0).len();
        let seqs = |max| -> Vec<u64> {
            st.audit_entries(0, 3, max)
                .unwrap()
                .iter()
                .map(|e| e.seq)
                .collect()
        };
        assert_eq!(seqs(2 * one), vec![0, 1], "two fit exactly");
        assert_eq!(seqs(2 * one - 1), vec![0], "the second would overflow");
        assert_eq!(seqs(1), vec![0], "never fewer than one");
    }

    #[test]
    fn consistency_matches_merkle_and_rejects_bad_ranges() {
        let st = store();
        for _ in 0..8 {
            append(&st);
        }
        let proof = st.audit_consistency(3, 8).unwrap().unwrap();
        let leaves: Vec<Hash> = (0..8)
            .map(push_leaf)
            .map(|l| merkle::hash_leaf(&l))
            .collect();
        assert_eq!(proof, merkle::consistency(3, 8, &leaves));

        assert_eq!(
            st.audit_consistency(0, 8).unwrap(),
            Err(ConsistencyError::BadRange)
        );
        assert_eq!(
            st.audit_consistency(5, 3).unwrap(),
            Err(ConsistencyError::BadRange)
        );
        assert_eq!(
            st.audit_consistency(1, 100).unwrap(),
            Err(ConsistencyError::SecondBeyondTreeSize)
        );
    }

    /// A proof comes from the tree in memory: the table can be out of reach
    /// entirely and it is still served, which is what keeps the store's one
    /// lock from being held over a read of every leaf.
    #[test]
    fn a_proof_does_not_read_the_table() {
        let st = store();
        for _ in 0..40 {
            append(&st);
        }
        let want = st.audit_consistency(7, 40).unwrap().unwrap();
        st.with_raw(|c| c.execute_batch("ALTER TABLE audit_log RENAME TO audit_log_away"))
            .unwrap();
        assert_eq!(st.audit_consistency(7, 40).unwrap().unwrap(), want);
    }

    /// The triggers, not just application discipline: a raw `UPDATE`,
    /// `DELETE`, upsert, `INSERT OR REPLACE` over a leaf, or an insert that
    /// skips a seq, aborts.
    #[test]
    fn nothing_but_the_next_leaf_can_be_written() {
        let st = store();
        append(&st);
        append(&st);

        let refused = |sql: &str| {
            assert!(st.with_raw(|c| c.execute(sql, [])).is_err(), "{sql}");
        };
        refused("UPDATE audit_log SET seq = 99 WHERE seq = 0");
        refused("DELETE FROM audit_log WHERE seq = 0");
        refused(
            "INSERT OR REPLACE INTO audit_log (seq, leaf, leaf_hash) \
             VALUES (0, CAST('forged' AS BLOB), zeroblob(32))",
        );
        refused(
            "INSERT INTO audit_log (seq, leaf, leaf_hash) VALUES (0, x'01', zeroblob(32)) \
             ON CONFLICT(seq) DO UPDATE SET leaf = excluded.leaf",
        );
        refused("INSERT INTO audit_log (seq, leaf, leaf_hash) VALUES (100, x'02', zeroblob(32))");
        refused("INSERT INTO audit_log (leaf, leaf_hash) VALUES (x'02', zeroblob(32))");
        assert_eq!(
            st.audit_entries(0, 2, usize::MAX).unwrap()[0].leaf,
            push_leaf(0),
            "leaf 0 is what was written"
        );
        assert_eq!(st.audit_checkpoint().0, 2);

        // The next seq is still accepted — the trigger stops only the rest.
        st.with_raw(|c| {
            c.execute(
                "INSERT INTO audit_log (seq, leaf, leaf_hash) VALUES (2, x'03', zeroblob(32))",
                [],
            )
        })
        .unwrap();
    }

    /// The tree is rebuilt at open from what the file holds, so a reopened
    /// store answers with the same checkpoint and goes on from the same seq.
    #[test]
    fn reopening_rebuilds_the_same_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.db");
        let before = {
            let st = Store::open(&path).unwrap();
            for _ in 0..5 {
                append(&st);
            }
            st.audit_checkpoint()
        };
        let st = Store::open(&path).unwrap();
        assert_eq!(st.audit_checkpoint(), before);
        assert_eq!(st.audit_append(|seq, _| push_leaf(seq)).unwrap(), 5);
    }

    /// Opening refuses a log changed behind the triggers' back: a leaf
    /// rewritten without its hash, one rewritten with it but the hash row
    /// left, or a gap.
    #[test]
    fn opening_refuses_a_damaged_log() {
        let damaged = |how: &str| -> String {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("r.db");
            {
                let st = Store::open(&path).unwrap();
                for _ in 0..4 {
                    append(&st);
                }
            }
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "DROP TRIGGER audit_log_no_update; DROP TRIGGER audit_log_no_delete; {how}"
            ))
            .unwrap();
            drop(conn);
            match Store::open(&path) {
                Ok(_) => panic!("opened a log damaged by {how}"),
                Err(e) => format!("{e:#}"),
            }
        };
        let e = damaged("UPDATE audit_log SET leaf = CAST('forged' AS BLOB) WHERE seq = 1");
        assert!(e.contains("leaf 1 no longer hashes"), "{e}");
        let e = damaged("DELETE FROM audit_log WHERE seq = 2");
        assert!(e.contains("leaf 2 is missing"), "{e}");
        let e = damaged("UPDATE audit_log SET leaf_hash = zeroblob(32) WHERE seq = 3");
        assert!(e.contains("leaf 3 no longer hashes"), "{e}");
    }
}
