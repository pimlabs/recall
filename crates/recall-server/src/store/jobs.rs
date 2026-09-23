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

use super::{read_file, write_file, Store};
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
    /// When this call marked the job failed, why: what `/health` shows as
    /// the last merge error.
    pub failed: Option<String>,
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
/// attempt, or failed for good. Answers the error when it is final.
fn after_failure(
    conn: &Connection,
    job: &JobRow,
    error: &str,
    now: OffsetDateTime,
    keep_lease: bool,
) -> Result<Option<String>> {
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
            let error = format!("{error} (gave up after {} attempts)", job.attempt);
            conn.execute(
                "UPDATE jobs SET state = 'failed', lease_id = ?2, lease_expires_at = NULL,
                     error = ?3, applied = 0, updated_at = ?4
                 WHERE id = ?1",
                (&job.id, &lease, &error, ts(now)),
            )?;
            Ok(Some(format!(
                "merge of {}/{} failed: {error}",
                job.project_key, job.file_path
            )))
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
    /// a merge of it with whatever it displaced, in one transaction: the
    /// job's `stored` side is read under the same lock as the write, so it
    /// is exactly the version this push replaced.
    pub fn write_and_queue_merge(
        &self,
        project_key: &str,
        file_path: &str,
        incoming: &MergeSide,
        job_id: &str,
        now: OffsetDateTime,
    ) -> Result<Queued> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let displaced = read_file(&tx, project_key, file_path)?
            .filter(|e| !e.deleted && e.content != incoming.content);
        write_file(
            &tx,
            project_key,
            file_path,
            &incoming.content,
            &incoming.source_env,
            &incoming.updated_at,
        )?;
        let Some(displaced) = displaced else {
            tx.commit()?;
            return Ok(Queued::Nothing);
        };
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'leased')",
            [],
            |r| r.get(0),
        )?;
        if open as usize >= MAX_OPEN_JOBS {
            tx.commit()?;
            return Ok(Queued::Full);
        }
        let input = MergeInput {
            project_key: project_key.to_string(),
            file_path: file_path.to_string(),
            stored: side_of(
                &displaced.content,
                &displaced.source_env,
                &displaced.updated_at,
            ),
            incoming: incoming.clone(),
        };
        insert_merge_job(&tx, job_id, &input, None, now)?;
        tx.commit()?;
        Ok(Queued::Queued(job_id.to_string()))
    }

    /// Puts every job whose lease ran out back in the queue, or fails it
    /// when that was its last attempt. Answers the error of each job it
    /// failed.
    pub fn expire_leases(&self, now: OffsetDateTime) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let expired: Vec<JobRow> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'leased' AND lease_expires_at <= ?1"
            ))?;
            let rows = stmt.query_map((ts(now),), row_from)?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut failed = Vec::new();
        for job in &expired {
            if let Some(error) = after_failure(
                &tx,
                job,
                "the worker did not report back before its lease ended",
                now,
                false,
            )? {
                failed.push(error);
            }
        }
        tx.commit()?;
        Ok(failed)
    }

    /// Leases the oldest queued job of one of `kinds` that may run now,
    /// for `lease` from `now`, under `lease_id`.
    pub fn claim_job(
        &self,
        kinds: &[String],
        lease_id: &str,
        lease: Duration,
        now: OffsetDateTime,
    ) -> Result<Option<Job>> {
        if kinds.is_empty() {
            return Ok(None);
        }
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // Kinds come from the request, so they are bound, never spliced.
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
            return Ok(None);
        };
        let expires = ts(now + lease);
        tx.execute(
            "UPDATE jobs SET state = 'leased', lease_id = ?2, lease_expires_at = ?3,
                 attempt = attempt + 1, updated_at = ?4
             WHERE id = ?1",
            (&job.id, lease_id, &expires, &now_text),
        )?;
        tx.commit()?;
        let merge = match job.kind.as_str() {
            KIND_MERGE => Some(job.merge_input()?),
            _ => None,
        };
        Ok(Some(Job {
            id: job.id,
            kind: job.kind,
            lease_id: lease_id.to_string(),
            lease_expires_at: expires,
            attempt: job.attempt + 1,
            merge,
        }))
    }

    /// Records a worker's result for job `id`.
    ///
    /// A result counts only under the job's current, unexpired lease. The
    /// same result posted again under the lease that settled it changes
    /// nothing and answers as the first did.
    ///
    /// A merge is applied as a compare-and-swap: only if the file still
    /// has the hash of the version the job stored, and then attributed to
    /// `worker`. If another push landed meanwhile, the merged content
    /// becomes the `stored` side of a follow-up job, `follow_up_id`,
    /// against the newer version, at most [`MAX_LINKS`] deep.
    pub fn settle_job(
        &self,
        id: &str,
        result: &ResultRequest,
        worker: &str,
        follow_up_id: &str,
        now: OffsetDateTime,
    ) -> Result<Settlement> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let Some(job) = get_job(&tx, id)? else {
            return Ok(Settlement::NotFound);
        };
        if job.lease_id.as_deref() != Some(result.lease_id.as_str()) {
            return Ok(Settlement::LeaseEnded);
        }
        if job.state != STATE_LEASED {
            // Settled already, under this very lease: the repeat of a
            // result that was recorded.
            return Ok(Settlement::Recorded(Settled {
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
            return Ok(Settlement::LeaseEnded);
        }

        let now_text = ts(now);
        let settled = match (&result.merge, &result.error) {
            (_, Some(error)) => {
                let failed = after_failure(&tx, &job, error, now, true)?;
                Settled {
                    response: ResultResponse::default(),
                    applied: false,
                    queued: false,
                    failed,
                }
            }
            (Some(merged), None) => {
                if job.kind != KIND_MERGE {
                    return Ok(Settlement::WrongKind);
                }
                let input = job.merge_input()?;
                let merged_side = side_of(&merged.content, worker, &now_text);
                let current =
                    read_file(&tx, &input.project_key, &input.file_path)?.filter(|e| !e.deleted);
                match current {
                    // The compare-and-swap: nothing landed since the push
                    // that queued this, so the merge replaces it.
                    Some(file) if content_sha256(&file.content) == input.incoming.sha256 => {
                        write_file(
                            &tx,
                            &input.project_key,
                            &input.file_path,
                            &merged.content,
                            worker,
                            &now_text,
                        )?;
                        finish(&tx, &job, STATE_DONE, true, None, None, None, now)?;
                        Settled {
                            response: ResultResponse::default(),
                            applied: true,
                            queued: false,
                            failed: None,
                        }
                    }
                    // Something else landed meanwhile, and says exactly
                    // what the merge came to.
                    Some(file) if file.content == merged.content => {
                        finish(&tx, &job, STATE_DONE, true, None, None, None, now)?;
                        Settled {
                            response: ResultResponse::default(),
                            applied: false,
                            queued: false,
                            failed: None,
                        }
                    }
                    // Something else landed: merge the result with it,
                    // unless this conflict has been chased far enough.
                    Some(file) if job.link < MAX_LINKS => {
                        let next = MergeInput {
                            project_key: input.project_key.clone(),
                            file_path: input.file_path.clone(),
                            stored: merged_side,
                            incoming: side_of(&file.content, &file.source_env, &file.updated_at),
                        };
                        insert_merge_job(
                            &tx,
                            follow_up_id,
                            &next,
                            Some((&job.id, job.link + 1)),
                            now,
                        )?;
                        finish(
                            &tx,
                            &job,
                            STATE_DONE,
                            false,
                            None,
                            None,
                            Some(follow_up_id),
                            now,
                        )?;
                        Settled {
                            response: ResultResponse::default(),
                            applied: false,
                            queued: true,
                            failed: None,
                        }
                    }
                    Some(_) => {
                        let error = "the file kept changing while it was merged; the newest \
                                     push stands, and this result is kept in the job";
                        finish(
                            &tx,
                            &job,
                            STATE_FAILED,
                            false,
                            Some(error),
                            Some(&merged_side),
                            None,
                            now,
                        )?;
                        Settled {
                            response: ResultResponse::default(),
                            applied: false,
                            queued: false,
                            failed: Some(format!(
                                "merge of {}/{} not applied: {error}",
                                job.project_key, job.file_path
                            )),
                        }
                    }
                    // Deleted meanwhile: the delete said to discard it, as
                    // a push after a delete is never merged either. The
                    // result is kept in the job all the same.
                    None => {
                        finish(
                            &tx,
                            &job,
                            STATE_DONE,
                            false,
                            Some("the file was deleted while it was merged; the delete stands"),
                            Some(&merged_side),
                            None,
                            now,
                        )?;
                        Settled {
                            response: ResultResponse::default(),
                            applied: false,
                            queued: false,
                            failed: None,
                        }
                    }
                }
            }
            (None, None) => anyhow::bail!("a result carries a merge or an error"),
        };
        let response = get_job(&tx, id)?.expect("the job was read above").outcome();
        tx.commit()?;
        Ok(Settlement::Recorded(Settled {
            response,
            ..settled
        }))
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

    /// Queues a failed job again, with its attempts counted afresh.
    ///
    /// A job that failed because the file kept changing kept its result:
    /// it is queued as a merge of that result with the file as it is now,
    /// which is the merge that was never finished.
    pub fn retry_job(&self, id: &str, now: OffsetDateTime) -> Result<Retried> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let Some(job) = get_job(&tx, id)? else {
            return Ok(Retried::NotFound);
        };
        if job.state != STATE_FAILED {
            return Ok(Retried::NotFailed(job.state));
        }
        let mut payload = job.payload.clone();
        if let (Some(kept), KIND_MERGE) = (&job.result, job.kind.as_str()) {
            let kept: MergeSide = serde_json::from_str(kept)?;
            if let Some(file) =
                read_file(&tx, &job.project_key, &job.file_path)?.filter(|e| !e.deleted)
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
        let job = get_job(&tx, id)?.expect("the job was read above");
        tx.commit()?;
        Ok(Retried::Queued(job.summary()))
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
    pub fn prune_jobs(&self, before: &str) -> Result<usize> {
        Ok(self.lock().execute(
            "DELETE FROM jobs WHERE state = 'done' AND updated_at < ?1",
            (before,),
        )?)
    }
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
    use recall_wire::MergeResult;

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
        assert_eq!(m.incoming, side("B", "cloud", 1));
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
        assert!(s.failed.unwrap().contains("claude timed out"));
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

    #[test]
    fn finished_jobs_are_pruned_and_failed_ones_kept() {
        let st = conflicted();
        st.claim_job(&merge_kinds(), "lse", Duration::from_secs(60), at(2))
            .unwrap();
        st.settle_job("job_1", &merged("lse", "AB"), "worker", "j", at(3))
            .unwrap();
        assert_eq!(st.prune_jobs(&ts(at(3))).unwrap(), 0);
        assert_eq!(st.prune_jobs(&ts(at(4))).unwrap(), 1);
    }
}
