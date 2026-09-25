//! `/v1/jobs`: the merge queue, and the worker that drains it.
//!
//! A push whose base is stale, on a server with a worker enrolled, is stored
//! at once (last-write-wins) and a `merge` job is queued holding both
//! versions. The worker, `recall-worker`, is an enrolled device with the
//! [`SCOPE_WORKER`](crate::devices::SCOPE_WORKER) scope and no inbound port:
//! it long-polls [`CLAIM_PATH`], merges with the local `claude` CLI, and
//! posts the result to [`result_path`]. Nothing a hook does ever waits on
//! it; the merged file arrives with a later pull.
//!
//! A claim takes a **lease**: the job is the worker's until
//! `lease_expires_at`, and a result counts only under the `lease_id` it was
//! handed. An expired lease puts the job back in the queue, and the old
//! holder's late result is refused with `409`, so a worker that stalled
//! cannot overwrite the one that took over (a fencing token).
//!
//! Applying a merge is a compare-and-swap on the file's hash: if another
//! push landed while the job ran, the merged content is not written over it
//! but queued again against the new version, as a follow-up job.

use serde::{Deserialize, Serialize};

/// `POST`: wait for a job, and lease it. A worker device only.
pub const CLAIM_PATH: &str = "/v1/jobs/claim";

/// `GET`: jobs, newest first, without file content. Admin only; takes an
/// optional `?state=`.
pub const JOBS_PATH: &str = "/v1/jobs";

/// `POST`: hand back the result of the job `id`, or the error it met.
pub fn result_path(id: &str) -> String {
    format!("{JOBS_PATH}/{id}/result")
}

/// `POST`: queue the failed job `id` again. Admin only.
pub fn retry_path(id: &str) -> String {
    format!("{JOBS_PATH}/{id}/retry")
}

/// Reconcile two versions of one file.
pub const KIND_MERGE: &str = "merge";

/// Look over memory and report what needs the owner's attention: see
/// [`crate::evaluations`].
pub const KIND_EVALUATE: &str = "evaluate";

/// Waiting to be claimed, or to reach its `not_before` after a failed
/// attempt.
pub const STATE_QUEUED: &str = "queued";
/// Claimed, under a lease that has not ended.
pub const STATE_LEASED: &str = "leased";
/// Finished: its result was applied, or superseded by a follow-up.
pub const STATE_DONE: &str = "done";
/// Out of attempts, or its result could not be applied. Kept, with any
/// unapplied result, until someone retries it.
pub const STATE_FAILED: &str = "failed";

/// Every state a job can be in, in the order a job moves through them.
pub const STATES: [&str; 4] = [STATE_QUEUED, STATE_LEASED, STATE_DONE, STATE_FAILED];

/// The longest a claim may wait for a job, in seconds. Long enough that an
/// idle worker costs about two requests a minute; short enough to stay well
/// inside every proxy's idle timeout.
pub const MAX_WAIT_SECONDS: u64 = 30;

/// The shortest lease a claim may ask for, in seconds.
pub const MIN_LEASE_SECONDS: u64 = 30;

/// The longest lease a claim may ask for, in seconds.
pub const MAX_LEASE_SECONDS: u64 = 600;

fn default_lease_seconds() -> u64 {
    120
}

/// Body of `POST /v1/jobs/claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// The kinds this worker can do, such as [`KIND_MERGE`]. Empty is
    /// allowed: the claim then waits and answers `{"job": null}`, which is
    /// how a worker whose `claude` CLI is not logged in still reports that
    /// it is alive, and why, without taking jobs it would only fail.
    pub kinds: Vec<String>,
    /// How long to wait for a job before answering `{"job": null}`: 0 to
    /// [`MAX_WAIT_SECONDS`].
    #[serde(default)]
    pub wait_seconds: u64,
    /// How long the job is this worker's once claimed:
    /// [`MIN_LEASE_SECONDS`] to [`MAX_LEASE_SECONDS`].
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u64,
    /// The worker's own check of its `claude` CLI. This is how `/health`
    /// keeps reporting the CLI once it runs somewhere other than the API
    /// process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_cli: Option<ClaudeCliReport>,
}

/// A worker's check of its `claude` CLI, as it reports it with each claim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCliReport {
    /// When the check ran.
    pub checked_at: String,
    /// Whether the binary was found and runnable.
    pub available: bool,
    /// Whether it has a usable login.
    pub logged_in: bool,
    /// Why the check failed, when it did; empty otherwise.
    #[serde(default)]
    pub error: String,
}

/// `POST /v1/jobs/claim`'s answer: a leased job, or `null` when none
/// arrived within `wait_seconds`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimResponse {
    /// The job, now leased to the caller.
    pub job: Option<Job>,
}

/// One leased job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// `job_…`.
    pub id: String,
    /// What to do, such as [`KIND_MERGE`]; the member of the same name
    /// holds the input.
    pub kind: String,
    /// The fencing token: a result counts only under this.
    pub lease_id: String,
    /// When the lease ends and the job goes back to the queue.
    pub lease_expires_at: String,
    /// Which attempt this is, from 1.
    pub attempt: u32,
    /// The input of a [`KIND_MERGE`] job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeInput>,
    /// The input of a [`KIND_EVALUATE`] job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluate: Option<EvaluateInput>,
}

/// What a merge job reconciles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeInput {
    /// The project the file belongs to.
    pub project_key: String,
    /// The file.
    pub file_path: String,
    /// The version the push displaced.
    pub stored: MergeSide,
    /// The version the push stored. Its hash is the one the file must still
    /// have for the merged result to be applied.
    pub incoming: MergeSide,
}

/// One version of a file in a merge job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeSide {
    /// [`content_sha256`](crate::content_sha256) of `content`.
    pub sha256: String,
    /// The file's exact bytes.
    pub content: String,
    /// The machine that wrote this version.
    pub source_env: String,
    /// When it was written.
    pub updated_at: String,
}

/// What an evaluate job looks at, as the claim that leases it carries it.
///
/// The files come with the claim, read when it is made, rather than by the
/// worker pulling each project: a worker then reads memory only while it
/// holds an evaluation the owner asked for, and only the projects asked
/// for, and the worker scope stays what it was, the job routes and nothing
/// else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluateInput {
    /// The run this job makes the report for: `eval_…`.
    pub evaluation_id: String,
    /// The projects asked for; empty for every project.
    pub projects: Vec<String>,
    /// Whether to run the contradiction check, which asks `claude`.
    pub contradictions: bool,
    /// Every live file of the projects asked for (every project, when none
    /// was named) and of every global scope, ordered by project and path.
    /// Tombstones are left out.
    #[serde(default)]
    pub files: Vec<EvaluateFile>,
}

/// One file, as an evaluate job carries it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluateFile {
    /// The project, or scope, it belongs to.
    pub project_key: String,
    /// The file.
    pub file_path: String,
    /// Its exact bytes.
    pub content: String,
    /// When it was last written.
    pub updated_at: String,
}

/// An evaluate job's result: see [`crate::evaluations`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluateResult {
    /// What was found, each holding nothing but enums, a file, lines and
    /// related files; the server refuses a finding with any other key.
    pub findings: Vec<crate::evaluations::Finding>,
    /// Everything that quotes a note, as a
    /// [`Details`](crate::evaluations::Details) object.
    pub details: serde_json::Value,
}

/// Body of `POST /v1/jobs/{id}/result`: exactly one of `merge`, `evaluate`
/// and `error`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRequest {
    /// The lease the job was claimed under.
    pub lease_id: String,
    /// A merge job's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeResult>,
    /// Why the job could not be done. It is retried after 1, 5 and 30
    /// minutes, then marked failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// An evaluate job's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluate: Option<EvaluateResult>,
}

impl ResultRequest {
    /// How many of `merge`, `evaluate` and `error` it carries: exactly one
    /// is a result the server records.
    pub fn members(&self) -> usize {
        [
            self.merge.is_some(),
            self.evaluate.is_some(),
            self.error.is_some(),
        ]
        .into_iter()
        .filter(|m| *m)
        .count()
    }
}

/// A merge job's result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResult {
    /// The merged file.
    pub content: String,
}

/// `POST /v1/jobs/{id}/result`'s answer: what became of the job.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultResponse {
    /// The job.
    pub id: String,
    /// Its state now.
    pub state: String,
    /// Whether a merge result was written to the file. `false` for an
    /// error, for a result the file had moved on from, and for an
    /// evaluation, which writes no file.
    pub applied: bool,
    /// When the file changed while the job ran: the job that merges this
    /// result with the newer version.
    pub follow_up: Option<String>,
}

/// One job, as `GET /v1/jobs` lists it and as a retry answers. No file
/// content: the listing is for seeing what the queue is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobSummary {
    /// `job_…`.
    pub id: String,
    /// Such as [`KIND_MERGE`].
    pub kind: String,
    /// One of [`STATES`].
    pub state: String,
    /// The project of the file it concerns.
    pub project_key: String,
    /// The file it concerns.
    pub file_path: String,
    /// How many times it has been handed out.
    pub attempt: u32,
    /// When it was queued.
    pub created_at: String,
    /// When its state last changed.
    pub updated_at: String,
    /// The last error it met, or why its result was not applied.
    pub error: Option<String>,
    /// The follow-up job its result was queued into, if any.
    pub follow_up: Option<String>,
}

/// `GET /v1/jobs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobList {
    /// At most 200, newest first.
    pub jobs: Vec<JobSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_defaults_to_no_wait_and_a_two_minute_lease() {
        let claim: ClaimRequest = serde_json::from_str(r#"{"kinds":["merge"]}"#).unwrap();
        assert_eq!((claim.wait_seconds, claim.lease_seconds), (0, 120));
        assert_eq!(claim.claude_cli, None);
    }

    /// An empty answer says `null`, not nothing, so a client reads the
    /// absence of a job rather than guessing at it.
    #[test]
    fn no_job_is_null() {
        assert_eq!(
            serde_json::to_string(&ClaimResponse::default()).unwrap(),
            r#"{"job":null}"#
        );
    }

    #[test]
    fn a_result_sends_only_the_member_it_has() {
        let ok = ResultRequest {
            lease_id: "lse_a".into(),
            merge: Some(MergeResult {
                content: "merged".into(),
            }),
            error: None,
            evaluate: None,
        };
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"lease_id":"lse_a","merge":{"content":"merged"}}"#
        );
        let failed = ResultRequest {
            lease_id: "lse_a".into(),
            merge: None,
            error: Some("claude merge timed out after 45s".into()),
            evaluate: None,
        };
        assert_eq!(
            serde_json::to_string(&failed).unwrap(),
            r#"{"lease_id":"lse_a","error":"claude merge timed out after 45s"}"#
        );
    }

    #[test]
    fn paths() {
        assert_eq!(result_path("job_a"), "/v1/jobs/job_a/result");
        assert_eq!(retry_path("job_a"), "/v1/jobs/job_a/retry");
    }
}
