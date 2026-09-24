//! The merge queue: jobs, their leases, and applying their results.
//!
//! Every function that decides something takes `now`, so tests can move
//! the clock rather than wait for it, and every one that changes more than
//! one row does it in one transaction under the store's lock: a job is
//! never claimed twice, and a merged file is never written without its job
//! recording it.

use std::time::Duration;

use anyhow::{Context, Result};
use recall_wire::jobs::{KIND_MERGE, STATE_DONE, STATE_FAILED, STATE_LEASED, STATE_QUEUED};
use recall_wire::{
    content_sha256, Job, JobSummary, MergeInput, MergeSide, QueueStatus, ResultRequest,
    ResultResponse,
};
use rusqlite::{Connection, OptionalExtension, Row};
use time::OffsetDateTime;

use super::{read_file, write_file, Outcome, Store};
use crate::audit::leaf;
use crate::format_timestamp;

/// Created with the other tables, every time the store opens.
///
/// `kind` has no `CHECK`: later kinds (sealing, evaluating) arrive without
/// rebuilding the table, which is what `devices.scope` needed.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS jobs (
        id               TEXT PRIMARY KEY,
        kind             TEXT NOT NULL,
        state            TEXT NOT NULL
                         CHECK (state IN ('queued', 'leased', 'done', 'failed')),
        project_key      TEXT NOT NULL,
        file_path        TEXT NOT NULL,
        -- The job's input as JSON: for a merge, both versions. Emptied to
        -- '{}' once the job is done, since the file itself then holds what
        -- mattered.
        payload          TEXT NOT NULL,
        -- The current lease while leased; afterwards the lease its result
        -- came under, which is what makes posting that result again a
        -- no-op rather than a 409. NULL once a lease ran out unanswered.
        lease_id         TEXT,
        lease_expires_at TEXT,
        -- How many times it has been handed out.
        attempt          INTEGER NOT NULL DEFAULT 0,
        -- Not handed out before this: the retry delay after a failure.
        not_before       TEXT NOT NULL,
        -- The job whose result this one merges again, and how far down
        -- that chain it is, from 0.
        parent_id        TEXT,
        link             INTEGER NOT NULL DEFAULT 0,
        error            TEXT,
        -- A result that was not applied, kept so it can be recovered: the
        -- merged version, as JSON.
        result           TEXT,
        applied          INTEGER NOT NULL DEFAULT 0,
        follow_up        TEXT,
        created_at       TEXT NOT NULL,
        updated_at       TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS jobs_by_state ON jobs (state, not_before, created_at);
";

/// How many jobs may wait or be held at once. A queue a worker is not
/// draining must not grow without end; past this, a stale push is stored
/// as last-write-wins, as a failed merge always was, and says so in
/// `/health`.
pub const MAX_OPEN_JOBS: usize = 1000;

/// How many times a job is handed out before it is marked failed: once,
/// then again 1, 5 and 30 minutes after each failed attempt.
pub const MAX_ATTEMPTS: u32 = 4;

/// How long a job waits after its first, second and third failed attempt.
const RETRY_AFTER: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
];

/// How many follow-ups one conflict may lead to. A file that changes
/// faster than it can be merged stops being chased here: the newest push
/// stands, and the last result waits in a failed job.
pub const MAX_LINKS: u32 = 3;

/// The most of an error a job keeps, in bytes. The error is the worker's to
/// write and a result may be megabytes; what went wrong fits in far less,
/// and the listing and the server's log need no more.
pub const MAX_ERROR_BYTES: usize = 500;

/// Why a job whose merge came back empty is retried rather than applied.
const EMPTY_RESULT: &str = "the merge came back empty from two versions that were not";

/// `s`, cut to at most `max` bytes, on a character boundary.
pub fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

const JOB_COLUMNS: &str = "id, kind, state, project_key, file_path, payload, lease_id, \
     lease_expires_at, attempt, parent_id, link, error, result, applied, follow_up, \
     created_at, updated_at";

/// A job row.
struct JobRow {
    id: String,
    kind: String,
    state: String,
    project_key: String,
    file_path: String,
    payload: String,
    lease_id: Option<String>,
    lease_expires_at: Option<String>,
    attempt: u32,
    link: u32,
    error: Option<String>,
    result: Option<String>,
    applied: bool,
    follow_up: Option<String>,
    created_at: String,
    updated_at: String,
}

fn row_from(r: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        kind: r.get(1)?,
        state: r.get(2)?,
        project_key: r.get(3)?,
        file_path: r.get(4)?,
        payload: r.get(5)?,
        lease_id: r.get(6)?,
        lease_expires_at: r.get(7)?,
        attempt: r.get(8)?,
        link: r.get(10)?,
        error: r.get(11)?,
        result: r.get(12)?,
        applied: r.get::<_, i64>(13)? != 0,
        follow_up: r.get(14)?,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

impl JobRow {
    fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id.clone(),
            kind: self.kind.clone(),
            state: self.state.clone(),
            project_key: self.project_key.clone(),
            file_path: self.file_path.clone(),
            attempt: self.attempt,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            error: self.error.clone(),
            follow_up: self.follow_up.clone(),
        }
    }

    fn outcome(&self) -> ResultResponse {
        ResultResponse {
            id: self.id.clone(),
            state: self.state.clone(),
            applied: self.applied,
            follow_up: self.follow_up.clone(),
        }
    }

    fn merge_input(&self) -> Result<MergeInput> {
        serde_json::from_str(&self.payload)
            .with_context(|| format!("job {} has a payload that does not read", self.id))
    }
}

fn get_job(conn: &Connection, id: &str) -> Result<Option<JobRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
            (id,),
            row_from,
        )
        .optional()?)
}

fn ts(at: OffsetDateTime) -> String {
    format_timestamp(at)
}

/// The version of a file a row holds, as a job carries it.
fn side_of(content: &str, source_env: &str, updated_at: &str) -> MergeSide {
    MergeSide {
        sha256: content_sha256(content),
        content: content.to_string(),
        source_env: source_env.to_string(),
        updated_at: updated_at.to_string(),
    }
}

/// A job that ran out of attempts, or whose result could not be applied.
///
/// It is said two ways, because `/health` answers anyone:
/// [`Failure::public`] names the job and nothing else, never a project or a
/// path, and [`Failure::logged`], for the server's own log, names the file
/// and the error as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The job.
    pub id: String,
    /// The project its file belongs to.
    pub project_key: String,
    /// The file.
    pub file_path: String,
    /// What became of it, naming no project and no file, such as `failed
    /// after 4 attempts`.
    pub what: String,
    /// The last error it met, at most [`MAX_ERROR_BYTES`]; empty for none.
    pub error: String,
}

impl Failure {
    /// What `/health` shows as the last merge error: the job, what became
    /// of it, and where to look.
    pub fn public(&self) -> String {
        format!(
            "merge job {} {}; see GET /v1/jobs?state=failed",
            self.id, self.what
        )
    }

    /// What the server's log says: the file and the error too.
    pub fn logged(&self) -> String {
        let mut line = format!(
            "merge job {} for {}/{} {}",
            self.id, self.project_key, self.file_path, self.what
        );
        if !self.error.is_empty() {
            line.push_str(": ");
            line.push_str(&self.error);
        }
        line
    }
}

/// What queueing a merge came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Queued {
    /// The incoming version is stored, and this job will merge it with the
    /// one it displaced.
    Queued(String),
    /// The incoming version is stored, and there was nothing to merge it
    /// with: the file was new, a tombstone, or already this content.
    Nothing,
    /// The incoming version is stored, and the queue was full, so no job
    /// was made: last-write-wins, as a failed merge.
    Full,
}

/// What a result came to, when it was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The answer for the worker.
    pub response: ResultResponse,
    /// Whether this call wrote the merged file.
    pub applied: bool,
    /// Whether this call queued something a claim may now take: a
    /// follow-up.
    pub queued: bool,
    /// When this call marked the job failed, the failure: what `/health`
    /// shows as the last merge error.
    pub failed: Option<Box<Failure>>,
}

/// What posting a result came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Recorded, or recorded already under this lease and left alone.
    Recorded(Settled),
    /// No job has that id.
    NotFound,
    /// The lease is not the job's current one, or has run out: someone
    /// else has, or will have, the job.
    LeaseEnded,
    /// A merge result for a job that is not a merge.
    WrongKind,
}

/// What retrying a job came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retried {
    /// Queued again.
    Queued(JobSummary),
    /// No job has that id.
    NotFound,
    /// It has not failed, so there is nothing to retry.
    NotFailed(String),
}

/// After a failed attempt: back in the queue after the delay for that
/// attempt, or failed for good. Answers the failure when it is final.
/// `error` is kept to [`MAX_ERROR_BYTES`].
fn after_failure(
    conn: &Connection,
    job: &JobRow,
    error: &str,
    now: OffsetDateTime,
    keep_lease: bool,
) -> Result<Option<Failure>> {
    let error = clip(error, MAX_ERROR_BYTES);
    let lease = if keep_lease {
        job.lease_id.clone()
    } else {
        None
    };
    let retry = (job.attempt as usize)
        .checked_sub(1)
        .and_then(|i| RETRY_AFTER.get(i))
        .filter(|_| job.attempt < MAX_ATTEMPTS);
    match retry {
        Some(delay) => {
            conn.execute(
                "UPDATE jobs SET state = 'queued', lease_id = ?2, lease_expires_at = NULL,
                     not_before = ?3, error = ?4, applied = 0, updated_at = ?5
                 WHERE id = ?1",
                (&job.id, &lease, ts(now + *delay), error, ts(now)),
            )?;
            Ok(None)
        }
        None => {
            let kept = format!("{error} (gave up after {} attempts)", job.attempt);
            conn.execute(
                "UPDATE jobs SET state = 'failed', lease_id = ?2, lease_expires_at = NULL,
                     error = ?3, applied = 0, updated_at = ?4
                 WHERE id = ?1",
                (&job.id, &lease, &kept, ts(now)),
            )?;
            Ok(Some(Failure {
                id: job.id.clone(),
                project_key: job.project_key.clone(),
                file_path: job.file_path.clone(),
                what: format!("failed after {} attempts", job.attempt),
                error: error.to_string(),
            }))
        }
    }
}

fn insert_merge_job(
    conn: &Connection,
    id: &str,
    input: &MergeInput,
    parent: Option<(&str, u32)>,
    now: OffsetDateTime,
) -> Result<()> {
    let now = ts(now);
    conn.execute(
        "INSERT INTO jobs (id, kind, state, project_key, file_path, payload, not_before,
                           parent_id, link, created_at, updated_at)
         VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, ?8, ?6, ?6)",
        (
            id,
            KIND_MERGE,
            &input.project_key,
            &input.file_path,
            serde_json::to_string(input)?,
            &now,
            parent.map(|(p, _)| p),
            parent.map_or(0, |(_, link)| link),
        ),
    )?;
    Ok(())
}

impl Store {
    /// Stores `incoming` as the file's content, last-write-wins, and queues
    /// a merge of it with whatever it displaced, in one transaction with
    /// the push's leaf, which `build_leaf` makes knowing what was queued:
    /// the job's `stored` side is read under the same lock as the write, so
    /// it is exactly the version this push replaced. Answers what was
    /// queued and the write's `updated_at`, which is its leaf's `at`, as
    /// every audited write's is; `incoming.updated_at` is not used.
    pub fn write_and_queue_merge_audited(
        &self,
        project_key: &str,
        file_path: &str,
        incoming: &MergeSide,
        job_id: &str,
        now: OffsetDateTime,
        build_leaf: impl FnOnce(u64, &str, &Queued) -> Vec<u8>,
    ) -> Result<(Queued, String)> {
        let mut stamped = None;
        let queued = self.audited(
            |tx, at| {
                stamped = Some(at.to_string());
                let incoming = MergeSide {
                    updated_at: at.to_string(),
                    ..incoming.clone()
                };
                let displaced = read_file(tx, project_key, file_path)?
                    .filter(|e| !e.deleted && e.content != incoming.content);
                write_file(
                    tx,
                    project_key,
                    file_path,
                    &incoming.content,
                    &incoming.source_env,
                    at,
                )?;
                let Some(displaced) = displaced else {
                    return Ok(Outcome::Commit(Queued::Nothing));
                };
                let open: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'leased')",
                    [],
                    |r| r.get(0),
                )?;
                if open as usize >= MAX_OPEN_JOBS {
                    return Ok(Outcome::Commit(Queued::Full));
                }
                let input = MergeInput {
                    project_key: project_key.to_string(),
                    file_path: file_path.to_string(),
                    stored: side_of(
                        &displaced.content,
                        &displaced.source_env,
                        &displaced.updated_at,
                    ),
                    incoming,
                };
                insert_merge_job(tx, job_id, &input, None, now)?;
                Ok(Outcome::Commit(Queued::Queued(job_id.to_string())))
            },
            build_leaf,
        )?;
        Ok((queued, stamped.context("the write ran")?))
    }

    /// Puts every job whose lease ran out back in the queue, or fails it
    /// when that was its last attempt. Answers each job it failed.
    ///
    /// Each job whose lease ran out gets a `job_result` leaf, which
    /// `build_leaf` makes, in the same transaction: the attempt ended with
    /// no result, and the job waits for another or has failed. A call that
    /// finds no lease run out changes nothing and appends nothing.
    pub fn expire_leases_audited(
        &self,
        now: OffsetDateTime,
        build_leaf: impl Fn(u64, &str, &leaf::JobChange<'_>) -> Vec<u8>,
    ) -> Result<Vec<Failure>> {
        let (failed, _) = self.audited_each(
            |tx, _| {
                let expired: Vec<JobRow> = {
                    let mut stmt = tx.prepare(&format!(
                        "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'leased' AND lease_expires_at <= ?1"
                    ))?;
                    let rows = stmt.query_map((ts(now),), row_from)?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                if expired.is_empty() {
                    return Ok(Outcome::Refuse((Vec::new(), Vec::new())));
                }
                let mut failed = Vec::new();
                let mut ended = Vec::new();
                for job in expired {
                    let failure = after_failure(
                        tx,
                        &job,
                        "the worker did not report back before its lease ended",
                        now,
                        false,
                    )?;
                    let state = if failure.is_some() {
                        STATE_FAILED
                    } else {
                        STATE_QUEUED
                    };
                    failed.extend(failure);
                    ended.push((job.id, job.project_key, job.file_path, state));
                }
                Ok(Outcome::Commit((failed, ended)))
            },
            |seq, at, (_, ended)| {
                ended
                    .iter()
                    .zip(seq..)
                    .map(|((job_id, project_key, file_path, state), seq)| {
                        build_leaf(
                            seq,
                            at,
                            &leaf::JobChange {
                                job_id,
                                project_key,
                                file_path,
                                state,
                                stored_sha256: None,
                                follow_up: None,
                            },
                        )
                    })
                    .collect()
            },
        )?;
        Ok(failed)
    }

    /// Leases the oldest queued job of one of `kinds` that may run now,
    /// for `lease` from `now`, under `lease_id`, and appends the
    /// `job_claim` leaf `build_leaf` makes in the same transaction. Finding
    /// nothing to lease changes nothing and appends nothing.
    pub fn claim_job_audited(
        &self,
        kinds: &[String],
        lease_id: &str,
        lease: Duration,
        now: OffsetDateTime,
        build_leaf: impl FnOnce(u64, &str, &Job) -> Vec<u8>,
    ) -> Result<Option<Job>> {
        if kinds.is_empty() {
            return Ok(None);
        }
        self.audited(
            |tx, _| {
                // Kinds come from the request, so they are bound, never
                // spliced.
                let marks = (0..kinds.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");
                let now_text = ts(now);
                let mut params: Vec<&dyn rusqlite::ToSql> = vec![&now_text];
                for kind in kinds {
                    params.push(kind);
                }
                let job = tx
                    .query_row(
                        &format!(
                            "SELECT {JOB_COLUMNS} FROM jobs
                             WHERE state = 'queued' AND not_before <= ?1 AND kind IN ({marks})
                             ORDER BY created_at, id LIMIT 1"
                        ),
                        params.as_slice(),
                        row_from,
                    )
                    .optional()?;
                let Some(job) = job else {
                    return Ok(Outcome::Refuse(None));
                };
                let expires = ts(now + lease);
                tx.execute(
                    "UPDATE jobs SET state = 'leased', lease_id = ?2, lease_expires_at = ?3,
                         attempt = attempt + 1, updated_at = ?4
                     WHERE id = ?1",
                    (&job.id, lease_id, &expires, &now_text),
                )?;
                let merge = match job.kind.as_str() {
                    KIND_MERGE => Some(job.merge_input()?),
                    _ => None,
                };
                Ok(Outcome::Commit(Some(Job {
                    id: job.id,
                    kind: job.kind,
                    lease_id: lease_id.to_string(),
                    lease_expires_at: expires,
                    attempt: job.attempt + 1,
                    merge,
                })))
            },
            |seq, at, job| build_leaf(seq, at, job.as_ref().expect("a leaf only for a lease")),
        )
    }

    /// Records a worker's result for job `id`, with the `job_result` leaf
    /// `build_leaf` makes from what it changed, in one transaction.
    ///
    /// A result counts only under the job's current, unexpired lease. The
    /// same result posted again under the lease that settled it changes
    /// nothing, answers as the first did, and appends nothing.
    ///
    /// A merge is applied as a compare-and-swap: only if the file still
    /// has the hash of the version the job stored, and then attributed to
    /// `worker`. If another push landed meanwhile, the merged content
    /// becomes the `stored` side of a follow-up job, `follow_up_id`,
    /// against the newer version, at most [`MAX_LINKS`] deep. An empty
    /// merge of two versions that were not empty is never applied: it is
    /// taken as an error, and retried.
    pub fn settle_job_audited(
        &self,
        id: &str,
        result: &ResultRequest,
        worker: &str,
        follow_up_id: &str,
        now: OffsetDateTime,
        build_leaf: impl FnOnce(u64, &str, &leaf::JobChange<'_>) -> Vec<u8>,
    ) -> Result<Settlement> {
        let (settlement, _) = self.audited(
            |tx, _| {
                let refuse = |s: Settlement| Ok(Outcome::Refuse((s, None)));
                let Some(job) = get_job(tx, id)? else {
                    return refuse(Settlement::NotFound);
                };
                if job.lease_id.as_deref() != Some(result.lease_id.as_str()) {
                    return refuse(Settlement::LeaseEnded);
                }
                if job.state != STATE_LEASED {
                    // Settled already, under this very lease: the repeat
                    // of a result that was recorded.
                    return refuse(Settlement::Recorded(Settled {
                        response: job.outcome(),
                        applied: false,
                        queued: false,
                        failed: None,
                    }));
                }
                if job
                    .lease_expires_at
                    .as_deref()
                    .is_none_or(|at| at <= ts(now).as_str())
                {
                    return refuse(Settlement::LeaseEnded);
                }

                let mut stored = None;
                let settled = match (&result.merge, &result.error) {
                    (_, Some(error)) => {
                        let failed = after_failure(tx, &job, error, now, true)?.map(Box::new);
                        Settled {
                            response: ResultResponse::default(),
                            applied: false,
                            queued: false,
                            failed,
                        }
                    }
                    (Some(merged), None) => {
                        if job.kind != KIND_MERGE {
                            return refuse(Settlement::WrongKind);
                        }
                        let input = job.merge_input()?;
                        // Nothing from two versions that had something is
                        // not a merge but a malfunction, as the inline merge
                        // treats it: retried like an error, and never
                        // written, where it would replace both machines'
                        // notes with an empty file.
                        let emptied = merged.content.trim().is_empty()
                            && !(input.stored.content.trim().is_empty()
                                && input.incoming.content.trim().is_empty());
                        if emptied {
                            Settled {
                                response: ResultResponse::default(),
                                applied: false,
                                queued: false,
                                failed: after_failure(tx, &job, EMPTY_RESULT, now, true)?
                                    .map(Box::new),
                            }
                        } else {
                            let settled = apply_merge(
                                tx,
                                &job,
                                &input,
                                &merged.content,
                                worker,
                                follow_up_id,
                                now,
                            )?;
                            if settled.applied {
                                stored = Some(content_sha256(&merged.content));
                            }
                            settled
                        }
                    }
                    (None, None) => anyhow::bail!("a result carries a merge or an error"),
                };
                let response = get_job(tx, id)?
                    .context("the job was read above")?
                    .outcome();
                let change = (job.project_key, job.file_path, stored);
                Ok(Outcome::Commit((
                    Settlement::Recorded(Settled {
                        response,
                        ..settled
                    }),
                    Some(change),
                )))
            },
            |seq, at, (settlement, change)| {
                let (Settlement::Recorded(s), Some((project_key, file_path, stored))) =
                    (settlement, change)
                else {
                    unreachable!("a leaf only for a recorded result");
                };
                build_leaf(
                    seq,
                    at,
                    &leaf::JobChange {
                        job_id: &s.response.id,
                        project_key,
                        file_path,
                        state: &s.response.state,
                        stored_sha256: stored.as_deref(),
                        follow_up: s.response.follow_up.as_deref(),
                    },
                )
            },
        )?;
        Ok(settlement)
    }

    /// Jobs, newest first, at most `limit`, in `state` when given.
    pub fn jobs(&self, state: Option<&str>, limit: usize) -> Result<Vec<JobSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE ?1 IS NULL OR state = ?1
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map((state, limit as i64), row_from)?;
        Ok(rows
            .map(|r| r.map(|job| job.summary()))
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Queues a failed job again, with its attempts counted afresh, and
    /// appends the `job_retry` leaf `build_leaf` makes in the same
    /// transaction.
    ///
    /// A job that failed because the file kept changing kept its result:
    /// it is queued as a merge of that result with the file as it is now,
    /// which is the merge that was never finished.
    pub fn retry_job_audited(
        &self,
        id: &str,
        now: OffsetDateTime,
        build_leaf: impl FnOnce(u64, &str, &JobSummary) -> Vec<u8>,
    ) -> Result<Retried> {
        self.audited(
            |tx, _| {
                let Some(job) = get_job(tx, id)? else {
                    return Ok(Outcome::Refuse(Retried::NotFound));
                };
                if job.state != STATE_FAILED {
                    return Ok(Outcome::Refuse(Retried::NotFailed(job.state)));
                }
                let mut payload = job.payload.clone();
                if let (Some(kept), KIND_MERGE) = (&job.result, job.kind.as_str()) {
                    let kept: MergeSide = serde_json::from_str(kept)?;
                    if let Some(file) =
                        read_file(tx, &job.project_key, &job.file_path)?.filter(|e| !e.deleted)
                    {
                        payload = serde_json::to_string(&MergeInput {
                            project_key: job.project_key.clone(),
                            file_path: job.file_path.clone(),
                            stored: kept,
                            incoming: side_of(&file.content, &file.source_env, &file.updated_at),
                        })?;
                    }
                }
                let now = ts(now);
                tx.execute(
                    "UPDATE jobs SET state = 'queued', payload = ?2, lease_id = NULL,
                         lease_expires_at = NULL, attempt = 0, not_before = ?3, link = 0,
                         error = NULL, result = NULL, applied = 0, follow_up = NULL, updated_at = ?3
                     WHERE id = ?1",
                    (id, &payload, &now),
                )?;
                let job = get_job(tx, id)?.context("the job was read above")?;
                Ok(Outcome::Commit(Retried::Queued(job.summary())))
            },
            |seq, at, retried| match retried {
                Retried::Queued(job) => build_leaf(seq, at, job),
                _ => unreachable!("a leaf only for a retry"),
            },
        )
    }

    /// Makes every open job claimable at once: a leased one is released,
    /// its holder being gone, and a queued one's retry delay is dropped.
    /// Answers how many there are.
    ///
    /// For the server draining the queue itself once no worker is left to:
    /// a revoked worker's lease would otherwise hold its job until it ran
    /// out, and a job waiting out a delay the worker's failure set has no
    /// worker left to wait for. Attempts still count, so a job that keeps
    /// failing here is failed all the same.
    ///
    /// Not in the audit log: it changes no file, and what it frees is
    /// recorded around it — the `revoke` of the worker that held a lease
    /// before, and the server's own `job_claim` of each job after.
    pub fn release_open_jobs(&self, now: OffsetDateTime) -> Result<usize> {
        Ok(self.lock().execute(
            "UPDATE jobs SET state = 'queued', lease_id = NULL, lease_expires_at = NULL,
                 not_before = ?1
             WHERE state IN ('queued', 'leased')",
            (ts(now),),
        )?)
    }

    /// Marks every open job failed, with `why` as its error, for a queue
    /// nothing is left to drain. Each keeps its input, so a retry merges it
    /// once something can. Answers their ids.
    ///
    /// Each job gets a `job_result` leaf, which `build_leaf` makes, in the
    /// same transaction, as a lease that ran out on a last attempt does.
    pub fn fail_open_jobs_audited(
        &self,
        why: &str,
        now: OffsetDateTime,
        build_leaf: impl Fn(u64, &str, &leaf::JobChange<'_>) -> Vec<u8>,
    ) -> Result<Vec<String>> {
        let jobs = self.audited_each(
            |tx, _| {
                let jobs = {
                    let mut stmt = tx.prepare(
                        "SELECT id, project_key, file_path FROM jobs
                         WHERE state IN ('queued', 'leased') ORDER BY created_at, id",
                    )?;
                    let rows = stmt.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                if jobs.is_empty() {
                    return Ok(Outcome::Refuse(jobs));
                }
                tx.execute(
                    "UPDATE jobs SET state = 'failed', lease_id = NULL, lease_expires_at = NULL,
                         error = ?1, applied = 0, updated_at = ?2
                     WHERE state IN ('queued', 'leased')",
                    (clip(why, MAX_ERROR_BYTES), ts(now)),
                )?;
                Ok(Outcome::Commit(jobs))
            },
            |seq, at, jobs| {
                jobs.iter()
                    .zip(seq..)
                    .map(|((job_id, project_key, file_path), seq)| {
                        build_leaf(
                            seq,
                            at,
                            &leaf::JobChange {
                                job_id,
                                project_key,
                                file_path,
                                state: STATE_FAILED,
                                stored_sha256: None,
                                follow_up: None,
                            },
                        )
                    })
                    .collect()
            },
        )?;
        Ok(jobs.into_iter().map(|(id, _, _)| id).collect())
    }

    /// What `/health` says about the queue.
    pub fn queue_status(&self) -> Result<QueueStatus> {
        let conn = self.lock();
        let count = |state: &str| -> Result<u64> {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM jobs WHERE state = ?1",
                (state,),
                |r| r.get(0),
            )?;
            Ok(n as u64)
        };
        Ok(QueueStatus {
            queued: count(STATE_QUEUED)?,
            leased: count(STATE_LEASED)?,
            failed: count(STATE_FAILED)?,
            oldest_queued_at: conn.query_row(
                "SELECT MIN(created_at) FROM jobs WHERE state = 'queued'",
                [],
                |r| r.get(0),
            )?,
        })
    }

    /// Removes jobs that finished before `before`. Failed jobs stay until
    /// someone retries them: each may hold a result nobody has seen.
    ///
    /// Not in the audit log: the leaves of the jobs removed stay in it.
    pub fn prune_jobs(&self, before: &str) -> Result<usize> {
        Ok(self.lock().execute(
            "DELETE FROM jobs WHERE state = 'done' AND updated_at < ?1",
            (before,),
        )?)
    }
}

/// Applies a merged result to its file, as a compare-and-swap: written
/// only if the file still has the hash of the version the job stored, and
/// then attributed to `worker`. If another push landed meanwhile, the
/// merged content becomes the `stored` side of a follow-up job,
/// `follow_up_id`, against the newer version, at most [`MAX_LINKS`] deep.
#[allow(clippy::too_many_arguments)]
fn apply_merge(
    tx: &Connection,
    job: &JobRow,
    input: &MergeInput,
    merged: &str,
    worker: &str,
    follow_up_id: &str,
    now: OffsetDateTime,
) -> Result<Settled> {
    let now_text = ts(now);
    let merged_side = side_of(merged, worker, &now_text);
    let current = read_file(tx, &input.project_key, &input.file_path)?.filter(|e| !e.deleted);
    let settled = |applied, queued, failed| Settled {
        response: ResultResponse::default(),
        applied,
        queued,
        failed,
    };
    Ok(match current {
        // The compare-and-swap: nothing landed since the push that queued
        // this, so the merge replaces it.
        Some(file) if content_sha256(&file.content) == input.incoming.sha256 => {
            write_file(
                tx,
                &input.project_key,
                &input.file_path,
                merged,
                worker,
                &now_text,
            )?;
            finish(tx, job, STATE_DONE, true, None, None, None, now)?;
            settled(true, false, None)
        }
        // Something else landed meanwhile, and says exactly what the
        // merge came to.
        Some(file) if file.content == merged => {
            finish(tx, job, STATE_DONE, true, None, None, None, now)?;
            settled(false, false, None)
        }
        // Something else landed: merge the result with it, unless this
        // conflict has been chased far enough.
        Some(file) if job.link < MAX_LINKS => {
            let next = MergeInput {
                project_key: input.project_key.clone(),
                file_path: input.file_path.clone(),
                stored: merged_side,
                incoming: side_of(&file.content, &file.source_env, &file.updated_at),
            };
            insert_merge_job(tx, follow_up_id, &next, Some((&job.id, job.link + 1)), now)?;
            finish(
                tx,
                job,
                STATE_DONE,
                false,
                None,
                None,
                Some(follow_up_id),
                now,
            )?;
            settled(false, true, None)
        }
        Some(_) => {
            let error = "the file kept changing while it was merged; the newest push stands, \
                         and this result is kept in the job";
            finish(
                tx,
                job,
                STATE_FAILED,
                false,
                Some(error),
                Some(&merged_side),
                None,
                now,
            )?;
            settled(
                false,
                false,
                Some(Box::new(Failure {
                    id: job.id.clone(),
                    project_key: job.project_key.clone(),
                    file_path: job.file_path.clone(),
                    what: "was not applied: the file kept changing while it was merged".to_string(),
                    error: String::new(),
                })),
            )
        }
        // Deleted meanwhile: the delete said to discard it, as a push after
        // a delete is never merged either. The result is kept in the job
        // all the same. A delete closes its file's open jobs as it lands
        // (see [`close_for_delete`]), so this is a job retried after one.
        None => {
            finish(
                tx,
                job,
                STATE_DONE,
                false,
                Some("the file was deleted while it was merged; the delete stands"),
                Some(&merged_side),
                None,
                now,
            )?;
            settled(false, false, None)
        }
    })
}

/// Closes every merge job still waiting on, or held for, one file, as the
/// delete of that file lands, in the delete's transaction.
///
/// A delete says to discard the file, and a push after a delete is never
/// merged with what was there. A job left open would say otherwise: its
/// result, landing on the file a later push created, would not match the
/// hash it was queued against, and would be chased onto the new file as a
/// follow-up, bringing the deleted notes back into it. Closed now, it is
/// `done` and unapplied, and a result its holder still posts is answered as
/// one already recorded, changing nothing.
pub(super) fn close_for_delete(
    conn: &Connection,
    project_key: &str,
    file_path: &str,
    now: &str,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE jobs SET state = 'done', payload = '{}', lease_expires_at = NULL, applied = 0,
             error = ?3, updated_at = ?4
         WHERE kind = 'merge' AND project_key = ?1 AND file_path = ?2
           AND state IN ('queued', 'leased')",
        (
            project_key,
            file_path,
            "the file was deleted before it was merged; the delete stands",
            now,
        ),
    )?)
}

/// Why `recall-server admin` closes a job, which decides what it says and
/// what it keeps. See [`close_for_admin`].
pub(super) enum AdminClose<'a> {
    /// `remove`: the file's key is gone.
    Removed,
    /// `rename`: the file is under `to` now.
    Renamed {
        /// The key the rows moved to.
        to: &'a str,
    },
    /// `restore`: the file holds the backup's version now.
    Restored,
}

/// Closes every job still waiting on, or held for, the rows an admin change
/// touches, in the change's own transaction: every file under
/// `project_key`, or only `file_path` when one is given. Answers how many
/// it closed.
///
/// Closed as a delete closes them ([`close_for_delete`]): `done`,
/// unapplied, the lease kept, so a result its holder still posts is
/// answered as one already recorded and changes nothing. Left open, each
/// would be settled against a row the change moved, removed or replaced:
///
/// - after a remove, its result would be chased onto a file a machine still
///   syncing under the key pushes later, as a follow-up, bringing the
///   removed notes back into it;
/// - after a restore, the file no longer has the hash the job was queued
///   against, so its result, a merge of the versions the restore replaced,
///   would be chased onto the restored file as a follow-up, and undo the
///   restore once that is merged;
/// - after a rename, the job still names the old key, and its versions are
///   the old key's rows.
///
/// A rename could instead move a job to the new key. It does not, because
/// what makes a result safe to apply is the compare-and-swap against the
/// version the job was queued with, under the key it was queued with, and a
/// rename is the owner saying the old key's history ends here: a job
/// re-keyed would be applied to a key no push to it ever queued, on the
/// strength of a hash taken under another, and one whose file has moved on
/// since (a push after it, a result applied) would be chased across keys.
/// Closing costs one merge, and nothing is lost: the file keeps the newer
/// version the push stored, the version it displaced stays in the job
/// (a rename and a restore keep the job's input; only a remove drops it,
/// as a delete does), and the backup every change takes holds both.
pub(super) fn close_for_admin(
    conn: &Connection,
    project_key: &str,
    file_path: Option<&str>,
    why: &AdminClose<'_>,
    now: &str,
) -> Result<usize> {
    let (error, keep_input) = match why {
        AdminClose::Removed => (
            "removed by admin: `recall-server admin remove` deleted this file's project key \
             before it was merged; the remove stands"
                .to_string(),
            false,
        ),
        AdminClose::Renamed { to } => (
            format!(
                "renamed by admin: `recall-server admin rename` moved this file to {to:?} before \
                 it was merged; its versions are kept in this job"
            ),
            true,
        ),
        AdminClose::Restored => (
            "restored by admin: `recall-server admin restore` put back a backup's version of this \
             file before it was merged; the restore stands, and the versions this job held are \
             kept in it"
                .to_string(),
            true,
        ),
    };
    Ok(conn.execute(
        "UPDATE jobs SET state = 'done',
             payload = CASE WHEN ?4 THEN payload ELSE '{}' END,
             lease_expires_at = NULL, applied = 0, error = ?3, updated_at = ?5
         WHERE project_key = ?1 AND (?2 IS NULL OR file_path = ?2)
           AND state IN ('queued', 'leased')",
        (
            project_key,
            file_path,
            clip(&error, MAX_ERROR_BYTES),
            keep_input,
            now,
        ),
    )?)
}

/// Marks a job settled, keeping the lease it was settled under so the same
/// result posted again is recognised. A done job's input is dropped: the
/// file holds what mattered now, and the job need not hold a second copy.
#[allow(clippy::too_many_arguments)]
fn finish(
    conn: &Connection,
    job: &JobRow,
    state: &str,
    applied: bool,
    error: Option<&str>,
    kept: Option<&MergeSide>,
    follow_up: Option<&str>,
    now: OffsetDateTime,
) -> Result<()> {
    let kept = kept.map(serde_json::to_string).transpose()?;
    let payload = if state == STATE_DONE {
        "{}".to_string()
    } else {
        job.payload.clone()
    };
    conn.execute(
        "UPDATE jobs SET state = ?2, applied = ?3, error = ?4, result = ?5, follow_up = ?6,
             payload = ?7, lease_expires_at = NULL, updated_at = ?8
         WHERE id = ?1",
        (
            &job.id,
            state,
            applied as i64,
            error,
            kept,
            follow_up,
            payload,
            ts(now),
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_leaf;
    use recall_wire::MergeResult;

    /// The audited writes, with a leaf these tests do not look at, under
    /// the names the tests read best with. A file written here keeps the
    /// `updated_at` a test gives it, since these tests move the clock.
    impl Store {
        fn upsert(&self, pk: &str, fp: &str, content: &str, env: &str, at: &str) -> Result<()> {
            self.audited(
                |tx, _| {
                    write_file(tx, pk, fp, content, env, at)?;
                    Ok(Outcome::Commit(()))
                },
                |seq, at, ()| test_leaf(seq, at),
            )
        }

        fn tombstone(&self, pk: &str, fp: &str, env: &str, at: &str) -> Result<()> {
            self.audited(
                |tx, _| {
                    tx.execute(
                        "INSERT INTO memory_files
                             (project_key, file_path, content, source_env, updated_at, deleted)
                         VALUES (?1, ?2, '', ?3, ?4, 1)
                         ON CONFLICT(project_key, file_path) DO UPDATE SET
                             source_env = excluded.source_env,
                             updated_at = excluded.updated_at,
                             deleted = 1",
                        (pk, fp, env, at),
                    )?;
                    close_for_delete(tx, pk, fp, at)?;
                    Ok(Outcome::Commit(()))
                },
                |seq, at, ()| test_leaf(seq, at),
            )
        }

        fn write_and_queue_merge(
            &self,
            pk: &str,
            fp: &str,
            incoming: &MergeSide,
            job_id: &str,
            now: OffsetDateTime,
        ) -> Result<Queued> {
            self.write_and_queue_merge_audited(pk, fp, incoming, job_id, now, |seq, at, _| {
                test_leaf(seq, at)
            })
            .map(|(queued, _)| queued)
        }

        fn claim_job(
            &self,
            kinds: &[String],
            lease_id: &str,
            lease: Duration,
            now: OffsetDateTime,
        ) -> Result<Option<Job>> {
            self.claim_job_audited(kinds, lease_id, lease, now, |seq, at, _| test_leaf(seq, at))
        }

        fn settle_job(
            &self,
            id: &str,
            result: &ResultRequest,
            worker: &str,
            follow_up_id: &str,
            now: OffsetDateTime,
        ) -> Result<Settlement> {
            self.settle_job_audited(id, result, worker, follow_up_id, now, |seq, at, _| {
                test_leaf(seq, at)
            })
        }

        fn retry_job(&self, id: &str, now: OffsetDateTime) -> Result<Retried> {
            self.retry_job_audited(id, now, |seq, at, _| test_leaf(seq, at))
        }

        fn expire_leases(&self, now: OffsetDateTime) -> Result<Vec<Failure>> {
            self.expire_leases_audited(now, |seq, at, _| test_leaf(seq, at))
        }

        fn fail_open_jobs(&self, why: &str, now: OffsetDateTime) -> Result<Vec<String>> {
            self.fail_open_jobs_audited(why, now, |seq, at, _| test_leaf(seq, at))
        }
    }

    const P: &str = "acme/app";
    const F: &str = "topics/auth.md";

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_790_000_000 + secs).unwrap()
    }

    fn side(content: &str, by: &str, secs: i64) -> MergeSide {
        side_of(content, by, &ts(at(secs)))
    }

    fn merge_kinds() -> Vec<String> {
        vec![KIND_MERGE.to_string()]
    }

    fn merged(lease: &str, content: &str) -> ResultRequest {
        ResultRequest {
            lease_id: lease.into(),
            merge: Some(MergeResult {
                content: content.into(),
            }),
            error: None,
        }
    }

    fn failed(lease: &str, why: &str) -> ResultRequest {
        ResultRequest {
            lease_id: lease.into(),
            merge: None,
            error: Some(why.into()),
        }
    }

    fn recorded(s: Settlement) -> Settled {
        match s {
            Settlement::Recorded(s) => s,
            other => panic!("not recorded: {other:?}"),
        }
    }

    /// A store holding `A` for the file, then a push of `B` queued as job
    /// `job_1`.
    fn conflicted() -> Store {
        let st = Store::open_in_memory().unwrap();
        st.upsert(P, F, "A", "laptop", &ts(at(0))).unwrap();
        assert_eq!(
            st.write_and_queue_merge(P, F, &side("B", "cloud", 1), "job_1", at(1))
                .unwrap(),
            Queued::Queued("job_1".into())
        );
        st
    }

    fn content(st: &Store) -> (String, String) {
        let e = st.get(P, F).unwrap().unwrap();
        (e.content, e.source_env)
    }

    #[test]
    fn a_stale_push_is_stored_at_once_and_queued_with_both_versions() {
        let st = conflicted();
        assert_eq!(content(&st), ("B".into(), "cloud".into()));
        let job = st
            .claim_job(&merge_kinds(), "lse_1", Duration::from_secs(120), at(2))
            .unwrap()
            .unwrap();
        assert_eq!((job.id.as_str(), job.attempt), ("job_1", 1));
        assert_eq!(job.lease_expires_at, ts(at(122)));
        let m = job.merge.unwrap();
        assert_eq!(
            (m.stored.content.as_str(), m.stored.source_env.as_str()),
            ("A", "laptop")
        );
        assert_eq!(m.stored.sha256, content_sha256("A"));
        // Stamped as the file it stored was: with its leaf's `at`.
        let stored = st.get(P, F).unwrap().unwrap();
        assert_eq!(
            m.incoming,
            MergeSide {
                updated_at: stored.updated_at,
                ..side("B", "cloud", 1)
            }
        );
        // Leased: nobody else gets it.
        assert!(st
            .claim_job(&merge_kinds(), "lse_2", Duration::from_secs(120), at(3))
            .unwrap()
            .is_none());
    }

    #[test]
    fn nothing_is_queued_without_something_to_merge() {
        let st = Store::open_in_memory().unwrap();
        assert_eq!(
            st.write_and_queue_merge(P, F, &side("B", "cloud", 1), "job_1", at(1))
                .unwrap(),
            Queued::Nothing
        );
        assert_eq!(content(&st).0, "B");
        assert!(st.jobs(None, 10).unwrap().is_empty());
    }

    #[test]
    fn a_claim_asks_for_kinds_and_takes_the_oldest() {
        let st = conflicted();
        st.upsert(P, "other.md", "X", "laptop", &ts(at(0))).unwrap();
        st.write_and_queue_merge(P, "other.md", &side("Y", "cloud", 5), "job_0", at(5))
            .unwrap();
        assert!(st
            .claim_job(&[], "lse", Duration::from_secs(60), at(6))
            .unwrap()
            .is_none());
        assert!(st
            .claim_job(&["seal".into()], "lse", Duration::from_secs(60), at(6))
            .unwrap()
            .is_none());
        let first = st
            .claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(6))
            .unwrap()
            .unwrap();
        assert_eq!(
            first.id, "job_1",
            "created first, though its id sorts later"
        );
    }

    /// The property the lease exists for: a job a worker took and never
    /// finished comes back, and the old holder's result no longer counts.
    #[test]
    fn an_expired_lease_is_released_and_its_holder_fenced_off() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse_old", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        // Still inside the lease: nothing to release.
        assert!(st.expire_leases(at(61)).unwrap().is_empty());
        assert_eq!(st.queue_status().unwrap().leased, 1);
        // Past it: queued again, for a minute from now.
        assert!(st.expire_leases(at(62)).unwrap().is_empty());
        let q = st.queue_status().unwrap();
        assert_eq!((q.queued, q.leased), (1, 0));
        assert!(st
            .claim_job(&merge_kinds(), "lse_new", Duration::from_secs(60), at(100))
            .unwrap()
            .is_none());
        let again = st
            .claim_job(&merge_kinds(), "lse_new", Duration::from_secs(60), at(122))
            .unwrap()
            .unwrap();
        assert_eq!(again.attempt, 2);
        // The first holder reports late: refused, and nothing changes.
        assert_eq!(
            st.settle_job(
                "job_1",
                &merged("lse_old", "late"),
                "worker",
                "job_f",
                at(123)
            )
            .unwrap(),
            Settlement::LeaseEnded
        );
        assert_eq!(content(&st).0, "B");
        // The current holder's result counts.
        let s = recorded(
            st.settle_job(
                "job_1",
                &merged("lse_new", "AB"),
                "worker",
                "job_f",
                at(124),
            )
            .unwrap(),
        );
        assert!(s.applied && s.response.applied);
        assert_eq!(content(&st), ("AB".into(), "worker".into()));
    }

    /// A lease that ran out is over even before a sweep has noticed.
    #[test]
    fn a_result_after_the_lease_ends_is_refused_even_unswept() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse_1", Duration::from_secs(60), at(2))
            .unwrap();
        assert_eq!(
            st.settle_job("job_1", &merged("lse_1", "AB"), "worker", "job_f", at(62))
                .unwrap(),
            Settlement::LeaseEnded
        );
        assert_eq!(content(&st).0, "B");
    }

    #[test]
    fn a_result_is_applied_once_and_a_repeat_changes_nothing() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse_1", Duration::from_secs(60), at(2))
            .unwrap();
        let first = recorded(
            st.settle_job("job_1", &merged("lse_1", "AB"), "worker", "job_f", at(3))
                .unwrap(),
        );
        assert_eq!(
            first.response,
            ResultResponse {
                id: "job_1".into(),
                state: "done".into(),
                applied: true,
                follow_up: None
            }
        );
        // Someone pushes after the merge; the repeat must not undo it.
        st.upsert(P, F, "C", "laptop", &ts(at(4))).unwrap();
        let again = recorded(
            st.settle_job("job_1", &merged("lse_1", "AB"), "worker", "job_g", at(5))
                .unwrap(),
        );
        assert_eq!(again.response, first.response);
        assert!(!again.applied);
        assert_eq!(content(&st).0, "C");
        assert_eq!(st.jobs(None, 10).unwrap().len(), 1, "no follow-up either");
    }

    /// The compare-and-swap: a push that landed while the job ran is not
    /// overwritten, and the merge is chased onto it instead.
    #[test]
    fn a_result_for_a_file_that_moved_on_becomes_a_follow_up() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse_1", Duration::from_secs(60), at(2))
            .unwrap();
        st.upsert(P, F, "C", "laptop", &ts(at(3))).unwrap();
        let s = recorded(
            st.settle_job("job_1", &merged("lse_1", "AB"), "worker", "job_2", at(4))
                .unwrap(),
        );
        assert_eq!(
            (s.applied, s.queued, s.response.follow_up.as_deref()),
            (false, true, Some("job_2"))
        );
        assert_eq!(s.response.state, "done");
        assert_eq!(content(&st).0, "C", "the push in between stands");
        let next = st
            .claim_job(&merge_kinds(), "lse_2", Duration::from_secs(60), at(5))
            .unwrap()
            .unwrap();
        let m = next.merge.unwrap();
        assert_eq!(
            (m.stored.content.as_str(), m.stored.source_env.as_str()),
            ("AB", "worker")
        );
        assert_eq!(m.incoming.content, "C");
        recorded(
            st.settle_job("job_2", &merged("lse_2", "ABC"), "worker", "job_3", at(6))
                .unwrap(),
        );
        assert_eq!(content(&st).0, "ABC");
    }

    #[test]
    fn a_chase_stops_after_three_links_and_keeps_the_result() {
        let st = conflicted();
        let mut id = "job_1".to_string();
        for link in 0..=MAX_LINKS {
            let t = 10 * (i64::from(link) + 1);
            let lease = format!("lse_{link}");
            let job = st
                .claim_job(&merge_kinds(), &lease, Duration::from_secs(60), at(t))
                .unwrap()
                .unwrap();
            assert_eq!(job.id, id);
            st.upsert(P, F, &format!("push {link}"), "laptop", &ts(at(t + 1)))
                .unwrap();
            let next = format!("job_next_{link}");
            let s = recorded(
                st.settle_job(&id, &merged(&lease, "merged"), "worker", &next, at(t + 2))
                    .unwrap(),
            );
            if link < MAX_LINKS {
                assert_eq!(s.response.follow_up.as_deref(), Some(next.as_str()));
                id = next;
            } else {
                assert_eq!(s.response.state, "failed");
                assert!(s.failed.is_some());
            }
        }
        assert_eq!(content(&st).0, format!("push {MAX_LINKS}"));
        // Retrying merges the kept result with the file as it is now.
        let Retried::Queued(job) = st.retry_job(&id, at(50)).unwrap() else {
            panic!("not retried")
        };
        assert_eq!((job.state.as_str(), job.attempt), ("queued", 0));
        let m = st
            .claim_job(&merge_kinds(), "lse_r", Duration::from_secs(60), at(51))
            .unwrap()
            .unwrap()
            .merge
            .unwrap();
        assert_eq!(
            (m.stored.content.as_str(), m.incoming.content.as_str()),
            ("merged", "push 3")
        );
    }

    #[test]
    fn errors_retry_after_one_five_and_thirty_minutes_then_fail() {
        let st = conflicted();
        let mut t = 2;
        for (attempt, wait) in [(1, 60), (2, 300), (3, 1800)] {
            let lease = format!("lse_{attempt}");
            let job = st
                .claim_job(&merge_kinds(), &lease, Duration::from_secs(60), at(t))
                .unwrap()
                .unwrap();
            assert_eq!(job.attempt, attempt);
            let s = recorded(
                st.settle_job(
                    "job_1",
                    &failed(&lease, "claude timed out"),
                    "w",
                    "j",
                    at(t),
                )
                .unwrap(),
            );
            assert_eq!(s.response.state, "queued");
            assert!(s.failed.is_none());
            // A repeat of the same error is the same answer, and no change.
            assert_eq!(
                recorded(
                    st.settle_job(
                        "job_1",
                        &failed(&lease, "claude timed out"),
                        "w",
                        "j",
                        at(t)
                    )
                    .unwrap()
                )
                .response
                .state,
                "queued"
            );
            assert!(st
                .claim_job(
                    &merge_kinds(),
                    "x",
                    Duration::from_secs(60),
                    at(t + wait - 1)
                )
                .unwrap()
                .is_none());
            t += wait;
        }
        st.claim_job(&merge_kinds(), "lse_4", Duration::from_secs(60), at(t))
            .unwrap()
            .unwrap();
        let s = recorded(
            st.settle_job(
                "job_1",
                &failed("lse_4", "claude timed out"),
                "w",
                "j",
                at(t),
            )
            .unwrap(),
        );
        assert_eq!(s.response.state, "failed");
        let failure = s.failed.unwrap();
        assert!(failure.error.contains("claude timed out"));
        assert_eq!(failure.what, "failed after 4 attempts");
        assert_eq!(content(&st).0, "B", "last-write-wins stands");
        let listed = st.jobs(Some("failed"), 10).unwrap();
        assert_eq!(listed[0].attempt, 4);
        assert!(listed[0]
            .error
            .as_deref()
            .unwrap()
            .contains("gave up after 4"));
    }

    #[test]
    fn a_lease_that_runs_out_on_the_last_attempt_fails_the_job() {
        let st = conflicted();
        st.lock()
            .execute("UPDATE jobs SET attempt = 3", [])
            .unwrap();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        let failed = st.expire_leases(at(100)).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(st.queue_status().unwrap().failed, 1);
    }

    #[test]
    fn a_delete_meanwhile_stands() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap();
        st.tombstone(P, F, "laptop", &ts(at(3))).unwrap();
        let s = recorded(
            st.settle_job("job_1", &merged("lse", "AB"), "worker", "job_2", at(4))
                .unwrap(),
        );
        assert_eq!((s.response.state.as_str(), s.applied), ("done", false));
        assert!(st.get(P, F).unwrap().unwrap().deleted);
    }

    #[test]
    fn the_queue_is_bounded_and_a_full_one_still_stores_the_push() {
        let st = Store::open_in_memory().unwrap();
        {
            let conn = st.lock();
            for i in 0..MAX_OPEN_JOBS {
                conn.execute(
                    "INSERT INTO jobs (id, kind, state, project_key, file_path, payload,
                                       not_before, created_at, updated_at)
                     VALUES (?1, 'merge', 'queued', 'p', 'f', '{}', 'x', 'x', 'x')",
                    (format!("job_{i}"),),
                )
                .unwrap();
            }
        }
        st.upsert(P, F, "A", "laptop", &ts(at(0))).unwrap();
        assert_eq!(
            st.write_and_queue_merge(P, F, &side("B", "cloud", 1), "job_x", at(1))
                .unwrap(),
            Queued::Full
        );
        assert_eq!(content(&st).0, "B");
    }

    #[test]
    fn only_a_failed_job_is_retried() {
        let st = conflicted();
        assert!(matches!(
            st.retry_job("job_1", at(2)).unwrap(),
            Retried::NotFailed(_)
        ));
        assert_eq!(st.retry_job("job_none", at(2)).unwrap(), Retried::NotFound);
    }

    /// Done jobs go once old enough; failed ones stay however old, since
    /// each may hold a result nobody has seen, and open ones stay too.
    #[test]
    fn finished_jobs_are_pruned_and_failed_ones_kept() {
        let st = conflicted();
        st.upsert(P, "other.md", "X", "laptop", &ts(at(0))).unwrap();
        st.write_and_queue_merge(P, "other.md", &side("Y", "cloud", 1), "job_f", at(1))
            .unwrap();
        st.upsert(P, "third.md", "X", "laptop", &ts(at(0))).unwrap();
        st.write_and_queue_merge(P, "third.md", &side("Y", "cloud", 1), "job_q", at(1))
            .unwrap();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap();
        st.settle_job("job_1", &merged("lse", "AB"), "worker", "j", at(3))
            .unwrap();
        st.fail_open_jobs("nothing can merge this", at(3)).unwrap();
        st.upsert(P, "fourth.md", "X", "laptop", &ts(at(0)))
            .unwrap();
        st.write_and_queue_merge(P, "fourth.md", &side("Y", "cloud", 4), "job_o", at(4))
            .unwrap();
        assert_eq!(st.prune_jobs(&ts(at(3))).unwrap(), 0);
        assert_eq!(st.prune_jobs(&ts(at(1_000_000))).unwrap(), 1);
        let mut left: Vec<(String, String)> = st
            .jobs(None, 10)
            .unwrap()
            .into_iter()
            .map(|j| (j.id, j.state))
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                ("job_f".into(), "failed".into()),
                ("job_o".into(), "queued".into()),
                ("job_q".into(), "failed".into()),
            ]
        );
    }

    /// The fence holds as soon as the lease runs out, before anyone has
    /// claimed the job again: the old holder's late result is refused, not
    /// taken as the repeat of one already recorded.
    #[test]
    fn an_expired_lease_fences_its_holder_before_anyone_claims_again() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse_old", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        assert!(st.expire_leases(at(62)).unwrap().is_empty());
        assert_eq!(
            st.settle_job(
                "job_1",
                &merged("lse_old", "late"),
                "worker",
                "job_f",
                at(63)
            )
            .unwrap(),
            Settlement::LeaseEnded
        );
        assert_eq!(content(&st).0, "B");
        assert_eq!(st.jobs(Some("queued"), 10).unwrap().len(), 1);
    }

    /// A push onto a deleted file is not a conflict: the delete said to
    /// discard what was there.
    #[test]
    fn a_push_onto_a_deleted_file_queues_nothing() {
        let st = Store::open_in_memory().unwrap();
        st.upsert(P, F, "A", "laptop", &ts(at(0))).unwrap();
        st.tombstone(P, F, "laptop", &ts(at(1))).unwrap();
        assert_eq!(
            st.write_and_queue_merge(P, F, &side("B", "cloud", 2), "job_1", at(2))
                .unwrap(),
            Queued::Nothing
        );
        assert_eq!(content(&st).0, "B");
        assert!(st.jobs(None, 10).unwrap().is_empty());
    }

    /// Deleted, then made again, while its merge waited: the old notes must
    /// not come back into the new file, whether the job was still queued or
    /// already held by a worker.
    #[test]
    fn a_delete_closes_the_files_open_jobs() {
        // Queued when the delete landed.
        let st = conflicted();
        st.tombstone(P, F, "laptop", &ts(at(2))).unwrap();
        st.upsert(P, F, "fresh", "laptop", &ts(at(3))).unwrap();
        assert!(st
            .claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(4))
            .unwrap()
            .is_none());
        let job = &st.jobs(None, 10).unwrap()[0];
        assert_eq!(job.state, "done");
        assert!(job.error.as_deref().unwrap().contains("deleted"));

        // Held by a worker when the delete landed.
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        st.tombstone(P, F, "laptop", &ts(at(3))).unwrap();
        st.upsert(P, F, "fresh", "laptop", &ts(at(4))).unwrap();
        let s = recorded(
            st.settle_job("job_1", &merged("lse", "A and B"), "worker", "job_2", at(5))
                .unwrap(),
        );
        assert_eq!(
            (s.response.state.as_str(), s.applied, s.queued),
            ("done", false, false)
        );
        assert_eq!(s.response.follow_up, None);
        assert_eq!(content(&st).0, "fresh");
        assert_eq!(st.jobs(None, 10).unwrap().len(), 1, "no follow-up");
        // Another file's job is left alone.
        let st = conflicted();
        st.tombstone(P, "other.md", "laptop", &ts(at(2))).unwrap();
        assert_eq!(st.queue_status().unwrap().queued, 1);
    }

    /// An empty merge of two versions that had content is a malfunction:
    /// retried like an error, and never written over the file.
    #[test]
    fn an_empty_merge_of_non_empty_versions_is_retried_not_written() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        let s = recorded(
            st.settle_job("job_1", &merged("lse", " \n"), "worker", "job_2", at(3))
                .unwrap(),
        );
        assert_eq!((s.response.state.as_str(), s.applied), ("queued", false));
        assert_eq!(content(&st), ("B".into(), "cloud".into()));
        assert!(st.jobs(None, 10).unwrap()[0]
            .error
            .as_deref()
            .unwrap()
            .contains("empty"));

        // Two empty versions may merge to nothing.
        let st = Store::open_in_memory().unwrap();
        st.upsert(P, F, "", "laptop", &ts(at(0))).unwrap();
        st.write_and_queue_merge(P, F, &side("\n", "cloud", 1), "job_1", at(1))
            .unwrap();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        let s = recorded(
            st.settle_job("job_1", &merged("lse", ""), "worker", "job_2", at(3))
                .unwrap(),
        );
        assert!(s.applied);
    }

    /// What `/health` shows of a failure names the job, never the project
    /// or the file; the worker's error is kept short.
    #[test]
    fn a_failure_is_public_without_its_file() {
        let st = conflicted();
        st.lock()
            .execute("UPDATE jobs SET attempt = 3", [])
            .unwrap();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap()
            .unwrap();
        let long = format!("{P}/{F}: {}", "é".repeat(2000));
        let s = recorded(
            st.settle_job("job_1", &failed("lse", &long), "w", "j", at(3))
                .unwrap(),
        );
        let failure = s.failed.unwrap();
        let public = failure.public();
        assert!(public.starts_with("merge job job_1 failed after 4 attempts"));
        assert!(!public.contains(P) && !public.contains(F), "{public}");
        assert!(failure.logged().contains(F));
        assert!(failure.error.len() <= MAX_ERROR_BYTES);
        let kept = st.jobs(None, 1).unwrap()[0].error.clone().unwrap();
        assert!(kept.len() <= MAX_ERROR_BYTES + 40, "{}", kept.len());

        // Not applied because the file kept changing: the same.
        let st = conflicted();
        st.lock()
            .execute("UPDATE jobs SET link = ?1", (MAX_LINKS,))
            .unwrap();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap();
        st.upsert(P, F, "C", "laptop", &ts(at(3))).unwrap();
        let public = recorded(
            st.settle_job("job_1", &merged("lse", "AB"), "w", "j", at(4))
                .unwrap(),
        )
        .failed
        .unwrap()
        .public();
        assert!(!public.contains(P) && !public.contains(F), "{public}");
    }

    #[test]
    fn clip_keeps_whole_characters() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("abcdef", 3), "abc");
        assert_eq!(clip("aé", 2), "a");
    }

    /// With no worker left, every open job can be taken at once, a revoked
    /// holder's lease and a retry delay notwithstanding; or failed, keeping
    /// its input for a retry.
    #[test]
    fn open_jobs_are_released_or_failed_for_a_drain() {
        let st = conflicted();
        st.upsert(P, "other.md", "X", "laptop", &ts(at(0))).unwrap();
        st.write_and_queue_merge(P, "other.md", &side("Y", "cloud", 1), "job_2", at(1))
            .unwrap();
        st.claim_job(&merge_kinds(), "lse_gone", Duration::from_secs(600), at(2))
            .unwrap()
            .unwrap();
        st.claim_job(&merge_kinds(), "lse_x", Duration::from_secs(600), at(2))
            .unwrap()
            .unwrap();
        st.settle_job("job_2", &failed("lse_x", "boom"), "w", "j", at(3))
            .unwrap();
        assert_eq!(st.release_open_jobs(at(4)).unwrap(), 2);
        // The revoked holder's lease is over.
        assert_eq!(
            st.settle_job("job_1", &merged("lse_gone", "AB"), "w", "j", at(5))
                .unwrap(),
            Settlement::LeaseEnded
        );
        // Both claimable now, the retry delay dropped.
        for lease in ["lse_a", "lse_b"] {
            assert!(st
                .claim_job(&merge_kinds(), lease, Duration::from_secs(60), at(5))
                .unwrap()
                .is_some());
        }

        let st = conflicted();
        let ids = st.fail_open_jobs("no worker, and no CLI", at(2)).unwrap();
        assert_eq!(ids, vec!["job_1".to_string()]);
        assert_eq!(st.queue_status().unwrap().failed, 1);
        let Retried::Queued(job) = st.retry_job("job_1", at(3)).unwrap() else {
            panic!("not retried")
        };
        assert_eq!(job.state, "queued");
        let m = st
            .claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(4))
            .unwrap()
            .unwrap()
            .merge
            .unwrap();
        assert_eq!(
            (m.stored.content.as_str(), m.incoming.content.as_str()),
            ("A", "B")
        );
    }
}
