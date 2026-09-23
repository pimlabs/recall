//! The `audit_log` table: append-only storage for the Merkle tree in
//! [`crate::audit::merkle`], and the one transactional primitive
//! ([`Store::audited`]) every authenticated state change goes through so
//! its leaf commits with it.

use anyhow::Result;
use rusqlite::Connection;

use super::Store;
use crate::audit::merkle::{self, Hash};

/// Created alongside `memory_files` and the device tables, every time the
/// store opens.
///
/// `leaf` and `leaf_hash` are both stored: `leaf` is the exact bytes a
/// verifier hashes and exports, `leaf_hash` is kept beside it so the tree
/// can be rebuilt at start (and a consistency proof served) without
/// re-hashing every leaf on every boot.
///
/// The triggers are what makes this append-only in the database itself,
/// not only by convention: even a bug — or a future migration reaching for
/// `UPDATE` out of habit — cannot rewrite or remove a leaf.
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
";

/// Every `leaf_hash`, in `seq` order — what [`crate::audit::merkle::Frontier::rebuild`]
/// needs to bring the in-memory tree back after a restart.
pub(super) fn leaf_hashes(conn: &Connection) -> Result<Vec<Hash>> {
    let mut stmt = conn.prepare("SELECT leaf_hash FROM audit_log ORDER BY seq")?;
    let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let bytes = row?;
        let hash: Hash = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("a stored leaf_hash is not 32 bytes"))?;
        out.push(hash);
    }
    Ok(out)
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

impl Store {
    /// Runs `write` inside one transaction and, only if it commits,
    /// appends the leaf `build_leaf` makes from the position that leaf
    /// will hold (its `seq`) and `write`'s own result — then, and only
    /// then, the in-memory tree learns about it.
    ///
    /// This is the one place a leaf is ever appended, so "every
    /// authenticated state change appends exactly one leaf, in the same
    /// transaction as the change" is true by construction: nothing calls
    /// `INSERT INTO audit_log` any other way, and a write that never
    /// commits — because `write` returned [`Outcome::Refuse`] or an error —
    /// leaves neither the state change nor a leaf behind.
    pub fn audited<T>(
        &self,
        write: impl FnOnce(&rusqlite::Transaction) -> rusqlite::Result<Outcome<T>>,
        build_leaf: impl FnOnce(u64, &T) -> Vec<u8>,
    ) -> Result<T> {
        let mut state = self.lock();
        // The position this leaf will hold if it commits: the tree's
        // current size, read under the same lock the transaction below
        // holds for its whole duration, so no other request can observe
        // or claim this seq first.
        let seq = state.audit.size();
        let tx = state.conn.transaction()?;
        Ok(match write(&tx)? {
            Outcome::Commit(value) => {
                let leaf = build_leaf(seq, &value);
                let leaf_hash = merkle::hash_leaf(&leaf);
                tx.execute(
                    "INSERT INTO audit_log (seq, leaf, leaf_hash) VALUES (?1, ?2, ?3)",
                    (seq as i64, &leaf, leaf_hash.as_slice()),
                )?;
                tx.commit()?;
                state.audit.append(leaf_hash);
                value
            }
            Outcome::Refuse(value) => value, // `tx` drops here: rolled back.
        })
    }

    /// Appends a leaf with no other state to change: a pull, a sweep, the
    /// server's own `start`. Still one transaction (of one statement), so
    /// it shares [`Store::audited`]'s all-or-nothing behaviour rather than
    /// being a special case.
    ///
    /// Returns the leaf's `seq`, read out of the same closure `audited`
    /// calls under its lock — not by asking the tree its size again
    /// afterwards, which another append could have moved on by then.
    pub fn audit_append(&self, build_leaf: impl FnOnce(u64) -> Vec<u8>) -> Result<u64> {
        let assigned = std::cell::Cell::new(0u64);
        self.audited(
            |_tx| Ok(Outcome::Commit(())),
            |seq, ()| {
                assigned.set(seq);
                build_leaf(seq)
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

    /// Leaves `start` to `end - 1`. The caller (the route handler) is
    /// responsible for `end <= tree_size` and the 1,000-row page limit;
    /// this returns whatever is there for the range given.
    pub fn audit_entries(&self, start: u64, end: u64) -> Result<Vec<AuditEntry>> {
        let state = self.lock();
        let mut stmt = state
            .conn
            .prepare("SELECT seq, leaf FROM audit_log WHERE seq >= ?1 AND seq < ?2 ORDER BY seq")?;
        let rows = stmt.query_map((start as i64, end as i64), |r| {
            Ok(AuditEntry {
                seq: r.get::<_, i64>(0)? as u64,
                leaf: r.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The RFC 9162 §2.1.4 proof that the tree at `second` extends the one
    /// at `first`.
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
        let hashes = leaf_hashes(&state.conn)?;
        Ok(Ok(merkle::consistency(first, second, &hashes)))
    }

    /// Every leaf, in order — what `recall audit export` and the
    /// integration tests that exercise `scripts/audit-verify.py` read.
    pub fn audit_export(&self) -> Result<Vec<Vec<u8>>> {
        let state = self.lock();
        let mut stmt = state.conn.prepare("SELECT leaf FROM audit_log ORDER BY seq")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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
            leaf::subject_file("acme/app", "a.md", false, "abc", None),
            None,
        )
    }

    #[test]
    fn a_committed_write_appends_exactly_one_leaf() {
        let st = store();
        st.audited(
            |tx| {
                tx.execute(
                    "INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at) VALUES ('a','b','c','d','e')",
                    [],
                )?;
                Ok(Outcome::Commit(()))
            },
            |seq, ()| push_leaf(seq),
        )
        .unwrap();
        let (size, _) = st.audit_checkpoint();
        assert_eq!(size, 1);
        assert_eq!(st.audit_entries(0, 1).unwrap().len(), 1);
    }

    /// The atomicity the mutation table asks for: a write that returns an
    /// error after already changing something rolls the whole transaction
    /// back, leaf included.
    #[test]
    fn a_failing_write_appends_no_leaf_and_keeps_no_change() {
        let st = store();
        let result = st.audited(
            |tx| {
                tx.execute(
                    "INSERT INTO memory_files (project_key, file_path, content, source_env, updated_at) VALUES ('a','b','c','d','e')",
                    [],
                )?;
                Err(rusqlite::Error::ExecuteReturnedResults)
            },
            |seq, ()| push_leaf(seq),
        );
        assert!(result.is_err());
        assert_eq!(st.audit_checkpoint().0, 0, "no leaf from a rolled-back write");
        assert!(st.get("a", "b").unwrap().is_none(), "no row either");
    }

    /// A refused request — nothing wrong at the database level, just a
    /// decision not to proceed — is the same all-or-nothing story: no leaf.
    #[test]
    fn a_refused_write_appends_no_leaf() {
        let st = store();
        let refusal: &str = st
            .audited(
                |_tx| Ok(Outcome::Refuse("no such code")),
                |seq, _| push_leaf(seq),
            )
            .unwrap();
        assert_eq!(refusal, "no such code");
        assert_eq!(st.audit_checkpoint().0, 0);
    }

    #[test]
    fn checkpoint_matches_the_merkle_root_of_every_leaf() {
        let st = store();
        for i in 0..5u64 {
            st.audited(|_tx| Ok(Outcome::Commit(())), |seq, ()| push_leaf(seq))
                .unwrap();
            let _ = i;
        }
        let (size, root) = st.audit_checkpoint();
        assert_eq!(size, 5);
        let leaves: Vec<Hash> = (0..5).map(push_leaf).map(|l| merkle::hash_leaf(&l)).collect();
        assert_eq!(root, merkle::root(&leaves));
    }

    #[test]
    fn entries_pages_by_seq() {
        let st = store();
        for i in 0..3u64 {
            st.audited(|_tx| Ok(Outcome::Commit(())), move |seq, ()| push_leaf(seq))
                .unwrap();
            let _ = i;
        }
        let got = st.audit_entries(1, 3).unwrap();
        assert_eq!(got.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(got[0].leaf, push_leaf(1));
    }

    #[test]
    fn consistency_matches_merkle_and_rejects_bad_ranges() {
        let st = store();
        for i in 0..8u64 {
            st.audited(|_tx| Ok(Outcome::Commit(())), move |seq, ()| push_leaf(seq))
                .unwrap();
            let _ = i;
        }
        let proof = st.audit_consistency(3, 8).unwrap().unwrap();
        let leaves: Vec<Hash> = (0..8).map(push_leaf).map(|l| merkle::hash_leaf(&l)).collect();
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

    /// The triggers, not just application discipline: a raw `UPDATE` or
    /// `DELETE` against `audit_log` aborts.
    #[test]
    fn update_and_delete_on_audit_log_abort() {
        let st = store();
        st.audited(|_tx| Ok(Outcome::Commit(())), |seq, ()| push_leaf(seq))
            .unwrap();

        let update = st.with_raw(|c| c.execute("UPDATE audit_log SET seq = 99 WHERE seq = 0", []));
        assert!(update.is_err(), "UPDATE must abort");
        let delete = st.with_raw(|c| c.execute("DELETE FROM audit_log WHERE seq = 0", []));
        assert!(delete.is_err(), "DELETE must abort");
        assert_eq!(st.audit_checkpoint().0, 1, "the row must still be there");
    }
}
