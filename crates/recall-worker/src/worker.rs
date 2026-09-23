//! Enrolling, then the job loop: claim, merge, report, again.
//!
//! The worker enrols by the device flow, like any machine, and never with
//! an authkey: an authkey enrols `sync` devices only, so a leaked one
//! cannot mint a worker. The owner approves its code with the `worker`
//! scope, after comparing the fingerprint the worker prints.
//!
//! Each claim long-polls, so an idle worker costs about two requests a
//! minute and a merge starts within a second of the push that queued it.
//! Every claim also carries the worker's check of its `claude` CLI, which
//! is how `/health` still reports the CLI once it has left the API process.
//! A worker whose CLI is not logged in claims with no kinds: it stays
//! visible, and takes no job it would only fail.

use std::future::Future;
use std::time::{Duration, Instant};

use recall_wire::devices::{
    ACCESS_DENIED, AUTHORIZATION_PENDING, ENROLL_PATH, ENROLL_POLL_PATH, EXPIRED_TOKEN,
    INVALID_GRANT, SCOPE_WORKER, SLOW_DOWN,
};
use recall_wire::jobs::{self, KIND_MERGE};
use recall_wire::{
    ClaimRequest, ClaimResponse, ClaudeCliReport, EnrollPending, EnrollPollRequest,
    EnrollPollResponse, EnrollRequest, Job, MergeResult, ResultRequest, ResultResponse,
};
use reqwest::StatusCode;

use crate::api::{Api, ApiError};
use crate::config::Config;
use crate::identity::Identity;
use crate::merge::{Merger, Status};

/// How long an unsigned enrolment request may take.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Why the worker stopped. Each is something only a person can fix, so the
/// process exits and says what to do rather than retrying forever.
#[derive(Debug, thiserror::Error)]
pub enum Fatal {
    /// The identity file could not be read or written.
    #[error("{0}")]
    Identity(String),
    /// The owner denied the enrolment.
    #[error("the owner denied this worker's enrolment. Delete {0} and restart to ask again")]
    Denied(String),
    /// The owner approved it with a scope other than `worker`.
    #[error(
        "this device was approved with the {0} scope, not worker, so it cannot claim jobs. \
         Revoke it, delete {1} and restart, then approve the new code with --scope worker"
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
}

fn log(message: &str) {
    eprintln!("recall-worker: {message}");
}

impl Worker {
    /// A worker for `cfg`, with the identity kept in its data directory,
    /// made there if this is the first start.
    pub fn new(cfg: Config) -> Result<Self, Fatal> {
        let id = Identity::load_or_create(&cfg.data_dir).map_err(|e| {
            Fatal::Identity(format!(
                "cannot use {}: {e}",
                Identity::path(&cfg.data_dir).display()
            ))
        })?;
        let api = Api::new(&cfg.url).map_err(|e| Fatal::Other(e.to_string()))?;
        let merger = Merger::new(cfg.claude_bin.clone(), cfg.merge_timeout);
        Ok(Self {
            cfg,
            api,
            id,
            merger,
            status: Status::default(),
            status_at: None,
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

    /// Enrols, until the owner approves, then runs jobs until `shutdown`
    /// resolves. Returns only on shutdown, or on something a person has to
    /// fix.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> Result<(), Fatal> {
        tokio::pin!(shutdown);
        log(&format!(
            "{} for {}, key fingerprint {}",
            crate::user_agent(),
            self.cfg.url,
            self.id.fingerprint()
        ));
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
                    let wait = match &e {
                        ApiError::Status {
                            retry_after: Some(s),
                            ..
                        } => Duration::from_secs(*s),
                        _ => (backoff * 2).clamp(Duration::from_secs(1), MAX_BACKOFF),
                    };
                    log(&format!("claim failed ({e}); trying again in {wait:?}"));
                    wait
                }
            };
        }
    }

    /// Whether a refusal is one no retry will get past.
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

    /// Enrols by the device flow: asks for a code, prints it with the key
    /// fingerprint, and polls until the owner approves it. A restart while
    /// waiting keeps polling the same enrolment.
    pub async fn enrol(&mut self) -> Result<(), Fatal> {
        loop {
            if self.id.enrollment_id.is_none() {
                self.start_enrolment().await?;
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
        let mut backoff = Duration::from_secs(1);
        let pending: EnrollPending = loop {
            match self.api.post(ENROLL_PATH, &req, ENROLL_TIMEOUT).await {
                Ok(pending) => break pending,
                Err(e) if e.status() == Some(StatusCode::CONFLICT) => {
                    return Err(Fatal::Other(format!(
                        "{}. Set RECALL_WORKER_NAME to another name, or revoke the old worker",
                        e.message()
                    )))
                }
                Err(e) if e.status() == Some(StatusCode::BAD_REQUEST) => {
                    return Err(Fatal::Other(format!("enrolment refused: {e}")))
                }
                Err(e) => {
                    log(&format!(
                        "cannot reach the server to enrol ({e}); trying again in {backoff:?}"
                    ));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
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
    fn announce(&self, code: &str) {
        log(&format!(
            "waiting for approval of code {code} as a worker, key fingerprint {}",
            self.id.fingerprint()
        ));
        log(&format!(
            "approve it from an admin device: recall devices approve {code} --scope worker"
        ));
        log(&format!(
            "or with the operator token: POST /v1/devices/approve \
             {{\"user_code\":\"{code}\",\"scope\":\"worker\",\"fingerprint\":\"{}\"}} \
             (see deploy/README.md, \"The merge worker\")",
            self.id.fingerprint()
        ));
    }

    /// Polls until approved (`Some(device_id)`), or until the code expires
    /// (`None`).
    async fn wait_for_approval(&mut self) -> Result<Option<String>, Fatal> {
        let Some(enrollment_id) = self.id.enrollment_id.clone() else {
            return Ok(None);
        };
        if let Some(code) = &self.id.user_code {
            self.announce(code);
        }
        let mut interval = Duration::from_secs(recall_wire::devices::POLL_INTERVAL_SECONDS);
        let req = EnrollPollRequest { enrollment_id };
        loop {
            tokio::time::sleep(interval).await;
            let polled: Result<EnrollPollResponse, ApiError> =
                self.api.post(ENROLL_POLL_PATH, &req, ENROLL_TIMEOUT).await;
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
                    SLOW_DOWN => interval += Duration::from_secs(5),
                    EXPIRED_TOKEN | INVALID_GRANT => {
                        log("the code expired before it was approved; asking for a new one");
                        return Ok(None);
                    }
                    ACCESS_DENIED => return Err(Fatal::Denied(self.identity_path())),
                    other => log(&format!("poll refused: {other}")),
                },
                Err(e) => log(&format!("poll failed ({e}); trying again")),
            }
        }
    }

    /// Re-checks the CLI when the last check is old enough.
    async fn refresh_status(&mut self) {
        let every = if self.status.logged_in {
            self.cfg.claude_status_interval
        } else {
            self.cfg.claude_status_interval.min(NOT_LOGGED_IN_RECHECK)
        };
        if self.status_at.is_some_and(|at| at.elapsed() < every) {
            return;
        }
        let (was, first) = (self.status.logged_in, self.status_at.is_none());
        self.status = self.merger.check_status().await;
        self.status_at = Some(Instant::now());
        if first || self.status.logged_in != was {
            log(&if self.status.logged_in {
                "the claude CLI is logged in; taking merge jobs".to_string()
            } else {
                format!(
                    "the claude CLI cannot merge ({}); taking no jobs until it can. \
                     Log it in with: docker compose exec -it recall-worker claude setup-token",
                    if self.status.error.is_empty() {
                        "not logged in"
                    } else {
                        &self.status.error
                    }
                )
            });
        }
    }

    /// One claim, and the job it brought, if any.
    pub async fn step(&mut self) -> Result<Step, ApiError> {
        self.refresh_status().await;
        let device_id = self.id.device_id.clone().unwrap_or_default();
        let claim = ClaimRequest {
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
        };
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
    pub async fn work(&self, job: &Job) -> ResultRequest {
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
                    self.merger
                        .merge(&m.stored.content, &m.incoming.content)
                        .await
                        .map_err(|e| e.to_string())
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
