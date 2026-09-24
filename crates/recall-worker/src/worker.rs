//! Enrolling, then the job loop: claim, merge, report, again.
//!
//! Before anything else the worker reads the server's discovery document,
//! and goes no further unless the server lists the merge queue: a worker
//! pointed at the wrong server, or one too old to have the queue, stops
//! there, and never asks it to approve anything.
//!
//! The worker enrols by the device flow, like any machine, and never with
//! an authkey: an authkey enrols `sync` devices only, so a leaked one
//! cannot mint a worker. The owner approves its code with the `worker`
//! scope, after comparing the fingerprint the worker prints. Its identity
//! file records the server it was made for, and the worker refuses to run
//! against any other.
//!
//! Each claim long-polls, so an idle worker costs about two requests a
//! minute and a merge starts within a second of the push that queued it.
//! Every claim also carries the worker's check of its `claude` CLI, which
//! is how `/health` still reports the CLI once it has left the API process.
//! A worker whose CLI is not logged in claims with no kinds: it stays
//! visible, and takes no job it would only fail.
//!
//! A refusal no retry will get past (a `4xx` other than `408` and `429`,
//! or a server with no merge queue) is [`Fatal`]: the worker says what to
//! do and stops asking. Anything else, such as a server that is restarting,
//! is retried with a backoff.

use std::future::Future;
use std::time::{Duration, Instant};

use recall_wire::devices::{
    ACCESS_DENIED, AUTHORIZATION_PENDING, ENROLL_PATH, ENROLL_POLL_PATH, EXPIRED_TOKEN,
    INVALID_GRANT, POLL_INTERVAL_SECONDS, SCOPE_WORKER, SLOW_DOWN,
};
use recall_wire::discovery::CAPABILITY_MERGE_QUEUE;
use recall_wire::jobs::{self, KIND_MERGE};
use recall_wire::{
    ClaimRequest, ClaimResponse, ClaudeCliReport, Discovery, EnrollPending, EnrollPollRequest,
    EnrollPollResponse, EnrollRequest, Job, MergeResult, ResultRequest, ResultResponse,
    DISCOVERY_PATH,
};
use reqwest::StatusCode;

use crate::api::{Api, ApiError};
use crate::config::Config;
use crate::identity::Identity;
use crate::merge::{Merger, Status};

/// How long an unsigned request (discovery, enrolment) may take.
const UNSIGNED_TIMEOUT: Duration = Duration::from_secs(30);

/// How long posting a result may take. A result carries one file.
const RESULT_TIMEOUT: Duration = Duration::from_secs(60);

/// How many times a result is posted before the worker gives up on it and
/// lets the lease run out, which puts the job back in the queue.
const RESULT_TRIES: u32 = 5;

/// The longest the worker waits between failed attempts to reach the
/// server.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How often the CLI is checked while it is not logged in, whatever the
/// configured interval: logging in on the worker should be noticed within
/// a minute, not half an hour.
const NOT_LOGGED_IN_RECHECK: Duration = Duration::from_secs(60);

/// For how long after a code is printed it is polled at the interval the
/// server asks for. The owner approving it is most likely at a terminal in
/// that first minute; after it, [`SLOW_POLL`] is plenty.
const FAST_POLL_FOR: Duration = Duration::from_secs(60);

/// How often a code nobody approved in its first minute is polled.
const SLOW_POLL: Duration = Duration::from_secs(30);

/// Why the worker stopped. Each is something only a person can fix, so the
/// worker says what to do rather than retrying forever.
#[derive(Debug, thiserror::Error)]
pub enum Fatal {
    /// The identity file could not be read or written.
    #[error("{0}")]
    Identity(String),
    /// The identity was made for another server.
    #[error(
        "{path} was made for {made_for}, not {configured}, and a worker's key is never \
         offered to a second server. If RECALL_WORKER_SERVER is wrong, correct it; to move \
         this worker to {configured}, revoke it on {made_for}, delete {path} and restart"
    )]
    OtherServer {
        /// The server the identity records.
        made_for: String,
        /// `RECALL_WORKER_SERVER`.
        configured: String,
        /// The identity file.
        path: String,
    },
    /// An identity an earlier build wrote, part way through enrolling
    /// with a server it did not record.
    #[error(
        "{0} was written by an earlier recall-worker that did not record which server it \
         enrolled with, so it cannot be checked against RECALL_WORKER_SERVER. Revoke the \
         device it enrolled as (or deny its code), delete {0} and restart"
    )]
    Unbound(String),
    /// The server is not one this worker can work for.
    #[error("{0}")]
    NotRecall(String),
    /// The owner denied the enrolment.
    #[error("the owner denied this worker's enrolment. Delete {0} and restart to ask again")]
    Denied(String),
    /// The owner approved it with a scope other than `worker`.
    #[error(
        "this device was approved with the {0} scope, not worker, so it cannot claim jobs. \
         Revoke it, delete {1} and restart, then approve the new code as a worker \
         (recall devices approve <code> --worker)"
    )]
    WrongScope(String, String),
    /// The server no longer knows this device, or revoked it.
    #[error(
        "the server refused this worker ({0}). If it was revoked on purpose, nothing is wrong; \
         to enrol again, delete {1} and restart"
    )]
    Refused(String, String),
    /// Something else that will not fix itself, such as a name another
    /// device already has.
    #[error("{0}")]
    Other(String),
}

/// What one claim came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// No job arrived within the wait.
    Idle,
    /// A job was done and its result recorded.
    Done(ResultResponse),
    /// A job was done, and the server would not take the result: its
    /// lease ended, or the job is gone.
    Rejected(String),
}

/// A worker: its settings, identity and CLI.
pub struct Worker {
    cfg: Config,
    api: Api,
    id: Identity,
    merger: Merger,
    status: Status,
    status_at: Option<Instant>,
    /// The last code the approval instructions were printed for.
    announced: Option<String>,
}

fn log(message: &str) {
    eprintln!("recall-worker: {message}");
}

/// Whether an answer is one that asking again will not change: a redirect
/// (never followed), or a client error other than a timeout or a rate
/// limit.
fn is_final(e: &ApiError) -> bool {
    e.status().is_some_and(|s| {
        s.is_redirection()
            || (s.is_client_error()
                && s != StatusCode::REQUEST_TIMEOUT
                && s != StatusCode::TOO_MANY_REQUESTS)
    })
}

/// How long to wait before asking again after `e`: what the server asked
/// for, or the next step of a backoff that doubles up to [`MAX_BACKOFF`].
fn next_wait(e: &ApiError, backoff: Duration) -> Duration {
    match e {
        ApiError::Status {
            retry_after: Some(s),
            ..
        } => Duration::from_secs(*s),
        _ => (backoff * 2).clamp(Duration::from_secs(1), MAX_BACKOFF),
    }
}

impl Worker {
    /// A worker for `cfg`, with the identity kept in its data directory,
    /// made there if this is the first start. Refuses an identity made for
    /// another server.
    pub fn new(cfg: Config) -> Result<Self, Fatal> {
        if cfg.data_dir.as_os_str().is_empty() {
            return Err(Fatal::Other(
                "no data directory: set RECALL_WORKER_DIR to where the worker's key is kept".into(),
            ));
        }
        let path = Identity::path(&cfg.data_dir).display().to_string();
        let mut id = Identity::load_or_create(&cfg.data_dir, &cfg.server)
            .map_err(|e| Fatal::Identity(format!("cannot use {path}: {e}")))?;
        match id.server.as_deref() {
            Some(server) if server == cfg.server => {}
            Some(server) => {
                return Err(Fatal::OtherServer {
                    made_for: server.to_string(),
                    configured: cfg.server.clone(),
                    path,
                })
            }
            // An earlier build's file. With nothing enrolled yet, the key
            // has been offered to no server, and is this one's from now on.
            None if id.device_id.is_none() && id.enrollment_id.is_none() => {
                id.server = Some(cfg.server.clone());
                id.save(&cfg.data_dir)
                    .map_err(|e| Fatal::Identity(format!("cannot write {path}: {e}")))?;
            }
            None => return Err(Fatal::Unbound(path)),
        }
        let api = Api::new(&cfg.server).map_err(|e| Fatal::Other(e.to_string()))?;
        let merger = Merger::new(cfg.claude_bin.clone(), cfg.merge_timeout);
        Ok(Self {
            cfg,
            api,
            id,
            merger,
            status: Status::default(),
            status_at: None,
            announced: None,
        })
    }

    /// Its identity: key fingerprint, and device id once approved.
    pub fn identity(&self) -> &Identity {
        &self.id
    }

    fn identity_path(&self) -> String {
        Identity::path(&self.cfg.data_dir).display().to_string()
    }

    fn save(&self) -> Result<(), Fatal> {
        self.id
            .save(&self.cfg.data_dir)
            .map_err(|e| Fatal::Identity(format!("cannot write {}: {e}", self.identity_path())))
    }

    /// Checks the server, enrols until the owner approves, then runs jobs
    /// until `shutdown` resolves. Returns only on shutdown, or on something
    /// a person has to fix.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> Result<(), Fatal> {
        tokio::pin!(shutdown);
        log(&format!(
            "{} for {}, key fingerprint {}",
            crate::user_agent(),
            self.cfg.server,
            self.id.fingerprint()
        ));
        if let Some(warning) = self.cfg.plaintext_warning() {
            log(&warning);
        }
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            checked = self.check_server() => checked?,
        }
        if self.id.device_id.is_none() {
            tokio::select! {
                _ = &mut shutdown => return Ok(()),
                enrolled = self.enrol() => enrolled?,
            }
        }
        log(&format!(
            "enrolled as {}; waiting for jobs",
            self.id.device_id.as_deref().unwrap_or("")
        ));
        let mut backoff = Duration::ZERO;
        loop {
            if !backoff.is_zero() {
                tokio::select! {
                    _ = &mut shutdown => return Ok(()),
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
            let step = tokio::select! {
                _ = &mut shutdown => return Ok(()),
                step = self.step() => step,
            };
            backoff = match step {
                Ok(Step::Idle) => Duration::ZERO,
                Ok(Step::Done(outcome)) => {
                    log(&format!(
                        "job {}: {}{}",
                        outcome.id,
                        outcome.state,
                        match (outcome.applied, &outcome.follow_up) {
                            (true, _) => ", merged file stored".to_string(),
                            (false, Some(next)) => {
                                format!(", the file changed meanwhile; follow-up {next}")
                            }
                            (false, None) => String::new(),
                        }
                    ));
                    Duration::ZERO
                }
                Ok(Step::Rejected(why)) => {
                    log(&why);
                    Duration::ZERO
                }
                Err(e) => {
                    if let Some(fatal) = self.fatal(&e) {
                        return Err(fatal);
                    }
                    // Not a refusal of this device but of the claim itself,
                    // such as the 404 of a server rolled back to a release
                    // from before the queue. A 401 is left to retry: it is
                    // also what a signature made just before a restart gets.
                    if is_final(&e) && e.status() != Some(StatusCode::UNAUTHORIZED) {
                        return Err(Fatal::NotRecall(format!(
                            "{} refused a claim ({e}), and asking again will not change that. \
                             If the server was rolled back to a release without the merge \
                             queue, restart the worker once it has the queue again",
                            self.cfg.server
                        )));
                    }
                    let wait = next_wait(&e, backoff);
                    log(&format!("claim failed ({e}); trying again in {wait:?}"));
                    wait
                }
            };
        }
    }

    /// Whether a refusal is of this device, which no retry will get past.
    fn fatal(&self, e: &ApiError) -> Option<Fatal> {
        match e.status()? {
            // A 401 is also what a signature made a moment before the
            // server restarted gets, which the next request fixes; only a
            // device the server does not know, or revoked, is final.
            StatusCode::UNAUTHORIZED
                if e.message().contains("unknown device") || e.message().contains("revoked") =>
            {
                Some(Fatal::Refused(
                    e.message().to_string(),
                    self.identity_path(),
                ))
            }
            StatusCode::FORBIDDEN => Some(Fatal::Refused(
                e.message().to_string(),
                self.identity_path(),
            )),
            _ => None,
        }
    }

    /// Reads the server's discovery document, and goes on only if the
    /// server lists the merge queue. An answer that says this is not such a
    /// server is final; one that says nothing (the server is restarting, or
    /// not up yet) is asked again.
    pub async fn check_server(&self) -> Result<(), Fatal> {
        let server = &self.cfg.server;
        let mut backoff = Duration::ZERO;
        loop {
            let answer: Result<Discovery, ApiError> =
                self.api.get(DISCOVERY_PATH, UNSIGNED_TIMEOUT).await;
            match answer {
                Ok(doc) if doc.can(CAPABILITY_MERGE_QUEUE) => return Ok(()),
                Ok(doc) => {
                    return Err(Fatal::NotRecall(format!(
                        "{server} is Recall {}, which has no merge queue, so there is nothing \
                         for a worker to do. Upgrade it, or stop the worker",
                        doc.server.version
                    )))
                }
                Err(ApiError::Body(why)) => {
                    return Err(Fatal::NotRecall(format!(
                        "{server} did not answer {DISCOVERY_PATH} with Recall's discovery \
                         document ({why}); RECALL_WORKER_SERVER must name a Recall server"
                    )))
                }
                Err(e) if is_final(&e) => {
                    return Err(Fatal::NotRecall(format!(
                        "{server} answered {DISCOVERY_PATH} with {e}: it is not a Recall server, \
                         or one older than 0.4.1, which has no merge queue. Check \
                         RECALL_WORKER_SERVER"
                    )))
                }
                Err(e) => {
                    backoff = next_wait(&e, backoff);
                    log(&format!(
                        "cannot reach {server} ({e}); trying again in {backoff:?}"
                    ));
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    /// Enrols by the device flow: asks for a code, prints it with the key
    /// fingerprint, and polls until the owner approves it. A restart while
    /// waiting keeps polling the same enrolment.
    pub async fn enrol(&mut self) -> Result<(), Fatal> {
        loop {
            match self.id.user_code.clone() {
                Some(code) if self.id.enrollment_id.is_some() => self.announce(&code),
                _ => self.start_enrolment().await?,
            }
            match self.wait_for_approval().await? {
                Some(device_id) => {
                    self.id.device_id = Some(device_id);
                    self.id.enrollment_id = None;
                    self.id.user_code = None;
                    self.save()?;
                    return Ok(());
                }
                // The code expired before anyone approved it: ask for a
                // new one.
                None => {
                    self.id.enrollment_id = None;
                    self.id.user_code = None;
                    self.save()?;
                }
            }
        }
    }

    async fn start_enrolment(&mut self) -> Result<(), Fatal> {
        let req = EnrollRequest {
            name: self.cfg.name.clone(),
            public_key: self.id.public_key(),
            agent: crate::user_agent(),
            authkey: None,
        };
        let mut backoff = Duration::ZERO;
        let pending: EnrollPending = loop {
            match self.api.post(ENROLL_PATH, &req, UNSIGNED_TIMEOUT).await {
                Ok(pending) => break pending,
                Err(e) if e.status() == Some(StatusCode::CONFLICT) => {
                    return Err(Fatal::Other(format!(
                        "{}. Set RECALL_WORKER_NAME to another name, or revoke the old worker",
                        e.message()
                    )))
                }
                // A 404 among them: a server without the enrolment route
                // answers every retry the same way.
                Err(e) if is_final(&e) => {
                    return Err(Fatal::Other(format!(
                        "{} refused the enrolment ({e}); asking again would be refused \
                         the same way",
                        self.cfg.server
                    )))
                }
                Err(e) => {
                    backoff = next_wait(&e, backoff);
                    log(&format!(
                        "cannot reach the server to enrol ({e}); trying again in {backoff:?}"
                    ));
                    tokio::time::sleep(backoff).await;
                }
            }
        };
        self.id.enrollment_id = Some(pending.enrollment_id);
        self.id.user_code = Some(pending.user_code.clone());
        self.save()?;
        self.announce(&pending.user_code);
        Ok(())
    }

    /// Says what the owner has to do, once per code.
    fn announce(&mut self, code: &str) {
        if self.announced.as_deref() == Some(code) {
            return;
        }
        self.announced = Some(code.to_string());
        log(&format!(
            "waiting for approval of code {code} as a worker, key fingerprint {}",
            self.id.fingerprint()
        ));
        log(&format!(
            "approve it from an admin device: recall devices approve {code} --worker \
             --fingerprint {}",
            self.id.fingerprint()
        ));
        log(&format!(
            "or with the operator token: POST /v1/devices/approve \
             {{\"user_code\":\"{code}\",\"scope\":\"worker\",\"fingerprint\":\"{}\"}} \
             (see deploy/README.md, \"The merge worker\")",
            self.id.fingerprint()
        ));
    }

    /// How long to wait before the next poll of a code printed `since`
    /// ago, `slowed` being what the server's `slow_down` answers added.
    fn poll_interval(since: Duration, slowed: Duration) -> Duration {
        let base = if since < FAST_POLL_FOR {
            Duration::from_secs(POLL_INTERVAL_SECONDS)
        } else {
            SLOW_POLL
        };
        base + slowed
    }

    /// Polls until approved (`Some(device_id)`), or until the code expires
    /// (`None`).
    async fn wait_for_approval(&mut self) -> Result<Option<String>, Fatal> {
        let Some(enrollment_id) = self.id.enrollment_id.clone() else {
            return Ok(None);
        };
        let since = Instant::now();
        let mut slowed = Duration::ZERO;
        let req = EnrollPollRequest { enrollment_id };
        loop {
            tokio::time::sleep(Self::poll_interval(since.elapsed(), slowed)).await;
            let polled: Result<EnrollPollResponse, ApiError> = self
                .api
                .post(ENROLL_POLL_PATH, &req, UNSIGNED_TIMEOUT)
                .await;
            match polled {
                Ok(approved) if approved.scope == SCOPE_WORKER => {
                    return Ok(Some(approved.device_id))
                }
                Ok(approved) => {
                    // Kept, so the owner can find and revoke it; the worker
                    // itself will not run as anything but a worker.
                    self.id.device_id = Some(approved.device_id);
                    self.save()?;
                    return Err(Fatal::WrongScope(approved.scope, self.identity_path()));
                }
                Err(e) if e.status() == Some(StatusCode::BAD_REQUEST) => match e.message() {
                    AUTHORIZATION_PENDING => {}
                    SLOW_DOWN => slowed += Duration::from_secs(5),
                    EXPIRED_TOKEN | INVALID_GRANT => {
                        log("the code expired before it was approved; asking for a new one");
                        return Ok(None);
                    }
                    ACCESS_DENIED => return Err(Fatal::Denied(self.identity_path())),
                    other => log(&format!("poll refused: {other}")),
                },
                Err(e) if is_final(&e) => {
                    return Err(Fatal::Other(format!(
                        "{} refused the poll for this worker's approval ({e})",
                        self.cfg.server
                    )))
                }
                Err(e) => log(&format!("poll failed ({e}); trying again")),
            }
        }
    }

    /// Re-checks the CLI when the last check is old enough, or when a
    /// merge has just failed.
    async fn refresh_status(&mut self) {
        let every = if self.status.logged_in {
            self.cfg.claude_status_interval
        } else {
            self.cfg.claude_status_interval.min(NOT_LOGGED_IN_RECHECK)
        };
        if self.status_at.is_some_and(|at| at.elapsed() < every) {
            return;
        }
        // Never checked before: whatever it finds is news.
        let (was, first) = (self.status.logged_in, self.status.checked_at.is_empty());
        self.status = self.merger.check_status().await;
        self.status_at = Some(Instant::now());
        if first || self.status.logged_in != was {
            log(&if self.status.logged_in {
                "the claude CLI is logged in; taking merge jobs".to_string()
            } else {
                format!(
                    "the claude CLI cannot merge ({}); taking no jobs until it can. \
                     Log it in with: docker exec -it -u node recall-worker claude setup-token",
                    if self.status.error.is_empty() {
                        "not logged in"
                    } else {
                        &self.status.error
                    }
                )
            });
        }
    }

    /// The claim this worker sends now: every kind it can do, which is
    /// none while its CLI cannot merge, and that CLI's last check.
    pub fn claim_request(&self) -> ClaimRequest {
        ClaimRequest {
            kinds: if self.status.logged_in {
                vec![KIND_MERGE.to_string()]
            } else {
                Vec::new()
            },
            wait_seconds: self.cfg.wait_seconds,
            lease_seconds: self.cfg.lease_seconds,
            claude_cli: Some(ClaudeCliReport {
                checked_at: self.status.checked_at.clone(),
                available: self.status.available,
                logged_in: self.status.logged_in,
                error: self.status.error.clone(),
            }),
        }
    }

    /// One claim, and the job it brought, if any.
    pub async fn step(&mut self) -> Result<Step, ApiError> {
        self.refresh_status().await;
        let device_id = self.id.device_id.clone().unwrap_or_default();
        let claim = self.claim_request();
        let answer: ClaimResponse = self
            .api
            .post_signed(
                jobs::CLAIM_PATH,
                &claim,
                self.id.key(),
                &device_id,
                Duration::from_secs(self.cfg.wait_seconds + 30),
            )
            .await?;
        let Some(job) = answer.job else {
            return Ok(Step::Idle);
        };
        let result = self.work(&job).await;
        self.report(&job, &result).await
    }

    /// Does one job, and says what to report.
    ///
    /// A merge that fails has the CLI checked again before the next claim,
    /// rather than at the next scheduled check: a CLI that was logged out
    /// under the worker should stop it taking jobs now, not in half an hour.
    pub async fn work(&mut self, job: &Job) -> ResultRequest {
        let outcome = match (job.kind.as_str(), &job.merge) {
            (KIND_MERGE, Some(m)) => {
                if recall_wire::content_sha256(&m.stored.content) != m.stored.sha256
                    || recall_wire::content_sha256(&m.incoming.content) != m.incoming.sha256
                {
                    Err("a version's content does not match its sha256".to_string())
                } else if m.stored.content == m.incoming.content {
                    // Identical inputs never reach claude: there is nothing
                    // to reconcile, and a call would cost money to say so.
                    Ok(m.incoming.content.clone())
                } else {
                    log(&format!(
                        "job {}: merging {}/{} (attempt {})",
                        job.id, m.project_key, m.file_path, job.attempt
                    ));
                    let merged = self
                        .merger
                        .merge(&m.stored.content, &m.incoming.content)
                        .await;
                    if merged.is_err() {
                        self.status_at = None;
                    }
                    merged.map_err(|e| e.to_string())
                }
            }
            (kind, _) => Err(format!("this worker does not do {kind} jobs")),
        };
        match outcome {
            Ok(content) => ResultRequest {
                lease_id: job.lease_id.clone(),
                merge: Some(MergeResult { content }),
                error: None,
            },
            Err(error) => {
                log(&format!("job {}: {error}", job.id));
                ResultRequest {
                    lease_id: job.lease_id.clone(),
                    merge: None,
                    error: Some(error),
                }
            }
        }
    }

    /// Posts a result, trying again when the server could not be reached.
    /// Posting the same result twice is safe: the server records it once.
    async fn report(&self, job: &Job, result: &ResultRequest) -> Result<Step, ApiError> {
        let device_id = self.id.device_id.clone().unwrap_or_default();
        let mut wait = Duration::from_secs(1);
        let mut last = None;
        for _ in 0..RESULT_TRIES {
            let posted: Result<ResultResponse, ApiError> = self
                .api
                .post_signed(
                    &jobs::result_path(&job.id),
                    result,
                    self.id.key(),
                    &device_id,
                    RESULT_TIMEOUT,
                )
                .await;
            match posted {
                Ok(outcome) => return Ok(Step::Done(outcome)),
                Err(e)
                    if matches!(
                        e.status(),
                        Some(
                            StatusCode::CONFLICT | StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST
                        )
                    ) =>
                {
                    return Ok(Step::Rejected(format!(
                        "job {}: the server did not take the result: {}",
                        job.id,
                        e.message()
                    )))
                }
                Err(e) if self.fatal(&e).is_some() => return Err(e),
                Err(e) => {
                    log(&format!(
                        "job {}: posting the result failed ({e}); trying again in {wait:?}",
                        job.id
                    ));
                    last = Some(e);
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
            }
        }
        Err(last.unwrap_or_else(|| ApiError::Transport("no attempt was made".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use recall_wire::{MergeInput, MergeSide};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config(dir: &std::path::Path, server: &str) -> Config {
        Config {
            server: server.to_string(),
            data_dir: dir.to_path_buf(),
            // Never run: nothing in these tests may reach a real CLI.
            claude_bin: "definitely-not-a-real-claude".into(),
            ..Config::default()
        }
    }

    fn side(content: &str) -> MergeSide {
        MergeSide {
            sha256: recall_wire::content_sha256(content),
            content: content.to_string(),
            source_env: "laptop".into(),
            updated_at: "2026-10-02T09:10:11.020Z".into(),
        }
    }

    fn job(stored: MergeSide, incoming: MergeSide) -> Job {
        Job {
            id: "job_1".into(),
            kind: KIND_MERGE.into(),
            lease_id: "lse_1".into(),
            lease_expires_at: "2026-10-02T09:16:03.118Z".into(),
            attempt: 1,
            merge: Some(MergeInput {
                project_key: "acme/app".into(),
                file_path: "topics/auth.md".into(),
                stored,
                incoming,
            }),
        }
    }

    /// A worker for a server nothing listens on: nothing here sends a
    /// request.
    fn worker() -> (tempfile::TempDir, Worker) {
        let dir = tempfile::tempdir().unwrap();
        let w = Worker::new(config(dir.path(), "http://127.0.0.1:9")).unwrap();
        (dir, w)
    }

    /// Either side's content must match the hash the server sent with it,
    /// the stored side as much as the incoming one, or the worker merges
    /// nothing and says why.
    #[tokio::test]
    async fn a_version_that_does_not_match_its_hash_is_not_merged() {
        let (_dir, mut w) = worker();
        let mut stored = side("A");
        stored.sha256 = recall_wire::content_sha256("not A");
        let mut incoming = side("B");
        incoming.sha256 = recall_wire::content_sha256("not B");
        for (s, i) in [
            (stored.clone(), side("B")),
            (side("A"), incoming.clone()),
            // Identical but for the hash: without the check this would be
            // answered as a merge without asking claude at all.
            (stored.clone(), {
                let mut same = side("A");
                same.sha256 = stored.sha256.clone();
                same
            }),
        ] {
            let result = w.work(&job(s, i)).await;
            assert!(result.merge.is_none(), "{result:?}");
            assert!(
                result.error.as_deref().unwrap().contains("sha256"),
                "{result:?}"
            );
            assert_eq!(result.lease_id, "lse_1");
        }
        // Matching hashes and the same content: answered without claude.
        let result = w.work(&job(side("A"), side("A"))).await;
        assert_eq!(result.merge.unwrap().content, "A");
    }

    /// A worker whose CLI cannot merge asks for no kind of job, so it is
    /// never handed one it would only fail.
    #[test]
    fn a_worker_that_cannot_merge_claims_nothing() {
        let (_dir, mut w) = worker();
        assert!(w.claim_request().kinds.is_empty(), "never checked");
        w.status = Status {
            checked_at: "2026-10-02T09:13:40.002Z".into(),
            available: true,
            logged_in: false,
            error: "not logged in".into(),
        };
        let claim = w.claim_request();
        assert!(claim.kinds.is_empty());
        assert_eq!(claim.claude_cli.unwrap().error, "not logged in");
        w.status.logged_in = true;
        assert_eq!(w.claim_request().kinds, vec![KIND_MERGE.to_string()]);
    }

    /// A merge that fails has the CLI checked again before the next claim.
    #[tokio::test]
    async fn a_failed_merge_has_the_cli_checked_again() {
        let (_dir, mut w) = worker();
        w.status.logged_in = true;
        w.status_at = Some(Instant::now());
        let result = w.work(&job(side("A"), side("B"))).await;
        assert!(result.error.unwrap().contains("unavailable"));
        assert!(w.status_at.is_none());
    }

    #[test]
    fn a_code_is_polled_quickly_for_a_minute_then_slowly() {
        let fast = Duration::from_secs(POLL_INTERVAL_SECONDS);
        assert_eq!(Worker::poll_interval(Duration::ZERO, Duration::ZERO), fast);
        assert_eq!(
            Worker::poll_interval(Duration::from_secs(59), Duration::from_secs(5)),
            fast + Duration::from_secs(5)
        );
        assert_eq!(
            Worker::poll_interval(Duration::from_secs(61), Duration::ZERO),
            SLOW_POLL
        );
        assert!(SLOW_POLL >= Duration::from_secs(30));
    }

    /// A key enrolled with one server is never offered to another.
    #[test]
    fn an_identity_is_bound_to_its_server() {
        let dir = tempfile::tempdir().unwrap();
        Worker::new(config(dir.path(), "http://recall-server:8787")).unwrap();
        match Worker::new(config(dir.path(), "https://recall.example.com")) {
            Err(Fatal::OtherServer {
                made_for,
                configured,
                ..
            }) => {
                assert_eq!(made_for, "http://recall-server:8787");
                assert_eq!(configured, "https://recall.example.com");
            }
            other => panic!("not refused: {:?}", other.err()),
        }
        assert!(Worker::new(config(dir.path(), "http://recall-server:8787")).is_ok());
    }

    /// A file an earlier build wrote records no server. Unused, it is bound
    /// to this one; part way through an enrolment, it is refused.
    #[test]
    fn an_identity_from_before_servers_were_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let mut id = Identity::from_seed([3; 32]);
        id.save(dir.path()).unwrap();
        Worker::new(config(dir.path(), "http://recall-server:8787")).unwrap();
        assert_eq!(
            Identity::load(dir.path())
                .unwrap()
                .unwrap()
                .server
                .as_deref(),
            Some("http://recall-server:8787")
        );

        let dir = tempfile::tempdir().unwrap();
        id.device_id = Some("dev_somewhere".into());
        id.save(dir.path()).unwrap();
        assert!(matches!(
            Worker::new(config(dir.path(), "http://recall-server:8787")),
            Err(Fatal::Unbound(_))
        ));
    }

    #[test]
    fn a_worker_needs_a_data_directory() {
        assert!(matches!(
            Worker::new(Config {
                server: "http://127.0.0.1:9".into(),
                ..Config::default()
            }),
            Err(Fatal::Other(_))
        ));
    }

    /// A stand-in server on loopback that answers each request with what
    /// `answer` returns for its method and path, and counts the requests.
    async fn fake_server(
        answer: fn(&str, &str) -> (u16, String),
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let counted = counted.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // The head, then as much body as it says there is.
                    let head_end = loop {
                        let n = conn.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        let n = conn.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let mut first = head.lines().next().unwrap_or_default().split(' ');
                    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
                    counted.fetch_add(1, Ordering::SeqCst);
                    let (status, body) = answer(method, path);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = conn.write_all(resp.as_bytes()).await;
                    let _ = conn.shutdown().await;
                });
            }
        });
        (url, count)
    }

    fn discovery(capabilities: &str) -> String {
        format!(
            r#"{{"protocol":{{"current":1,"supported":[1]}},"server":{{"version":"0.4.0","build":{{"channel":"release"}}}},"min_client":"0.1.0","auth":{{"methods":["bearer"]}},"capabilities":{{{capabilities}}}}}"#
        )
    }

    /// Runs a worker against `url` until it stops, which must be soon.
    async fn run_briefly(url: &str) -> Result<(), Fatal> {
        let dir = tempfile::tempdir().unwrap();
        let w = Worker::new(config(dir.path(), url)).unwrap();
        tokio::time::timeout(Duration::from_secs(10), w.run(std::future::pending()))
            .await
            .expect("the worker kept retrying something that will not change")
    }

    /// A server that is not Recall, or has no merge queue, is never asked
    /// to enrol anything.
    #[tokio::test]
    async fn a_server_without_the_merge_queue_is_never_enrolled_with() {
        let (url, count) = fake_server(|_, path| match path {
            DISCOVERY_PATH => (404, r#"{"error":"not found"}"#.into()),
            _ => (500, "{}".into()),
        })
        .await;
        assert!(matches!(run_briefly(&url).await, Err(Fatal::NotRecall(_))));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (url, count) = fake_server(|_, path| match path {
            DISCOVERY_PATH => (200, discovery(r#""devices":{}"#)),
            _ => (500, "{}".into()),
        })
        .await;
        match run_briefly(&url).await {
            Err(Fatal::NotRecall(why)) => assert!(why.contains("no merge queue"), "{why}"),
            other => panic!("not refused: {other:?}"),
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (url, _) = fake_server(|_, _| (200, "<html>hello</html>".into())).await;
        assert!(matches!(run_briefly(&url).await, Err(Fatal::NotRecall(_))));
    }

    /// An enrolment refused with a 404 is refused for good: the worker says
    /// so once, rather than asking again every minute.
    #[tokio::test]
    async fn an_enrolment_refused_with_404_is_final() {
        let (url, count) = fake_server(|_, path| match path {
            DISCOVERY_PATH => (200, discovery(r#""merge_queue":{}"#)),
            _ => (404, r#"{"error":"not found"}"#.into()),
        })
        .await;
        match run_briefly(&url).await {
            Err(Fatal::Other(why)) => assert!(why.contains("refused the enrolment"), "{why}"),
            other => panic!("not refused: {other:?}"),
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
