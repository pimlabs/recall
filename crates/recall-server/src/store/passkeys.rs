//! The owner's passkeys and the admin page's sessions.
//!
//! A passkey is stored as `webauthn-rs` serialises it, in `passkey`, and
//! that JSON is opaque to everything here: the store never needs to look
//! inside it, so it does not depend on the crate that wrote it. What the
//! store does need to compare, the credential id, the user handle and the
//! signature counter, has a column of its own.
//!
//! Timestamps are [`crate::now`]'s format, so they compare as strings.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row};

use super::Store;

/// Created alongside the other tables, every time the store opens.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS admin_credentials (
        -- The WebAuthn credential id, base64url without padding.
        id           TEXT PRIMARY KEY,
        -- The WebAuthn user handle, a UUID. Every passkey has the same one:
        -- there is one owner, and an authenticator given the same handle
        -- twice for a site replaces its passkey rather than adding one.
        user_handle  TEXT NOT NULL,
        -- What the owner called it, such as 'iPhone'.
        name         TEXT NOT NULL,
        -- webauthn-rs's Passkey, as JSON: the public key and its flags.
        passkey      TEXT NOT NULL,
        -- The authenticator's signature counter at the last sign-in. Kept
        -- beside the JSON so that checking it and moving it forward is one
        -- conditional UPDATE, which two sign-ins at once cannot both pass.
        sign_count   INTEGER NOT NULL DEFAULT 0,
        created_at   TEXT NOT NULL,
        last_used_at TEXT
    );
    CREATE TABLE IF NOT EXISTS admin_sessions (
        -- SHA-256 of the cookie's value, lowercase hex. The value itself is
        -- never stored, so a copy of the database signs nobody in.
        token_sha256  TEXT PRIMARY KEY,
        -- The passkey that signed in: removing it ends its sessions.
        credential_id TEXT NOT NULL,
        created_at    TEXT NOT NULL,
        last_used_at  TEXT NOT NULL,
        -- The absolute limit, however recently it was used.
        expires_at    TEXT NOT NULL
    );
";

/// One of the owner's passkeys, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminCredential {
    /// The credential id, base64url.
    pub id: String,
    /// The WebAuthn user handle.
    pub user_handle: String,
    /// What the owner called it.
    pub name: String,
    /// webauthn-rs's `Passkey`, as JSON.
    pub passkey: String,
    /// The signature counter at the last sign-in.
    pub sign_count: u32,
    /// When it was registered.
    pub created_at: String,
    /// When it last signed in, if ever.
    pub last_used_at: Option<String>,
}

/// A passkey about to be stored.
#[derive(Debug, Clone)]
pub struct NewAdminCredential<'a> {
    /// The credential id, base64url.
    pub id: &'a str,
    /// The WebAuthn user handle.
    pub user_handle: &'a str,
    /// What the owner called it.
    pub name: &'a str,
    /// webauthn-rs's `Passkey`, as JSON.
    pub passkey: &'a str,
    /// Its signature counter as registered.
    pub sign_count: u32,
    /// Now.
    pub created_at: &'a str,
}

/// What storing a passkey came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddedCredential {
    /// Stored.
    Added,
    /// Only the first passkey may be stored this way, and there is one
    /// already.
    NotFirst,
    /// A passkey with that credential id is stored already.
    Duplicate,
}

/// What removing a passkey came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovedCredential {
    /// Removed, with every session it signed in.
    Removed(AdminCredential),
    /// It is the only one; removing it would lock the owner out.
    Last,
    /// No passkey has that id.
    NotFound,
}

/// An admin session, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminSession {
    /// The passkey that signed in.
    pub credential_id: String,
    /// When it signed in.
    pub created_at: String,
    /// When it was last used.
    pub last_used_at: String,
    /// When it ends, however recently it was used.
    pub expires_at: String,
}

const CREDENTIAL_COLUMNS: &str =
    "id, user_handle, name, passkey, sign_count, created_at, last_used_at";

fn credential_from(r: &Row<'_>) -> rusqlite::Result<AdminCredential> {
    Ok(AdminCredential {
        id: r.get(0)?,
        user_handle: r.get(1)?,
        name: r.get(2)?,
        passkey: r.get(3)?,
        sign_count: r.get(4)?,
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
    })
}

fn get_credential(conn: &Connection, id: &str) -> Result<Option<AdminCredential>> {
    Ok(conn
        .query_row(
            &format!("SELECT {CREDENTIAL_COLUMNS} FROM admin_credentials WHERE id = ?1"),
            (id,),
            credential_from,
        )
        .optional()?)
}

impl Store {
    /// Whether the owner has registered any passkey.
    pub fn has_admin_credentials(&self) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM admin_credentials", [], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Stores a passkey. With `first_only`, only while none is stored:
    /// checked in the same statement that inserts, so two bootstraps at
    /// once cannot both be the first.
    pub fn add_admin_credential(
        &self,
        c: &NewAdminCredential<'_>,
        first_only: bool,
    ) -> Result<AddedCredential> {
        let conn = self.lock();
        if get_credential(&conn, c.id)?.is_some() {
            return Ok(AddedCredential::Duplicate);
        }
        let guard = if first_only {
            "WHERE NOT EXISTS (SELECT 1 FROM admin_credentials)"
        } else {
            ""
        };
        let inserted = conn.execute(
            &format!(
                "INSERT INTO admin_credentials
                     (id, user_handle, name, passkey, sign_count, created_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6 {guard}"
            ),
            (
                c.id,
                c.user_handle,
                c.name,
                c.passkey,
                c.sign_count,
                c.created_at,
            ),
        )?;
        Ok(if inserted == 1 {
            AddedCredential::Added
        } else {
            AddedCredential::NotFirst
        })
    }

    /// One passkey.
    pub fn admin_credential(&self, id: &str) -> Result<Option<AdminCredential>> {
        get_credential(&self.lock(), id)
    }

    /// Every passkey, oldest first.
    pub fn admin_credentials(&self) -> Result<Vec<AdminCredential>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM admin_credentials ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map([], credential_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records a sign-in, if its signature counter moved forward.
    ///
    /// WebAuthn §7.2 step 22: when either the stored counter or the new one
    /// is nonzero, the new one must be greater, or two copies of the
    /// credential's key may exist. Both zero is what a synced passkey
    /// reports every time, and is accepted. The check and the update are one
    /// statement, so two sign-ins racing with the same counter cannot both
    /// pass. Answers whether it was recorded; `false` is a refusal.
    pub fn record_admin_sign_in(
        &self,
        id: &str,
        sign_count: u32,
        passkey: &str,
        now: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        let updated = conn.execute(
            "UPDATE admin_credentials
             SET sign_count = ?2, passkey = ?3, last_used_at = ?4
             WHERE id = ?1 AND (sign_count < ?2 OR (sign_count = 0 AND ?2 = 0))",
            (id, sign_count, passkey, now),
        )?;
        Ok(updated == 1)
    }

    /// Removes a passkey and every session it signed in, unless it is the
    /// last one.
    pub fn remove_admin_credential(&self, id: &str) -> Result<RemovedCredential> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let Some(credential) = get_credential(&tx, id)? else {
            return Ok(RemovedCredential::NotFound);
        };
        let n: i64 = tx.query_row("SELECT COUNT(*) FROM admin_credentials", [], |r| r.get(0))?;
        if n <= 1 {
            return Ok(RemovedCredential::Last);
        }
        tx.execute("DELETE FROM admin_credentials WHERE id = ?1", (id,))?;
        tx.execute("DELETE FROM admin_sessions WHERE credential_id = ?1", (id,))?;
        tx.commit()?;
        Ok(RemovedCredential::Removed(credential))
    }

    /// Removes every passkey and every session: what `recall-server
    /// reset-passkeys` does, for an owner who has lost them all. Answers how
    /// many passkeys went.
    pub fn reset_admin_credentials(&self) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM admin_credentials", [])?;
        tx.execute("DELETE FROM admin_sessions", [])?;
        tx.commit()?;
        Ok(n)
    }

    /// Stores a new session, only if the passkey that signed it in is still
    /// there: a sign-in that finishes as its passkey is removed must not
    /// outlive it.
    pub fn create_admin_session(
        &self,
        token_sha256: &str,
        credential_id: &str,
        now: &str,
        expires_at: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT INTO admin_sessions
                 (token_sha256, credential_id, created_at, last_used_at, expires_at)
             SELECT ?1, ?2, ?3, ?3, ?4
             WHERE EXISTS (SELECT 1 FROM admin_credentials WHERE id = ?2)",
            (token_sha256, credential_id, now, expires_at),
        )?;
        Ok(inserted == 1)
    }

    /// One session, by the hash of its token.
    pub fn admin_session(&self, token_sha256: &str) -> Result<Option<AdminSession>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT credential_id, created_at, last_used_at, expires_at
                 FROM admin_sessions WHERE token_sha256 = ?1",
                (token_sha256,),
                |r| {
                    Ok(AdminSession {
                        credential_id: r.get(0)?,
                        created_at: r.get(1)?,
                        last_used_at: r.get(2)?,
                        expires_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Records that a session was used, which is what keeps it from going
    /// idle.
    pub fn touch_admin_session(&self, token_sha256: &str, now: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE admin_sessions SET last_used_at = ?2 WHERE token_sha256 = ?1",
            (token_sha256, now),
        )?;
        Ok(())
    }

    /// Ends a session.
    pub fn delete_admin_session(&self, token_sha256: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM admin_sessions WHERE token_sha256 = ?1",
            (token_sha256,),
        )?;
        Ok(())
    }

    /// Removes sessions past their absolute limit, or idle since before
    /// `idle_before`. Answers how many went.
    pub fn sweep_admin_sessions(&self, now: &str, idle_before: &str) -> Result<usize> {
        let conn = self.lock();
        Ok(conn.execute(
            "DELETE FROM admin_sessions WHERE expires_at <= ?1 OR last_used_at < ?2",
            (now, idle_before),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential<'a>(id: &'a str, created_at: &'a str) -> NewAdminCredential<'a> {
        NewAdminCredential {
            id,
            user_handle: "u",
            name: "phone",
            passkey: "{}",
            sign_count: 0,
            created_at,
        }
    }

    const T0: &str = "2026-09-23T10:00:00.000Z";
    const T1: &str = "2026-09-23T11:00:00.000Z";

    #[test]
    fn only_the_first_passkey_can_be_added_as_the_first() {
        let st = Store::open_in_memory().unwrap();
        assert!(!st.has_admin_credentials().unwrap());
        assert_eq!(
            st.add_admin_credential(&credential("a", T0), true).unwrap(),
            AddedCredential::Added
        );
        assert!(st.has_admin_credentials().unwrap());
        assert_eq!(
            st.add_admin_credential(&credential("b", T0), true).unwrap(),
            AddedCredential::NotFirst
        );
        assert_eq!(
            st.add_admin_credential(&credential("a", T0), false)
                .unwrap(),
            AddedCredential::Duplicate
        );
        assert_eq!(
            st.add_admin_credential(&credential("b", T1), false)
                .unwrap(),
            AddedCredential::Added
        );
        let ids: Vec<String> = st
            .admin_credentials()
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    /// The counter rule, including the case webauthn-rs's own check cannot
    /// see: two sign-ins verified against the same stored counter, of which
    /// only the first may be recorded.
    #[test]
    fn a_sign_in_is_recorded_only_if_its_counter_moved_forward() {
        let st = Store::open_in_memory().unwrap();
        st.add_admin_credential(&credential("a", T0), true).unwrap();
        // Both zero: a synced passkey, every time.
        assert!(st.record_admin_sign_in("a", 0, "{}", T1).unwrap());
        assert!(st.record_admin_sign_in("a", 0, "{}", T1).unwrap());
        assert!(st.record_admin_sign_in("a", 5, "{}", T1).unwrap());
        assert!(!st.record_admin_sign_in("a", 5, "{}", T1).unwrap(), "equal");
        assert!(!st.record_admin_sign_in("a", 4, "{}", T1).unwrap(), "lower");
        assert!(!st.record_admin_sign_in("a", 0, "{}", T1).unwrap(), "reset");
        assert!(st.record_admin_sign_in("a", 6, "{}", T1).unwrap());
        let stored = st.admin_credential("a").unwrap().unwrap();
        assert_eq!(stored.sign_count, 6);
        assert_eq!(stored.last_used_at.as_deref(), Some(T1));
        assert!(!st.record_admin_sign_in("nobody", 9, "{}", T1).unwrap());
    }

    #[test]
    fn the_last_passkey_cannot_be_removed_and_removing_one_ends_its_sessions() {
        let st = Store::open_in_memory().unwrap();
        st.add_admin_credential(&credential("a", T0), true).unwrap();
        assert_eq!(
            st.remove_admin_credential("a").unwrap(),
            RemovedCredential::Last
        );
        st.add_admin_credential(&credential("b", T1), false)
            .unwrap();
        assert!(st.create_admin_session("s1", "a", T1, T1).unwrap());
        assert!(st.create_admin_session("s2", "b", T1, T1).unwrap());
        assert!(matches!(
            st.remove_admin_credential("a").unwrap(),
            RemovedCredential::Removed(c) if c.id == "a"
        ));
        assert_eq!(st.admin_session("s1").unwrap(), None);
        assert!(st.admin_session("s2").unwrap().is_some());
        assert_eq!(
            st.remove_admin_credential("a").unwrap(),
            RemovedCredential::NotFound
        );
        assert!(
            !st.create_admin_session("s3", "a", T1, T1).unwrap(),
            "a removed passkey signs nobody in"
        );
    }

    #[test]
    fn sessions_are_swept_when_idle_or_past_their_limit() {
        let st = Store::open_in_memory().unwrap();
        st.add_admin_credential(&credential("a", T0), true).unwrap();
        st.create_admin_session("idle", "a", T0, "2026-10-23T10:00:00.000Z")
            .unwrap();
        st.create_admin_session("old", "a", T1, T1).unwrap();
        st.create_admin_session("fine", "a", T1, "2026-10-23T10:00:00.000Z")
            .unwrap();
        assert_eq!(
            st.sweep_admin_sessions(T1, "2026-09-23T10:30:00.000Z")
                .unwrap(),
            2
        );
        assert!(st.admin_session("fine").unwrap().is_some());
        assert_eq!(st.reset_admin_credentials().unwrap(), 1);
        assert_eq!(st.admin_session("fine").unwrap(), None);
        assert!(!st.has_admin_credentials().unwrap());
    }
}
