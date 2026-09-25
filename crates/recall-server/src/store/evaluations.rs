//! Evaluation reports: the runs the owner asks for, and what the worker
//! found.
//!
//! A run is one row here and one `evaluate` job in `jobs`, made together
//! in one transaction with the `evaluate` leaf that records who asked. The
//! job carries no file content: the claim that leases it reads the files
//! then (see [`evaluate_input`]), and nothing of them is stored.
//!
//! `findings` holds only what [`recall_wire::evaluations::check_finding`]
//! lets through: enums, files the store holds, line numbers. `details`,
//! everything that quotes a note, is stored as the worker sent it, as plain
//! JSON. The design seals it with the content key; that was parked on
//! 2026-09-25, and when it returns only this column changes.
//!
//! A run's state is its job's while the job is open or failed: `queued`,
//! `running` while leased, `failed` once out of attempts. `done` is
//! recorded here, with the report, in the transaction that settles the job,
//! so a run keeps its state after finished jobs are pruned.

use anyhow::{Context, Result};
use recall_wire::evaluations::{
    self, EvaluationRequest, GLOBAL_PREFIX, STATE_DONE, STATE_FAILED, STATE_QUEUED, STATE_RUNNING,
};
use recall_wire::jobs::{KIND_EVALUATE, STATE_LEASED};
use recall_wire::{EvaluateFile, EvaluateInput, Evaluation, EvaluationSummary, Finding};
use rusqlite::{Connection, OptionalExtension, Row};
use time::OffsetDateTime;

use super::{Outcome, Store};
use crate::format_timestamp;

/// Created with the other tables, every time the store opens.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS evaluations (
        id             TEXT PRIMARY KEY,
        -- 'queued' until its report is in, then 'done'. While not done,
        -- the state reported is its job's.
        state          TEXT NOT NULL CHECK (state IN ('queued', 'done')),
        job_id         TEXT NOT NULL,
        -- What was asked: a JSON list of project keys, empty for every
        -- project, and whether to run the contradiction check.
        projects       TEXT NOT NULL,
        contradictions INTEGER NOT NULL DEFAULT 0,
        -- The report: findings as JSON, never note text, and details as
        -- the worker sent them (plain JSON; sealed once encryption lands).
        findings       TEXT,
        details        TEXT,
        created_at     TEXT NOT NULL,
        finished_at    TEXT
    );
    CREATE INDEX IF NOT EXISTS evaluations_by_created ON evaluations (created_at);
";

/// What the job row of an evaluate job holds as its payload: what was
/// asked, and never a file.
#[derive(serde::Serialize, serde::Deserialize)]
struct Payload {
    evaluation_id: String,
    projects: Vec<String>,
    contradictions: bool,
}

/// What asking for a run came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// Queued as this job.
    Queued(String),
    /// The queue is full: nothing was made.
    Full,
}

/// A run's row, and its job's state where the job is still there.
struct EvalRow {
    id: String,
    state: String,
    projects: String,
    contradictions: bool,
    findings: Option<String>,
    details: Option<String>,
    created_at: String,
    finished_at: Option<String>,
    job_state: Option<String>,
    job_error: Option<String>,
    job_updated_at: Option<String>,
}

const EVAL_COLUMNS: &str = "e.id, e.state, e.projects, e.contradictions, e.findings, e.details, \
     e.created_at, e.finished_at, j.state, j.error, j.updated_at";

fn eval_from(r: &Row<'_>) -> rusqlite::Result<EvalRow> {
    Ok(EvalRow {
        id: r.get(0)?,
        state: r.get(1)?,
        projects: r.get(2)?,
        contradictions: r.get::<_, i64>(3)? != 0,
        findings: r.get(4)?,
        details: r.get(5)?,
        created_at: r.get(6)?,
        finished_at: r.get(7)?,
        job_state: r.get(8)?,
        job_error: r.get(9)?,
        job_updated_at: r.get(10)?,
    })
}

impl EvalRow {
    /// The state reported: `done` once recorded, otherwise the job's.
    fn state(&self) -> &'static str {
        if self.state == STATE_DONE {
            return STATE_DONE;
        }
        match self.job_state.as_deref() {
            Some(STATE_LEASED) => STATE_RUNNING,
            Some("failed") => STATE_FAILED,
            _ => STATE_QUEUED,
        }
    }

    fn finished_at(&self) -> Option<String> {
        match self.state() {
            STATE_DONE => self.finished_at.clone(),
            STATE_FAILED => self.job_updated_at.clone(),
            _ => None,
        }
    }

    fn error(&self) -> Option<String> {
        if self.state == STATE_DONE {
            return None;
        }
        self.job_error.clone()
    }

    fn projects(&self) -> Vec<String> {
        serde_json::from_str(&self.projects).unwrap_or_default()
    }

    fn findings(&self) -> Result<Vec<Finding>> {
        match &self.findings {
            Some(text) => serde_json::from_str(text)
                .with_context(|| format!("evaluation {} has findings that do not read", self.id)),
            None => Ok(Vec::new()),
        }
    }

    fn summary(&self) -> Result<EvaluationSummary> {
        let mut counts = std::collections::BTreeMap::new();
        for f in self.findings()? {
            *counts.entry(f.kind).or_insert(0) += 1;
        }
        Ok(EvaluationSummary {
            id: self.id.clone(),
            state: self.state().to_string(),
            created_at: self.created_at.clone(),
            finished_at: self.finished_at(),
            counts,
            projects: self.projects(),
            contradictions: self.contradictions,
            error: self.error(),
        })
    }
}

fn ts(at: OffsetDateTime) -> String {
    format_timestamp(at)
}

/// The input a claim hands the worker for the evaluate job whose payload
/// is `payload`: what was asked, and every live file of the projects asked
/// for (every project when none was) and of every global scope, read now,
/// in the claim's transaction.
pub(super) fn evaluate_input(conn: &Connection, payload: &str) -> Result<EvaluateInput> {
    let asked: Payload = serde_json::from_str(payload)
        .context("an evaluate job has a payload that does not read")?;
    let mut stmt = conn.prepare(
        "SELECT project_key, file_path, content, updated_at FROM memory_files
         WHERE deleted = 0 ORDER BY project_key, file_path",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(EvaluateFile {
            project_key: r.get(0)?,
            file_path: r.get(1)?,
            content: r.get(2)?,
            updated_at: r.get(3)?,
        })
    })?;
    let mut files = Vec::new();
    for row in rows {
        let file = row?;
        if asked.projects.is_empty()
            || file.project_key.starts_with(GLOBAL_PREFIX)
            || asked.projects.contains(&file.project_key)
        {
            files.push(file);
        }
    }
    Ok(EvaluateInput {
        evaluation_id: asked.evaluation_id,
        projects: asked.projects,
        contradictions: asked.contradictions,
        files,
    })
}

/// Whether the store holds the file, a tombstone included: the check that
/// every file a finding names is one, so a path cannot carry note text.
pub(super) fn holds(conn: &Connection, project_key: &str, file_path: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM memory_files WHERE project_key = ?1 AND file_path = ?2",
            (project_key, file_path),
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Records the report of the run `evaluate job_id` was made for: its
/// findings and details, `done`, finished `now`.
pub(super) fn record_report(
    conn: &Connection,
    job_id: &str,
    payload: &str,
    findings: &[Finding],
    details: &serde_json::Value,
    now: OffsetDateTime,
) -> Result<()> {
    let asked: Payload = serde_json::from_str(payload)
        .context("an evaluate job has a payload that does not read")?;
    let changed = conn.execute(
        "UPDATE evaluations SET state = 'done', findings = ?3, details = ?4, finished_at = ?5
         WHERE id = ?1 AND job_id = ?2",
        (
            &asked.evaluation_id,
            job_id,
            serde_json::to_string(findings)?,
            serde_json::to_string(details)?,
            ts(now),
        ),
    )?;
    anyhow::ensure!(
        changed == 1,
        "job {job_id} is for evaluation {}, which is not there",
        asked.evaluation_id
    );
    Ok(())
}

impl Store {
    /// Whether the store holds any row under `project_key`, tombstones
    /// included: whether an evaluation of it would have anything to read.
    pub fn has_project(&self, project_key: &str) -> Result<bool> {
        Ok(self
            .lock()
            .query_row(
                "SELECT 1 FROM memory_files WHERE project_key = ?1 LIMIT 1",
                (project_key,),
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Queues a run as `id`, with its `evaluate` job `job_id`, and appends
    /// the `evaluate` leaf `build_leaf` makes, all in one transaction. A
    /// queue already holding [`super::MAX_OPEN_JOBS`] open jobs makes
    /// nothing and appends nothing.
    pub fn request_evaluation_audited(
        &self,
        id: &str,
        job_id: &str,
        req: &EvaluationRequest,
        now: OffsetDateTime,
        build_leaf: impl FnOnce(u64, &str) -> Vec<u8>,
    ) -> Result<Requested> {
        self.audited(
            |tx, _| {
                let open: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM jobs WHERE state IN ('queued', 'leased')",
                    [],
                    |r| r.get(0),
                )?;
                if open as usize >= super::MAX_OPEN_JOBS {
                    return Ok(Outcome::Refuse(Requested::Full));
                }
                let now = ts(now);
                let projects = serde_json::to_string(&req.projects)?;
                let payload = serde_json::to_string(&Payload {
                    evaluation_id: id.to_string(),
                    projects: req.projects.clone(),
                    contradictions: req.contradictions,
                })?;
                // No project and no file: an evaluation is about many, and
                // the job routes that close a file's jobs leave it alone.
                tx.execute(
                    "INSERT INTO jobs (id, kind, state, project_key, file_path, payload, not_before,
                                       created_at, updated_at)
                     VALUES (?1, ?2, 'queued', '', '', ?3, ?4, ?4, ?4)",
                    (job_id, KIND_EVALUATE, &payload, &now),
                )?;
                tx.execute(
                    "INSERT INTO evaluations (id, state, job_id, projects, contradictions, created_at)
                     VALUES (?1, 'queued', ?2, ?3, ?4, ?5)",
                    (id, job_id, &projects, req.contradictions as i64, &now),
                )?;
                Ok(Outcome::Commit(Requested::Queued(job_id.to_string())))
            },
            |seq, at, _| build_leaf(seq, at),
        )
    }

    /// Runs, newest first, at most `limit`.
    pub fn evaluations(&self, limit: usize) -> Result<Vec<EvaluationSummary>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EVAL_COLUMNS} FROM evaluations e LEFT JOIN jobs j ON j.id = e.job_id
             ORDER BY e.created_at DESC, e.id DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map((limit as i64,), eval_from)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?.summary()?);
        }
        Ok(out)
    }

    /// One run, with its findings, and its details when `with_details`.
    pub fn evaluation(&self, id: &str, with_details: bool) -> Result<Option<Evaluation>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {EVAL_COLUMNS} FROM evaluations e LEFT JOIN jobs j ON j.id = e.job_id
                     WHERE e.id = ?1"
                ),
                (id,),
                eval_from,
            )
            .optional()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let details = match (&row.details, with_details) {
            (Some(text), true) => Some(
                serde_json::from_str(text)
                    .with_context(|| format!("evaluation {id} has details that do not read"))?,
            ),
            _ => None,
        };
        Ok(Some(Evaluation {
            id: row.id.clone(),
            state: row.state().to_string(),
            created_at: row.created_at.clone(),
            finished_at: row.finished_at(),
            findings: row.findings()?,
            details,
            projects: row.projects(),
            contradictions: row.contradictions,
            error: row.error(),
        }))
    }

    /// Whether a run is waiting or being made: a scheduled run is not
    /// queued behind another.
    pub fn evaluation_open(&self) -> Result<bool> {
        Ok(self
            .lock()
            .query_row(
                "SELECT 1 FROM jobs WHERE kind = ?1 AND state IN ('queued', 'leased') LIMIT 1",
                (KIND_EVALUATE,),
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Removes runs whose report came in before `before`, details and all.
    ///
    /// Not in the audit log: the `evaluate` leaf and the job's leaves of
    /// each stay in it.
    pub fn prune_evaluations(&self, before: &str) -> Result<usize> {
        Ok(self.lock().execute(
            "DELETE FROM evaluations WHERE state = 'done' AND finished_at < ?1",
            (before,),
        )?)
    }
}

/// The findings a result carries, checked against what the store holds:
/// every file a finding names, its own and each related one, must be a
/// file here. Their shape was checked as the request was read
/// ([`evaluations::check_finding`]).
pub(super) fn check_files(conn: &Connection, findings: &[Finding]) -> Result<Option<String>> {
    let mut ids = std::collections::HashSet::new();
    for f in findings {
        if !ids.insert(f.id.as_str()) {
            return Ok(Some(format!("two findings have the id {}", f.id)));
        }
        let named = std::iter::once((&f.project_key, &f.file_path))
            .chain(f.related.iter().map(|r| (&r.project_key, &r.file_path)));
        for (project_key, file_path) in named {
            if !holds(conn, project_key, file_path)? {
                return Ok(Some(format!(
                    "finding {} names a file this server does not hold",
                    f.id
                )));
            }
        }
    }
    if findings.len() > evaluations::MAX_FINDINGS {
        return Ok(Some(format!(
            "a report may carry at most {} findings",
            evaluations::MAX_FINDINGS
        )));
    }
    Ok(None)
}
