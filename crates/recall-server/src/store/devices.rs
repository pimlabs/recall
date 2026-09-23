//! Devices, the enrolments waiting for approval, and enrolment keys.
//!
//! Every timestamp is stored in [`crate::now`]'s format. That is the format
//! the API answers in, and, being fixed-width, it compares correctly as a
//! string, so expiry and idleness are plain `<` in SQL.

use std::time::Duration;

use anyhow::Result;
use recall_wire::signature::{fingerprint, parse_public_key};
use recall_wire::{Device, EnrollKey};
use rusqlite::{Connection, OptionalExtension, Row};
use time::OffsetDateTime;

use super::Store;
use crate::{format_timestamp, parse_timestamp};

/// Created alongside `memory_files`, every time the store opens: `IF NOT
/// EXISTS` makes that a no-op once they exist.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS devices (
        id            TEXT PRIMARY KEY,
        -- Always the one owner. Reserved now because adding it later would
        -- be a migration: see Part 3 of docs/design/handshake.md.
        owner_id      TEXT NOT NULL DEFAULT 'owner',
        name          TEXT NOT NULL,
        public_key    TEXT NOT NULL,
        scope         TEXT NOT NULL CHECK (scope IN ('sync', 'admin')),
        agent         TEXT NOT NULL DEFAULT '',
        ephemeral     INTEGER NOT NULL DEFAULT 0,
        enroll_key_id TEXT,
        created_at    TEXT NOT NULL,
        last_seen     TEXT,
        revoked_at    TEXT
    );
    CREATE TABLE IF NOT EXISTS device_enrollments (
        enrollment_id TEXT PRIMARY KEY,
        user_code     TEXT NOT NULL,
        name          TEXT NOT NULL,
        public_key    TEXT NOT NULL,
        agent         TEXT NOT NULL DEFAULT '',
        created_at    TEXT NOT NULL,
        expires_at    TEXT NOT NULL,
        -- The device approving it made; NULL while it waits.
        device_id     TEXT,
        denied        INTEGER NOT NULL DEFAULT 0,
        last_poll_at  TEXT
    );
    CREATE INDEX IF NOT EXISTS device_enrollments_user_code
        ON device_enrollments (user_code);
    CREATE TABLE IF NOT EXISTS enroll_keys (
        id          TEXT PRIMARY KEY,
        -- The key itself is never stored: it is shown once, and a copy of
        -- the database is not a copy of it.
        key_sha256  TEXT NOT NULL UNIQUE,
        tag         TEXT NOT NULL DEFAULT '',
        ephemeral   INTEGER NOT NULL DEFAULT 0,
        created_at  TEXT NOT NULL,
        expires_at  TEXT NOT NULL,
        revoked_at  TEXT
    );
";

const DEVICE_COLUMNS: &str = "id, name, scope, ephemeral, agent, public_key, enroll_key_id, \
     created_at, last_seen, revoked_at";

const ENROLL_KEY_COLUMNS: &str = "id, tag, ephemeral, created_at, expires_at, revoked_at";

fn device_from(r: &Row<'_>) -> rusqlite::Result<Device> {
    let public_key: String = r.get(5)?;
    Ok(Device {
        id: r.get(0)?,
        name: r.get(1)?,
        scope: r.get(2)?,
        ephemeral: r.get::<_, i64>(3)? != 0,
        agent: r.get(4)?,
        // Only a key that parsed was ever stored.
        fingerprint: parse_public_key(&public_key)
            .map(|k| fingerprint(&k))
            .unwrap_or_default(),
        public_key,
        enroll_key_id: r.get(6)?,
        created_at: r.get(7)?,
        last_seen: r.get(8)?,
        revoked_at: r.get(9)?,
    })
}

fn enroll_key_from(r: &Row<'_>) -> rusqlite::Result<EnrollKey> {
    Ok(EnrollKey {
        id: r.get(0)?,
        tag: r.get(1)?,
        ephemeral: r.get::<_, i64>(2)? != 0,
        created_at: r.get(3)?,
        expires_at: r.get(4)?,
        revoked_at: r.get(5)?,
    })
}

fn get_device(conn: &Connection, id: &str) -> Result<Option<Device>> {
    Ok(conn
        .query_row(
            &format!("SELECT {DEVICE_COLUMNS} FROM devices WHERE id = ?1"),
            (id,),
            device_from,
        )
        .optional()?)
}

fn get_enroll_key(conn: &Connection, column: &str, value: &str) -> Result<Option<EnrollKey>> {
    Ok(conn
        .query_row(
            &format!("SELECT {ENROLL_KEY_COLUMNS} FROM enroll_keys WHERE {column} = ?1"),
            (value,),
            enroll_key_from,
        )
        .optional()?)
}

/// A device about to be stored.
#[derive(Debug, Clone)]
pub struct NewDevice<'a> {
    /// `dev_…`.
    pub id: &'a str,
    /// What the owner sees it as.
    pub name: &'a str,
    /// Base64url, as `recall_wire::signature::encode_public_key` writes it.
    pub public_key: &'a str,
    /// `sync` or `admin`.
    pub scope: &'a str,
    /// The client's `User-Agent`.
    pub agent: &'a str,
    /// Removed once idle, when true.
    pub ephemeral: bool,
    /// The enrolment key it came in with, if any.
    pub enroll_key_id: Option<&'a str>,
    /// Now.
    pub created_at: &'a str,
}

/// An enrolment about to be stored.
#[derive(Debug, Clone)]
pub struct NewEnrollment<'a> {
    /// `enr_…`: the secret the machine polls with.
    pub enrollment_id: &'a str,
    /// `XXXX-XXXX`.
    pub user_code: &'a str,
    /// What the machine asked to be called.
    pub name: &'a str,
    /// Its public key, base64url.
    pub public_key: &'a str,
    /// Its `User-Agent`.
    pub agent: &'a str,
    /// Now.
    pub created_at: &'a str,
    /// When the code stops being approvable.
    pub expires_at: &'a str,
}

/// What storing an enrolment came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    /// Stored.
    Created,
    /// Another enrolment still waiting has that user code; pick another.
    CodeTaken,
    /// Too many enrolments are waiting already.
    Full,
}

/// An enrolment key about to be stored.
#[derive(Debug, Clone)]
pub struct NewEnrollKey<'a> {
    /// `ek_…`.
    pub id: &'a str,
    /// SHA-256 of the key, lowercase hex.
    pub key_sha256: &'a str,
    /// Its label.
    pub tag: &'a str,
    /// Whether it enrols ephemeral devices.
    pub ephemeral: bool,
    /// Now.
    pub created_at: &'a str,
    /// When it stops working.
    pub expires_at: &'a str,
}

/// Where an enrolment stands when its machine polls, in RFC 8628 §3.5's
/// terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Approved: here is the device.
    Approved {
        /// Its id.
        device_id: String,
        /// Its scope.
        scope: String,
    },
    /// Still waiting.
    Pending,
    /// Still waiting, and asked too soon.
    SlowDown,
    /// Nobody approved it in time.
    Expired,
    /// Denied, or approved and then revoked.
    Denied,
    /// No such enrolment, or one swept away long after it expired.
    Unknown,
}

/// What approving or denying a code came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision<T> {
    /// Done.
    Done(T),
    /// No enrolment has that code.
    NotFound,
    /// It had, but the code expired.
    Expired,
    /// It was approved or denied already.
    AlreadyDecided,
}

/// The newest enrolment with `user_code`: `(enrollment_id, name,
/// public_key, agent, expires_at, decided)`.
type Pending = (String, String, String, String, String, bool);

/// An enrolment still waiting for a decision, as an approver is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// What the machine asked to be called.
    pub name: String,
    /// Its public key, base64url.
    pub public_key: String,
    /// Its `User-Agent`.
    pub agent: String,
    /// When its code stops being approvable.
    pub expires_at: String,
}

fn pending_by_code(conn: &Connection, user_code: &str) -> Result<Option<Pending>> {
    Ok(conn
        .query_row(
            "SELECT enrollment_id, name, public_key, agent, expires_at,
                    device_id IS NOT NULL OR denied != 0
             FROM device_enrollments WHERE user_code = ?1
             ORDER BY created_at DESC LIMIT 1",
            (user_code,),
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get::<_, bool>(5)?,
                ))
            },
        )
        .optional()?)
}

impl Store {
    /// Stores a pending enrolment, unless its code is in use by another
    /// one still waiting, or `max_pending` are waiting already.
    pub fn create_enrollment(&self, e: &NewEnrollment<'_>, max_pending: usize) -> Result<Created> {
        let conn = self.lock();
        // Waiting means unexpired and undecided. Counting and inserting
        // under one lock is what makes the cap and the code's uniqueness
        // hold under concurrent requests.
        let waiting = "expires_at > ?1 AND device_id IS NULL AND denied = 0";
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM device_enrollments WHERE {waiting}"),
            (e.created_at,),
            |r| r.get(0),
        )?;
        if count as usize >= max_pending {
            return Ok(Created::Full);
        }
        let taken: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM device_enrollments WHERE {waiting} AND user_code = ?2"),
            (e.created_at, e.user_code),
            |r| r.get(0),
        )?;
        if taken > 0 {
            return Ok(Created::CodeTaken);
        }
        conn.execute(
            "INSERT INTO device_enrollments
                 (enrollment_id, user_code, name, public_key, agent, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                e.enrollment_id,
                e.user_code,
                e.name,
                e.public_key,
                e.agent,
                e.created_at,
                e.expires_at,
            ),
        )?;
        Ok(Created::Created)
    }

    /// Answers a machine's poll, and records when it asked.
    ///
    /// A poll sooner than `interval` after the last one is told to slow
    /// down. A second of slack keeps a client that sleeps exactly the
    /// interval from being told so because its previous request spent a
    /// moment in flight.
    pub fn poll_enrollment(
        &self,
        enrollment_id: &str,
        now: OffsetDateTime,
        interval: Duration,
    ) -> Result<Poll> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT e.expires_at, e.denied, e.last_poll_at, e.device_id, d.scope, d.revoked_at
                 FROM device_enrollments e LEFT JOIN devices d ON d.id = e.device_id
                 WHERE e.enrollment_id = ?1",
                (enrollment_id,),
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)? != 0,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((expires_at, denied, last_poll_at, device_id, scope, revoked_at)) = row else {
            return Ok(Poll::Unknown);
        };
        if denied {
            return Ok(Poll::Denied);
        }
        // Approval wins over expiry: a code approved in its last second
        // is still collected by the poll after it.
        if let Some(device_id) = device_id {
            return Ok(match (scope, revoked_at) {
                (Some(scope), None) => Poll::Approved { device_id, scope },
                // Revoked, or an ephemeral device already swept: either
                // way the owner no longer wants it.
                _ => Poll::Denied,
            });
        }
        let now_text = format_timestamp(now);
        if expires_at <= now_text {
            return Ok(Poll::Expired);
        }
        let too_soon = last_poll_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some_and(|last| {
                let min = interval.saturating_sub(Duration::from_secs(1));
                now - last < min
            });
        conn.execute(
            "UPDATE device_enrollments SET last_poll_at = ?1 WHERE enrollment_id = ?2",
            (&now_text, enrollment_id),
        )?;
        Ok(if too_soon {
            Poll::SlowDown
        } else {
            Poll::Pending
        })
    }

    /// Approves the enrolment waiting with `user_code`, making it device
    /// `device_id`.
    pub fn approve_enrollment(
        &self,
        user_code: &str,
        device_id: &str,
        scope: &str,
        now: &str,
    ) -> Result<Decision<Device>> {
        let mut conn = self.lock();
        let Some((enrollment_id, name, public_key, agent, expires_at, decided)) =
            pending_by_code(&conn, user_code)?
        else {
            return Ok(Decision::NotFound);
        };
        if decided {
            return Ok(Decision::AlreadyDecided);
        }
        if expires_at.as_str() <= now {
            return Ok(Decision::Expired);
        }
        // One transaction, so a device never exists without the enrolment
        // that made it knowing, and a crash between the two leaves neither.
        let tx = conn.transaction()?;
        insert_device(
            &tx,
            &NewDevice {
                id: device_id,
                name: &name,
                public_key: &public_key,
                scope,
                agent: &agent,
                ephemeral: false,
                enroll_key_id: None,
                created_at: now,
            },
        )?;
        tx.execute(
            "UPDATE device_enrollments SET device_id = ?1 WHERE enrollment_id = ?2",
            (device_id, &enrollment_id),
        )?;
        tx.commit()?;
        let device = get_device(&conn, device_id)?.expect("inserted above");
        Ok(Decision::Done(device))
    }

    /// Denies the enrolment waiting with `user_code`, answering with the
    /// name it asked for.
    pub fn deny_enrollment(&self, user_code: &str, now: &str) -> Result<Decision<String>> {
        let conn = self.lock();
        let Some((enrollment_id, name, _, _, expires_at, decided)) =
            pending_by_code(&conn, user_code)?
        else {
            return Ok(Decision::NotFound);
        };
        if decided {
            return Ok(Decision::AlreadyDecided);
        }
        if expires_at.as_str() <= now {
            return Ok(Decision::Expired);
        }
        conn.execute(
            "UPDATE device_enrollments SET denied = 1 WHERE enrollment_id = ?1",
            (&enrollment_id,),
        )?;
        Ok(Decision::Done(name))
    }

    /// What the enrolment waiting with `user_code` asked for, judged the
    /// way approving it would be, so a lookup and the approval after it
    /// never disagree about whether the code is still good.
    pub fn pending_enrollment(&self, user_code: &str, now: &str) -> Result<Decision<Waiting>> {
        let conn = self.lock();
        let Some((_, name, public_key, agent, expires_at, decided)) =
            pending_by_code(&conn, user_code)?
        else {
            return Ok(Decision::NotFound);
        };
        if decided {
            return Ok(Decision::AlreadyDecided);
        }
        if expires_at.as_str() <= now {
            return Ok(Decision::Expired);
        }
        Ok(Decision::Done(Waiting {
            name,
            public_key,
            agent,
            expires_at,
        }))
    }

    /// Stores a device, and answers with it as the API shows it.
    pub fn insert_device(&self, d: &NewDevice<'_>) -> Result<Device> {
        let conn = self.lock();
        insert_device(&conn, d)?;
        Ok(get_device(&conn, d.id)?.expect("inserted above"))
    }

    /// One device, revoked or not.
    pub fn device(&self, id: &str) -> Result<Option<Device>> {
        get_device(&self.lock(), id)
    }

    /// Every device, newest first.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEVICE_COLUMNS} FROM devices ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map([], device_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes a device. Revoking one already revoked keeps the first
    /// time. [`None`] when there is no such device.
    pub fn revoke_device(&self, id: &str, now: &str) -> Result<Option<Device>> {
        let conn = self.lock();
        conn.execute(
            "UPDATE devices SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
            (now, id),
        )?;
        get_device(&conn, id)
    }

    /// Records that a device was just seen.
    pub fn touch_device(&self, id: &str, now: &str) -> Result<()> {
        self.lock()
            .execute("UPDATE devices SET last_seen = ?1 WHERE id = ?2", (now, id))?;
        Ok(())
    }

    /// Stores an enrolment key's hash and details.
    pub fn insert_enroll_key(&self, k: &NewEnrollKey<'_>) -> Result<EnrollKey> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO enroll_keys (id, key_sha256, tag, ephemeral, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                k.id,
                k.key_sha256,
                k.tag,
                k.ephemeral as i64,
                k.created_at,
                k.expires_at,
            ),
        )?;
        Ok(get_enroll_key(&conn, "id", k.id)?.expect("inserted above"))
    }

    /// The enrolment key whose SHA-256 is `key_sha256`, in any state.
    pub fn enroll_key_by_hash(&self, key_sha256: &str) -> Result<Option<EnrollKey>> {
        get_enroll_key(&self.lock(), "key_sha256", key_sha256)
    }

    /// Every enrolment key, newest first.
    pub fn enroll_keys(&self) -> Result<Vec<EnrollKey>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ENROLL_KEY_COLUMNS} FROM enroll_keys ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map([], enroll_key_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes an enrolment key, keeping the first time if it already
    /// was. Devices it enrolled are untouched. [`None`] when there is no
    /// such key.
    pub fn revoke_enroll_key(&self, id: &str, now: &str) -> Result<Option<EnrollKey>> {
        let conn = self.lock();
        conn.execute(
            "UPDATE enroll_keys SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
            (now, id),
        )?;
        get_enroll_key(&conn, "id", id)
    }

    /// Removes ephemeral devices last seen (or, never seen, created)
    /// before `idle_before`, and enrolments that expired before
    /// `expired_before`. Answers how many of each went.
    pub fn sweep_devices(&self, idle_before: &str, expired_before: &str) -> Result<(usize, usize)> {
        let conn = self.lock();
        let devices = conn.execute(
            "DELETE FROM devices WHERE ephemeral = 1 AND COALESCE(last_seen, created_at) < ?1",
            (idle_before,),
        )?;
        let enrollments = conn.execute(
            "DELETE FROM device_enrollments WHERE expires_at < ?1",
            (expired_before,),
        )?;
        Ok((devices, enrollments))
    }
}

fn insert_device(conn: &Connection, d: &NewDevice<'_>) -> Result<()> {
    conn.execute(
        "INSERT INTO devices
             (id, name, public_key, scope, agent, ephemeral, enroll_key_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            d.id,
            d.name,
            d.public_key,
            d.scope,
            d.agent,
            d.ephemeral as i64,
            d.enroll_key_id,
            d.created_at,
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs";

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_790_000_000 + secs).unwrap()
    }

    fn ts(secs: i64) -> String {
        format_timestamp(at(secs))
    }

    fn enroll(st: &Store, id: &str, code: &str, created: i64) -> Created {
        st.create_enrollment(
            &NewEnrollment {
                enrollment_id: id,
                user_code: code,
                name: "laptop",
                public_key: KEY,
                agent: "recall/0.4.1",
                created_at: &ts(created),
                expires_at: &ts(created + 900),
            },
            3,
        )
        .unwrap()
    }

    #[test]
    fn the_tables_are_created_once_and_reopening_keeps_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.db");
        {
            let st = Store::open(&path).unwrap();
            assert_eq!(enroll(&st, "enr_a", "BCDF-GHJK", 0), Created::Created);
        }
        let st = Store::open(&path).unwrap();
        assert_eq!(
            st.poll_enrollment("enr_a", at(10), Duration::from_secs(5))
                .unwrap(),
            Poll::Pending
        );
    }

    #[test]
    fn a_code_in_use_is_refused_and_the_waiting_list_is_capped() {
        let st = Store::open_in_memory().unwrap();
        assert_eq!(enroll(&st, "enr_a", "BCDF-GHJK", 0), Created::Created);
        assert_eq!(enroll(&st, "enr_b", "BCDF-GHJK", 1), Created::CodeTaken);
        assert_eq!(enroll(&st, "enr_b", "BCDF-GHJL", 1), Created::Created);
        assert_eq!(enroll(&st, "enr_c", "BCDF-GHJM", 2), Created::Created);
        assert_eq!(enroll(&st, "enr_d", "BCDF-GHJN", 3), Created::Full);
        // Once they expire they no longer count, and their codes are free.
        assert_eq!(enroll(&st, "enr_d", "BCDF-GHJK", 1000), Created::Created);
    }

    #[test]
    fn a_poll_follows_rfc8628() {
        let st = Store::open_in_memory().unwrap();
        let every = Duration::from_secs(5);
        enroll(&st, "enr_a", "BCDF-GHJK", 0);
        assert_eq!(
            st.poll_enrollment("enr_a", at(1), every).unwrap(),
            Poll::Pending
        );
        assert_eq!(
            st.poll_enrollment("enr_a", at(3), every).unwrap(),
            Poll::SlowDown
        );
        // Four seconds after the last poll is within the slack.
        assert_eq!(
            st.poll_enrollment("enr_a", at(7), every).unwrap(),
            Poll::Pending
        );
        assert_eq!(
            st.poll_enrollment("enr_x", at(8), every).unwrap(),
            Poll::Unknown
        );
        assert_eq!(
            st.poll_enrollment("enr_a", at(900), every).unwrap(),
            Poll::Expired
        );

        enroll(&st, "enr_b", "BCDF-GHJL", 0);
        let Decision::Done(device) = st
            .approve_enrollment("BCDF-GHJL", "dev_1", "admin", &ts(10))
            .unwrap()
        else {
            panic!("not approved");
        };
        assert_eq!(
            (device.name.as_str(), device.scope.as_str()),
            ("laptop", "admin")
        );
        assert!(!device.fingerprint.is_empty());
        // Approved in time and collected late is still collected.
        assert_eq!(
            st.poll_enrollment("enr_b", at(2000), every).unwrap(),
            Poll::Approved {
                device_id: "dev_1".into(),
                scope: "admin".into()
            }
        );
        st.revoke_device("dev_1", &ts(20)).unwrap();
        assert_eq!(
            st.poll_enrollment("enr_b", at(30), every).unwrap(),
            Poll::Denied
        );
    }

    #[test]
    fn a_code_is_decided_once() {
        let st = Store::open_in_memory().unwrap();
        enroll(&st, "enr_a", "BCDF-GHJK", 0);
        let Decision::Done(waiting) = st.pending_enrollment("BCDF-GHJK", &ts(1)).unwrap() else {
            panic!("not waiting");
        };
        assert_eq!(
            (waiting.name.as_str(), waiting.expires_at),
            ("laptop", ts(900))
        );
        assert_eq!(
            st.pending_enrollment("BCDF-GHJK", &ts(900)).unwrap(),
            Decision::Expired
        );
        assert_eq!(
            st.pending_enrollment("ZZZZ-ZZZZ", &ts(1)).unwrap(),
            Decision::NotFound
        );
        assert_eq!(
            st.deny_enrollment("BCDF-GHJK", &ts(1)).unwrap(),
            Decision::Done("laptop".into())
        );
        assert_eq!(
            st.approve_enrollment("BCDF-GHJK", "dev_1", "sync", &ts(2))
                .unwrap(),
            Decision::AlreadyDecided
        );
        assert_eq!(
            st.poll_enrollment("enr_a", at(10), Duration::from_secs(5))
                .unwrap(),
            Poll::Denied
        );
        assert_eq!(
            st.approve_enrollment("ZZZZ-ZZZZ", "dev_1", "sync", &ts(2))
                .unwrap(),
            Decision::NotFound
        );
        enroll(&st, "enr_b", "BCDF-GHJL", 0);
        assert_eq!(
            st.approve_enrollment("BCDF-GHJL", "dev_1", "sync", &ts(901))
                .unwrap(),
            Decision::Expired
        );
        assert!(st.devices().unwrap().is_empty(), "nothing was approved");
    }

    #[test]
    fn revoking_keeps_the_first_time_and_the_row() {
        let st = Store::open_in_memory().unwrap();
        st.insert_device(&NewDevice {
            id: "dev_1",
            name: "laptop",
            public_key: KEY,
            scope: "sync",
            agent: "",
            ephemeral: false,
            enroll_key_id: None,
            created_at: &ts(0),
        })
        .unwrap();
        let first = st.revoke_device("dev_1", &ts(1)).unwrap().unwrap();
        let again = st.revoke_device("dev_1", &ts(2)).unwrap().unwrap();
        assert_eq!(first.revoked_at, Some(ts(1)));
        assert_eq!(again.revoked_at, Some(ts(1)));
        assert!(st.revoke_device("dev_none", &ts(3)).unwrap().is_none());
        assert_eq!(st.devices().unwrap().len(), 1);
    }

    #[test]
    fn only_idle_ephemeral_devices_and_long_expired_enrolments_are_swept() {
        let st = Store::open_in_memory().unwrap();
        for (id, ephemeral) in [("dev_kept", false), ("dev_idle", true), ("dev_busy", true)] {
            st.insert_device(&NewDevice {
                id,
                name: id,
                public_key: KEY,
                scope: "sync",
                agent: "",
                ephemeral,
                enroll_key_id: None,
                created_at: &ts(0),
            })
            .unwrap();
        }
        st.touch_device("dev_busy", &ts(5000)).unwrap();
        enroll(&st, "enr_old", "BCDF-GHJK", 0);
        enroll(&st, "enr_new", "BCDF-GHJL", 4000);

        let (devices, enrollments) = st.sweep_devices(&ts(3600), &ts(3600)).unwrap();
        assert_eq!((devices, enrollments), (1, 1));
        let left: Vec<String> = st.devices().unwrap().into_iter().map(|d| d.id).collect();
        assert!(left.contains(&"dev_kept".to_string()) && left.contains(&"dev_busy".to_string()));
        assert_eq!(
            st.poll_enrollment("enr_old", at(3700), Duration::from_secs(5))
                .unwrap(),
            Poll::Unknown
        );
    }

    #[test]
    fn enrolment_keys_are_found_by_hash_and_revoked_once() {
        let st = Store::open_in_memory().unwrap();
        st.insert_enroll_key(&NewEnrollKey {
            id: "ek_1",
            key_sha256: "abc",
            tag: "cloud",
            ephemeral: true,
            created_at: &ts(0),
            expires_at: &ts(86400),
        })
        .unwrap();
        let key = st.enroll_key_by_hash("abc").unwrap().unwrap();
        assert_eq!((key.id.as_str(), key.ephemeral), ("ek_1", true));
        assert!(st.enroll_key_by_hash("abd").unwrap().is_none());
        let revoked = st.revoke_enroll_key("ek_1", &ts(1)).unwrap().unwrap();
        assert_eq!(revoked.revoked_at, Some(ts(1)));
        assert_eq!(
            st.revoke_enroll_key("ek_1", &ts(2))
                .unwrap()
                .unwrap()
                .revoked_at,
            Some(ts(1))
        );
        assert_eq!(st.enroll_keys().unwrap().len(), 1);
    }
}
