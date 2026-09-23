//! Devices, the enrolments waiting for approval, and authkeys.
//!
//! Every timestamp is stored in [`crate::now`]'s format. That is the format
//! the API answers in, and, being fixed-width, it compares correctly as a
//! string, so expiry and idleness are plain `<` in SQL.

use std::time::Duration;

use anyhow::Result;
use recall_wire::signature::{fingerprint, parse_public_key};
use recall_wire::{Authkey, Device};
use rusqlite::{Connection, OptionalExtension, Row};
use time::OffsetDateTime;
use unicode_normalization::UnicodeNormalization;

use super::Store;
use crate::{format_timestamp, parse_timestamp};

/// The `devices` table's columns, for creating it and for rebuilding it
/// (see [`allow_worker_scope`]). A macro so both can be one literal.
macro_rules! devices_columns {
    () => {
        "(
        id            TEXT PRIMARY KEY,
        -- Always the one owner. Reserved now because adding it later would
        -- be a migration: see Part 3 of docs/design/handshake.md.
        owner_id      TEXT NOT NULL DEFAULT 'owner',
        name          TEXT NOT NULL,
        public_key    TEXT NOT NULL,
        scope         TEXT NOT NULL CHECK (scope IN ('sync', 'admin', 'worker')),
        agent         TEXT NOT NULL DEFAULT '',
        ephemeral     INTEGER NOT NULL DEFAULT 0,
        authkey_id    TEXT,
        created_at    TEXT NOT NULL,
        last_seen     TEXT,
        revoked_at    TEXT
    )"
    };
}

/// Created alongside `memory_files`, every time the store opens: `IF NOT
/// EXISTS` makes that a no-op once they exist.
pub(super) const SCHEMA: &str = concat!(
    "CREATE TABLE IF NOT EXISTS devices ",
    devices_columns!(),
    ";",
    "
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
        last_poll_at  TEXT,
        -- The address it came from, as the rate limiter keys it: what caps
        -- how many one address may have waiting.
        client_ip     TEXT NOT NULL DEFAULT ''
    );
    CREATE INDEX IF NOT EXISTS device_enrollments_user_code
        ON device_enrollments (user_code);
    CREATE TABLE IF NOT EXISTS authkeys (
        id          TEXT PRIMARY KEY,
        -- The key itself is never stored: it is shown once, and a copy of
        -- the database is not a copy of it.
        key_sha256  TEXT NOT NULL UNIQUE,
        tag         TEXT NOT NULL DEFAULT '',
        ephemeral   INTEGER NOT NULL DEFAULT 0,
        -- The most unrevoked devices it may have enrolled at once. Always
        -- stored; NULL would be read as the default, not as no limit.
        max_devices INTEGER,
        created_at  TEXT NOT NULL,
        expires_at  TEXT NOT NULL,
        revoked_at  TEXT
    );
"
);

/// Lets `devices.scope` be `worker`, on a database made before it could.
///
/// SQLite cannot change a `CHECK` constraint in place, so the table is
/// rebuilt: created under another name with the new constraint, the rows
/// copied, the old table dropped and the new one renamed, in one
/// transaction, so a crash leaves either the old table or the new one and
/// never neither. The rows are untouched, so an older server reading the
/// rebuilt table sees exactly what it wrote; a `worker` row reads to it as
/// a device without admin rights, which the owner approved.
///
/// Guarded by the table's own definition rather than `PRAGMA
/// user_version`: whether the constraint allows `worker` is exactly the
/// question, and a version number is a second thing to keep in step with
/// it, one another change to the schema could also want to move.
pub(super) fn allow_worker_scope(conn: &Connection) -> Result<()> {
    let sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'devices'",
        [],
        |r| r.get(0),
    )?;
    if sql.contains("'worker'") {
        return Ok(());
    }
    conn.execute_batch(concat!(
        "BEGIN IMMEDIATE;
         DROP TABLE IF EXISTS devices_rebuilt;
         CREATE TABLE devices_rebuilt ",
        devices_columns!(),
        ";
         INSERT INTO devices_rebuilt
             (id, owner_id, name, public_key, scope, agent, ephemeral, authkey_id,
              created_at, last_seen, revoked_at)
         SELECT id, owner_id, name, public_key, scope, agent, ephemeral, authkey_id,
                created_at, last_seen, revoked_at
         FROM devices;
         DROP TABLE devices;
         ALTER TABLE devices_rebuilt RENAME TO devices;
         COMMIT;"
    ))?;
    Ok(())
}

const DEVICE_COLUMNS: &str = "id, name, scope, ephemeral, agent, public_key, authkey_id, \
     created_at, last_seen, revoked_at";

const AUTHKEY_COLUMNS: &str = "id, tag, ephemeral, max_devices, created_at, expires_at, revoked_at";

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
        authkey_id: r.get(6)?,
        created_at: r.get(7)?,
        last_seen: r.get(8)?,
        revoked_at: r.get(9)?,
    })
}

fn authkey_from(r: &Row<'_>) -> rusqlite::Result<Authkey> {
    Ok(Authkey {
        id: r.get(0)?,
        tag: r.get(1)?,
        ephemeral: r.get::<_, i64>(2)? != 0,
        max_devices: r.get(3)?,
        created_at: r.get(4)?,
        expires_at: r.get(5)?,
        revoked_at: r.get(6)?,
    })
}

/// A device name, or an authkey's tag, as it is stored: trimmed, and
/// in Unicode's composed form (NFC), so the same letters typed as one
/// character or as a letter and its accent are stored alike.
pub fn plain_name(name: &str) -> String {
    name.trim().nfc().collect()
}

/// What a name is compared by: two keys, and two names are the same name
/// when either key is.
///
/// Both begin with NFKC, so a name typed decomposed, or with a ligature or
/// a full-width letter, is the name typed plainly. Then each takes the
/// name in one case and reduces it to its confusable skeleton (UTS #39),
/// which maps every character to the one it can be mistaken for, so
/// `lаptop` with a Cyrillic `а`, or `1aptop`, is `laptop`. It takes two
/// because a pair can look alike in one case and not the other: Cyrillic
/// `к` does not look like `k`, but `К` looks like `K`; and lowercasing
/// keeps `ß` apart from `ss`, where uppercasing makes it `SS`, so
/// `Straße` is `STRASSE`.
fn name_keys(name: &str) -> [String; 2] {
    let plain: String = name.trim().nfkc().collect();
    let skeleton = |s: String| unicode_security::skeleton(&s).collect::<String>();
    [
        skeleton(plain.to_lowercase()),
        skeleton(plain.to_uppercase()),
    ]
}

/// Whether an unrevoked device already has `name`, or one a person would
/// read as it (see [`name_keys`]), so neither `Laptop` nor `lаptop` can
/// stand beside `laptop`. A revoked device's name is free again.
fn name_taken(conn: &Connection, name: &str) -> Result<bool> {
    let [lower, upper] = name_keys(name);
    let mut stmt = conn.prepare("SELECT name FROM devices WHERE revoked_at IS NULL")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let [their_lower, their_upper] = name_keys(&row.get::<_, String>(0)?);
        if lower == their_lower || upper == their_upper {
            return Ok(true);
        }
    }
    Ok(false)
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

fn get_authkey(conn: &Connection, column: &str, value: &str) -> Result<Option<Authkey>> {
    Ok(conn
        .query_row(
            &format!("SELECT {AUTHKEY_COLUMNS} FROM authkeys WHERE {column} = ?1"),
            (value,),
            authkey_from,
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
    /// The authkey it came in with, if any.
    pub authkey_id: Option<&'a str>,
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
    /// The address it came from, as the rate limiter keys it.
    pub client_ip: &'a str,
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
    /// Too many enrolments from this address are waiting already.
    AddressFull,
}

/// What storing a device came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inserted {
    /// Stored. Boxed: the refusals are small, and a device is not.
    Done(Box<Device>),
    /// An unrevoked device already has that name.
    NameTaken,
    /// Its authkey already has as many unrevoked devices as it may.
    KeyFull,
}

/// An authkey about to be stored.
#[derive(Debug, Clone)]
pub struct NewAuthkey<'a> {
    /// `ak_…`.
    pub id: &'a str,
    /// SHA-256 of the key, lowercase hex.
    pub key_sha256: &'a str,
    /// Its label.
    pub tag: &'a str,
    /// Whether it enrols ephemeral devices.
    pub ephemeral: bool,
    /// The most unrevoked devices it may have enrolled at once.
    pub max_devices: Option<u32>,
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
    /// The approver named a key fingerprint, and the code's key has
    /// another.
    KeyMismatch,
    /// An unrevoked device already has the name it asked for.
    NameTaken(String),
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
    /// one still waiting, `max_pending` are waiting already, or
    /// `max_per_address` from its address are.
    pub fn create_enrollment(
        &self,
        e: &NewEnrollment<'_>,
        max_pending: usize,
        max_per_address: usize,
    ) -> Result<Created> {
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
        let from_here: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM device_enrollments WHERE {waiting} AND client_ip = ?2"),
            (e.created_at, e.client_ip),
            |r| r.get(0),
        )?;
        if from_here as usize >= max_per_address {
            return Ok(Created::AddressFull);
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
                 (enrollment_id, user_code, name, public_key, agent, created_at, expires_at,
                  client_ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                e.enrollment_id,
                e.user_code,
                e.name,
                e.public_key,
                e.agent,
                e.created_at,
                e.expires_at,
                e.client_ip,
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
    /// `device_id`. When `expected_fingerprint` is given, the code's key
    /// must have exactly that fingerprint.
    pub fn approve_enrollment(
        &self,
        user_code: &str,
        device_id: &str,
        scope: &str,
        now: &str,
        expected_fingerprint: Option<&str>,
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
        if let Some(expected) = expected_fingerprint {
            let actual = parse_public_key(&public_key)
                .map(|k| fingerprint(&k))
                .unwrap_or_default();
            if expected.trim() != actual {
                return Ok(Decision::KeyMismatch);
            }
        }
        // Checked under the same lock as the insert, so two approvals of
        // two machines both called `laptop` cannot both succeed.
        if name_taken(&conn, &name)? {
            return Ok(Decision::NameTaken(name));
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
                authkey_id: None,
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

    /// Stores a device, unless an unrevoked one has its name or, when
    /// `max_for_key` is given, its authkey already has that many
    /// unrevoked devices. Both are checked under the lock the insert holds.
    pub fn insert_device(&self, d: &NewDevice<'_>, max_for_key: Option<u32>) -> Result<Inserted> {
        let conn = self.lock();
        if name_taken(&conn, d.name)? {
            return Ok(Inserted::NameTaken);
        }
        if let (Some(max), Some(key)) = (max_for_key, d.authkey_id) {
            let live: i64 = conn.query_row(
                "SELECT COUNT(*) FROM devices WHERE authkey_id = ?1 AND revoked_at IS NULL",
                (key,),
                |r| r.get(0),
            )?;
            if live >= i64::from(max) {
                return Ok(Inserted::KeyFull);
            }
        }
        insert_device(&conn, d)?;
        Ok(Inserted::Done(Box::new(
            get_device(&conn, d.id)?.expect("inserted above"),
        )))
    }

    /// One device, revoked or not.
    pub fn device(&self, id: &str) -> Result<Option<Device>> {
        get_device(&self.lock(), id)
    }

    /// The newest unrevoked device with the `worker` scope. While there is
    /// one, a stale push is queued for it rather than merged inline.
    pub fn enrolled_worker(&self) -> Result<Option<Device>> {
        Ok(self
            .lock()
            .query_row(
                &format!(
                    "SELECT {DEVICE_COLUMNS} FROM devices
                     WHERE scope = 'worker' AND revoked_at IS NULL
                     ORDER BY created_at DESC, id LIMIT 1"
                ),
                [],
                device_from,
            )
            .optional()?)
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

    /// Stores an authkey's hash and details.
    pub fn insert_authkey(&self, k: &NewAuthkey<'_>) -> Result<Authkey> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO authkeys
                 (id, key_sha256, tag, ephemeral, max_devices, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                k.id,
                k.key_sha256,
                k.tag,
                k.ephemeral as i64,
                k.max_devices,
                k.created_at,
                k.expires_at,
            ),
        )?;
        Ok(get_authkey(&conn, "id", k.id)?.expect("inserted above"))
    }

    /// The authkey whose SHA-256 is `key_sha256`, in any state.
    pub fn authkey_by_hash(&self, key_sha256: &str) -> Result<Option<Authkey>> {
        get_authkey(&self.lock(), "key_sha256", key_sha256)
    }

    /// Every authkey, newest first.
    pub fn authkeys(&self) -> Result<Vec<Authkey>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AUTHKEY_COLUMNS} FROM authkeys ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map([], authkey_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes an authkey, keeping the first time if it already
    /// was, and with `devices` every device it enrolled too; without it
    /// they are untouched. [`None`] when there is no such key.
    pub fn revoke_authkey(&self, id: &str, now: &str, devices: bool) -> Result<Option<Authkey>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE authkeys SET revoked_at = COALESCE(revoked_at, ?1) WHERE id = ?2",
            (now, id),
        )?;
        if devices {
            tx.execute(
                "UPDATE devices SET revoked_at = COALESCE(revoked_at, ?1) WHERE authkey_id = ?2",
                (now, id),
            )?;
        }
        tx.commit()?;
        get_authkey(&conn, "id", id)
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
             (id, name, public_key, scope, agent, ephemeral, authkey_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        (
            d.id,
            d.name,
            d.public_key,
            d.scope,
            d.agent,
            d.ephemeral as i64,
            d.authkey_id,
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

    fn enroll_from(st: &Store, id: &str, code: &str, created: i64, ip: &str) -> Created {
        st.create_enrollment(
            &NewEnrollment {
                enrollment_id: id,
                user_code: code,
                name: "laptop",
                public_key: KEY,
                agent: "recall/0.4.1",
                created_at: &ts(created),
                expires_at: &ts(created + 900),
                client_ip: ip,
            },
            3,
            2,
        )
        .unwrap()
    }

    fn enroll(st: &Store, id: &str, code: &str, created: i64) -> Created {
        // Each from its own address, so only the tests about the address
        // cap meet it.
        enroll_from(st, id, code, created, id)
    }

    fn device<'a>(id: &'a str, name: &'a str, key: Option<&'a str>) -> NewDevice<'a> {
        NewDevice {
            id,
            name,
            public_key: KEY,
            scope: "sync",
            agent: "",
            ephemeral: false,
            authkey_id: key,
            created_at: "2026-09-23T00:00:00.000Z",
        }
    }

    fn inserted(st: &Store, d: &NewDevice<'_>) -> Device {
        match st.insert_device(d, None).unwrap() {
            Inserted::Done(device) => *device,
            other => panic!("not inserted: {other:?}"),
        }
    }

    /// A database made before the worker scope existed is rebuilt to allow
    /// it, keeping every row, and only once.
    #[test]
    fn an_older_devices_table_is_rebuilt_to_allow_workers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&SCHEMA.replace("'sync', 'admin', 'worker'", "'sync', 'admin'"))
                .unwrap();
            conn.execute(
                "INSERT INTO devices (id, name, public_key, scope, agent, created_at, last_seen)
                 VALUES ('dev_old', 'laptop', ?1, 'admin', 'recall/0.4.1', ?2, ?2)",
                (KEY, ts(0)),
            )
            .unwrap();
            assert!(conn
                .execute(
                    "INSERT INTO devices (id, name, public_key, scope, created_at)
                     VALUES ('dev_w', 'w', ?1, 'worker', ?2)",
                    (KEY, ts(0)),
                )
                .is_err());
        }
        let st = Store::open(&path).unwrap();
        let kept = st.device("dev_old").unwrap().unwrap();
        assert_eq!(
            (kept.name.as_str(), kept.scope.as_str(), kept.last_seen),
            ("laptop", "admin", Some(ts(0)))
        );
        inserted(
            &st,
            &NewDevice {
                scope: "worker",
                ..device("dev_w", "worker", None)
            },
        );
        assert_eq!(st.enrolled_worker().unwrap().unwrap().id, "dev_w");
        // Opening again finds the table already rebuilt.
        drop(st);
        let st = Store::open(&path).unwrap();
        assert_eq!(st.devices().unwrap().len(), 2);
        // Something other than a known scope is still refused.
        assert!(st
            .insert_device(
                &NewDevice {
                    scope: "root",
                    ..device("dev_x", "x", None)
                },
                None
            )
            .is_err());
    }

    #[test]
    fn a_revoked_worker_is_no_worker() {
        let st = Store::open_in_memory().unwrap();
        assert!(st.enrolled_worker().unwrap().is_none());
        inserted(
            &st,
            &NewDevice {
                scope: "worker",
                ..device("dev_w", "worker", None)
            },
        );
        assert!(st.enrolled_worker().unwrap().is_some());
        st.revoke_device("dev_w", &ts(1)).unwrap();
        assert!(st.enrolled_worker().unwrap().is_none());
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

    /// One address cannot hold the whole waiting list: the cap the review
    /// asked for, keyed the way the rate limiter keys it.
    #[test]
    fn one_address_may_have_only_so_many_waiting() {
        let st = Store::open_in_memory().unwrap();
        let from = |id, code, ip| enroll_from(&st, id, code, 0, ip);
        assert_eq!(from("enr_a", "BCDF-GHJK", "198.51.100.4"), Created::Created);
        assert_eq!(from("enr_b", "BCDF-GHJL", "198.51.100.4"), Created::Created);
        assert_eq!(
            from("enr_c", "BCDF-GHJM", "198.51.100.4"),
            Created::AddressFull
        );
        assert_eq!(from("enr_c", "BCDF-GHJM", "198.51.100.5"), Created::Created);
        // A decided one no longer counts against its address.
        st.deny_enrollment("BCDF-GHJK", &ts(1)).unwrap();
        assert_eq!(from("enr_d", "BCDF-GHJN", "198.51.100.4"), Created::Created);
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
            .approve_enrollment("BCDF-GHJL", "dev_1", "admin", &ts(10), None)
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
            st.approve_enrollment("BCDF-GHJK", "dev_1", "sync", &ts(2), None)
                .unwrap(),
            Decision::AlreadyDecided
        );
        assert_eq!(
            st.poll_enrollment("enr_a", at(10), Duration::from_secs(5))
                .unwrap(),
            Poll::Denied
        );
        assert_eq!(
            st.approve_enrollment("ZZZZ-ZZZZ", "dev_1", "sync", &ts(2), None)
                .unwrap(),
            Decision::NotFound
        );
        enroll(&st, "enr_b", "BCDF-GHJL", 0);
        assert_eq!(
            st.approve_enrollment("BCDF-GHJL", "dev_1", "sync", &ts(901), None)
                .unwrap(),
            Decision::Expired
        );
        assert!(st.devices().unwrap().is_empty(), "nothing was approved");
    }

    /// An approval that names a fingerprint approves only that key.
    #[test]
    fn an_approval_is_bound_to_the_fingerprint_it_names() {
        let st = Store::open_in_memory().unwrap();
        enroll(&st, "enr_a", "BCDF-GHJK", 0);
        let right = fingerprint(&parse_public_key(KEY).unwrap());
        assert_eq!(
            st.approve_enrollment("BCDF-GHJK", "dev_1", "sync", &ts(1), Some("SHA256:other"))
                .unwrap(),
            Decision::KeyMismatch
        );
        assert!(st.devices().unwrap().is_empty(), "nothing was approved");
        assert!(matches!(
            st.approve_enrollment("BCDF-GHJK", "dev_1", "sync", &ts(2), Some(&right))
                .unwrap(),
            Decision::Done(_)
        ));
    }

    /// Two live devices may not share a name, in any case; a revoked one's
    /// name is free again.
    #[test]
    fn names_are_unique_among_devices_not_revoked() {
        let st = Store::open_in_memory().unwrap();
        inserted(&st, &device("dev_1", "laptop", None));
        assert_eq!(
            st.insert_device(&device("dev_2", "Laptop", None), None)
                .unwrap(),
            Inserted::NameTaken
        );

        enroll(&st, "enr_a", "BCDF-GHJK", 0);
        assert_eq!(
            st.approve_enrollment("BCDF-GHJK", "dev_2", "sync", &ts(1), None)
                .unwrap(),
            Decision::NameTaken("laptop".into())
        );

        st.revoke_device("dev_1", &ts(2)).unwrap();
        assert!(matches!(
            st.approve_enrollment("BCDF-GHJK", "dev_2", "sync", &ts(3), None)
                .unwrap(),
            Decision::Done(_)
        ));
    }

    /// Verification finding N2: names a person reads as one name are one
    /// name, whatever characters spell them, in either order.
    #[test]
    fn names_that_look_alike_are_one_name() {
        for (a, b) in [
            // A Cyrillic а.
            ("laptop", "l\u{0430}ptop"),
            // é as one character, and as e and a combining accent.
            ("caf\u{00E9}", "cafe\u{0301}"),
            ("STRASSE", "Stra\u{00DF}e"),
            ("laptop", "LAPTOP"),
            ("laptop", "1aptop"),
            // A Cyrillic К, which looks like K only as a capital.
            ("Kiosk", "\u{041A}iosk"),
            // A ligature, and full-width letters.
            ("file", "\u{FB01}le"),
            ("desk", "\u{FF44}\u{FF45}\u{FF53}\u{FF4B}"),
        ] {
            for (taken, wanted) in [(a, b), (b, a)] {
                let st = Store::open_in_memory().unwrap();
                inserted(&st, &device("dev_1", taken, None));
                assert_eq!(
                    st.insert_device(&device("dev_2", wanted, None), None)
                        .unwrap(),
                    Inserted::NameTaken,
                    "{wanted:?} beside {taken:?}"
                );
            }
        }

        // Names that merely share letters are still two names.
        let st = Store::open_in_memory().unwrap();
        for (i, name) in ["laptop", "laptops", "lapdog", "desk", "desk-2"]
            .into_iter()
            .enumerate()
        {
            inserted(&st, &device(&format!("dev_{i}"), name, None));
        }
    }

    #[test]
    fn a_name_is_stored_composed_and_trimmed() {
        assert_eq!(plain_name("  cafe\u{0301} "), "caf\u{00E9}");
        assert_eq!(plain_name("laptop"), "laptop");
    }

    #[test]
    fn revoking_keeps_the_first_time_and_the_row() {
        let st = Store::open_in_memory().unwrap();
        inserted(&st, &device("dev_1", "laptop", None));
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
            inserted(
                &st,
                &NewDevice {
                    ephemeral,
                    created_at: &ts(0),
                    ..device(id, id, None)
                },
            );
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

    fn key(st: &Store, max_devices: Option<u32>) {
        st.insert_authkey(&NewAuthkey {
            id: "ak_1",
            key_sha256: "abc",
            tag: "cloud",
            ephemeral: true,
            max_devices,
            created_at: &ts(0),
            expires_at: &ts(86400),
        })
        .unwrap();
    }

    #[test]
    fn authkeys_are_found_by_hash_and_revoked_once() {
        let st = Store::open_in_memory().unwrap();
        key(&st, None);
        let found = st.authkey_by_hash("abc").unwrap().unwrap();
        assert_eq!(
            (found.id.as_str(), found.ephemeral, found.max_devices),
            ("ak_1", true, None)
        );
        assert!(st.authkey_by_hash("abd").unwrap().is_none());
        let revoked = st.revoke_authkey("ak_1", &ts(1), false).unwrap().unwrap();
        assert_eq!(revoked.revoked_at, Some(ts(1)));
        assert_eq!(
            st.revoke_authkey("ak_1", &ts(2), false)
                .unwrap()
                .unwrap()
                .revoked_at,
            Some(ts(1))
        );
        assert_eq!(st.authkeys().unwrap().len(), 1);
    }

    /// A key's device cap counts the devices it enrolled that are still
    /// there and unrevoked, so a leaked key cannot mint them without end,
    /// and a legitimate one frees a place each time one goes.
    #[test]
    fn a_key_enrols_no_more_than_its_cap() {
        let st = Store::open_in_memory().unwrap();
        key(&st, Some(2));
        inserted(&st, &device("dev_1", "cloud-1", Some("ak_1")));
        assert!(matches!(
            st.insert_device(&device("dev_2", "cloud-2", Some("ak_1")), Some(2))
                .unwrap(),
            Inserted::Done(_)
        ));
        assert_eq!(
            st.insert_device(&device("dev_3", "cloud-3", Some("ak_1")), Some(2))
                .unwrap(),
            Inserted::KeyFull
        );
        st.revoke_device("dev_1", &ts(1)).unwrap();
        assert!(matches!(
            st.insert_device(&device("dev_3", "cloud-3", Some("ak_1")), Some(2))
                .unwrap(),
            Inserted::Done(_)
        ));
    }

    #[test]
    fn revoking_a_key_can_revoke_what_it_enrolled() {
        let st = Store::open_in_memory().unwrap();
        key(&st, None);
        inserted(&st, &device("dev_1", "cloud-1", Some("ak_1")));
        inserted(&st, &device("dev_2", "laptop", None));
        st.revoke_authkey("ak_1", &ts(1), true).unwrap();
        let revoked: Vec<(String, bool)> = st
            .devices()
            .unwrap()
            .into_iter()
            .map(|d| (d.id, d.revoked_at.is_some()))
            .collect();
        assert!(revoked.contains(&("dev_1".into(), true)));
        assert!(
            revoked.contains(&("dev_2".into(), false)),
            "only the key's own devices"
        );
    }
}
