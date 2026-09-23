//! The merge queue through the router: a stale push queued for an enrolled
//! worker and answered at once, claims and their leases, results and the
//! compare-and-swap that applies them, what each scope may do, and the
//! inline merge a server without a worker still runs. The last test runs
//! the real `recall-worker` against this server on a real socket.
//!
//! Requests are signed with `recall_wire::signature`, as the worker signs
//! them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use recall_server::merge::Status;
use recall_server::{Config, Server, Store};
use recall_wire::devices::{self, SCOPE_ADMIN, SCOPE_SYNC, SCOPE_WORKER};
use recall_wire::jobs::{self as wire_jobs, CLAIM_PATH};
use recall_wire::signature::{self, encode_public_key, SigningKey, Target};
use recall_wire::{
    ClaimResponse, Device, EnrollPending, ErrorResponse, Health, JobList, JobSummary, PushResponse,
    ResultResponse, SyncResponse,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "jobs-test-token";
const HOST: &str = "recall.test";
const P: &str = "acme/app";
const F: &str = "topics/auth.md";

struct Harness {
    server: Server,
    dir: TempDir,
    _store: Arc<Store>,
    /// Keeps a fake `claude` alive for as long as the server may run it.
    _claude: Option<TempDir>,
}

/// A stand-in `claude`: `auth status` says it is logged in; a merge waits
/// `sleep` seconds, then answers `merged` (or with the two inputs joined
/// when `merged` is empty).
fn fake_claude(sleep: u32, merged: &str) -> (TempDir, String) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("claude");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(
        f,
        r#"if [ "$1" = auth ]; then printf '%s' '{{"loggedIn":true}}'; exit 0; fi"#
    )
    .unwrap();
    writeln!(f, "cat > /dev/null").unwrap();
    writeln!(f, "sleep {sleep}").unwrap();
    writeln!(
        f,
        r#"printf '%s' '{{"is_error":false,"result":"{merged}"}}'"#
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Wait out ETXTBSY: another test's fork may still hold the write
    // handle this process just closed (see merge.rs's `settle`).
    for _ in 0..200 {
        match std::process::Command::new(&path).arg("auth").output() {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(5)),
            _ => break,
        }
    }
    let bin = path.to_str().unwrap().to_string();
    (dir, bin)
}

fn harness(tweak: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let mut cfg = Config {
        token: TOKEN.to_string(),
        rate_limit_max: 10_000,
        ..Config::default()
    };
    tweak(&mut cfg);
    Harness {
        server: Server::new(cfg, store.clone()),
        dir,
        _store: store,
        _claude: None,
    }
}

/// A server whose inline merge would run a `claude` that takes `sleep`
/// seconds and answers `merged`, logged in as far as the server knows.
fn harness_with_claude(sleep: u32, merged: &str) -> Harness {
    let (claude, bin) = fake_claude(sleep, merged);
    let mut h = harness(|cfg| cfg.claude_bin = bin);
    h.server.set_claude_status(Status {
        checked_at: "2026-10-02T09:13:40.002Z".into(),
        available: true,
        logged_in: true,
        error: String::new(),
    });
    h._claude = Some(claude);
    h
}

struct Machine {
    key: SigningKey,
    name: String,
    id: String,
}

impl Machine {
    fn new(seed: u8, name: &str) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
            name: name.to_string(),
            id: String::new(),
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("jobs-nonce-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn signed_request(method: &str, uri: &str, body: Vec<u8>, m: &Machine) -> Request<Body> {
    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (uri, None),
    };
    let headers = signature::sign_request(
        &m.key,
        &m.id,
        &Target {
            method,
            authority: HOST,
            path,
            query,
        },
        "1",
        &body,
        unix_now(),
        &nonce(),
    )
    .unwrap();
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST)
        .header("content-type", "application/json")
        .header("user-agent", "recall-worker/test (linux-x86_64)")
        .header(recall_wire::PROTOCOL_HEADER, "1")
        .header(signature::CONTENT_DIGEST_HEADER, headers.content_digest)
        .header(signature::SIGNATURE_INPUT_HEADER, headers.signature_input)
        .header(signature::SIGNATURE_HEADER, headers.signature)
        .body(Body::from(body))
        .unwrap()
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> (StatusCode, Bytes) {
        let resp = self.server.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Bytes) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = body.map(|b| Body::from(b.to_string())).unwrap_or_default();
        self.send(req.body(body).unwrap()).await
    }

    async fn signed(
        &self,
        m: &Machine,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Bytes) {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        self.send(signed_request(method, uri, body, m)).await
    }

    async fn enrol(&self, m: &mut Machine, scope: &str) -> Device {
        let pending: EnrollPending = ok(self
            .call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": m.name, "public_key": encode_public_key(&m.key.verifying_key()), "agent": "recall-worker/test"})),
            )
            .await);
        let device: Device = ok(self
            .call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": pending.user_code, "scope": scope})),
            )
            .await);
        m.id = device.id.clone();
        device
    }

    async fn worker(&self) -> Machine {
        let mut w = Machine::new(40, "worker");
        let device = self.enrol(&mut w, SCOPE_WORKER).await;
        assert_eq!(device.scope, "worker");
        w
    }

    async fn push(&self, content: &str, base: Option<&str>) -> (StatusCode, Bytes) {
        let mut body =
            json!({"project_key": P, "file_path": F, "content": content, "source_env": "laptop"});
        if let Some(base) = base {
            body["base_sha256"] = json!(base);
        }
        self.call("POST", "/sync", Some(TOKEN), Some(body)).await
    }

    /// Stores `A`, then pushes `B` from a base that is not `A`: a
    /// conflict, answered with the push's response.
    async fn conflict(&self) -> PushResponse {
        ok::<PushResponse>(self.push("A", None).await);
        ok(self
            .push("B", Some(&recall_wire::content_sha256("an older A")))
            .await)
    }

    async fn stored(&self) -> (String, String) {
        let pulled: SyncResponse = ok(self
            .call("GET", &format!("/sync?project_key={P}"), Some(TOKEN), None)
            .await);
        let file = pulled.files.into_iter().find(|f| f.file_path == F).unwrap();
        (file.content.unwrap_or_default(), file.source_env)
    }

    async fn claim(&self, w: &Machine, wait: u64) -> ClaimResponse {
        ok(self
            .signed(
                w,
                "POST",
                CLAIM_PATH,
                Some(json!({"kinds": ["merge"], "wait_seconds": wait, "lease_seconds": 120,
                            "claude_cli": {"checked_at": "2026-10-02T09:13:40.002Z", "available": true, "logged_in": true, "error": ""}})),
            )
            .await)
    }

    async fn result(&self, w: &Machine, id: &str, body: Value) -> (StatusCode, Bytes) {
        self.signed(w, "POST", &wire_jobs::result_path(id), Some(body))
            .await
    }

    async fn jobs(&self) -> Vec<JobSummary> {
        ok::<JobList>(self.call("GET", "/v1/jobs", Some(TOKEN), None).await).jobs
    }

    fn sql(&self, statement: &str) {
        let conn = rusqlite::Connection::open(self.dir.path().join("recall.db")).unwrap();
        conn.execute(statement, []).unwrap();
    }
}

fn ok<T: DeserializeOwned>((status, body): (StatusCode, Bytes)) -> T {
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn error_of((status, body): (StatusCode, Bytes)) -> (StatusCode, String) {
    let err: ErrorResponse = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("not an error body: {}", String::from_utf8_lossy(&body)));
    (status, err.error)
}

// ---------------------------------------------------------------------------
// a stale push, with and without a worker
// ---------------------------------------------------------------------------

/// The reason the queue exists: with a worker enrolled, a hook's push is
/// never held by a merge. The `claude` here would take 30 seconds.
#[tokio::test]
async fn with_a_worker_a_stale_push_is_stored_and_answered_at_once() {
    let h = harness_with_claude(30, "never");
    h.worker().await;
    ok::<PushResponse>(h.push("A", None).await);

    let started = Instant::now();
    let pushed = tokio::time::timeout(
        Duration::from_secs(10),
        h.push("B", Some(&recall_wire::content_sha256("an older A"))),
    )
    .await
    .expect("the push waited on a merge");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "took {:?}",
        started.elapsed()
    );
    let (status, body) = pushed;
    assert_eq!(status, StatusCode::OK);
    let resp: PushResponse = serde_json::from_slice(&body).unwrap();
    assert!(!resp.merged, "queued, not merged");
    let job = resp.merge_job.expect("a job id");
    assert!(job.starts_with("job_"), "{job}");
    assert_eq!(h.stored().await, ("B".into(), "laptop".into()));

    let jobs = h.jobs().await;
    assert_eq!(jobs.len(), 1, "exactly one job");
    assert_eq!(
        (
            jobs[0].id.as_str(),
            jobs[0].state.as_str(),
            jobs[0].file_path.as_str()
        ),
        (job.as_str(), "queued", F)
    );
}

/// A server without a worker answers exactly as it did before the queue:
/// merged inline, and not one key more in the response.
#[tokio::test]
async fn without_a_worker_the_merge_runs_inline_and_the_push_answers_as_before() {
    let h = harness_with_claude(0, "A and B");
    ok::<PushResponse>(h.push("A", None).await);
    let (status, body) = h
        .push("B", Some(&recall_wire::content_sha256("an older A")))
        .await;
    assert_eq!(status, StatusCode::OK);
    let raw: Value = serde_json::from_slice(&body).unwrap();
    let keys: Vec<&str> = raw
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "ok",
            "project_key",
            "file_path",
            "deleted",
            "merged",
            "updated_at"
        ]
    );
    assert_eq!(raw["merged"], json!(true));
    assert_eq!(h.stored().await.0, "A and B");
    assert!(h.jobs().await.is_empty());
}

/// Revoking the worker is going back to the inline merge.
#[tokio::test]
async fn a_revoked_worker_puts_merging_back_inline() {
    let h = harness_with_claude(0, "A and B");
    let w = h.worker().await;
    let (status, _) = h
        .call(
            "POST",
            &devices::revoke_device_path(&w.id),
            Some(TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let pushed = h.conflict().await;
    assert!(pushed.merged && pushed.merge_job.is_none());
}

/// `RECALL_MERGE_ENABLED=false` keeps its meaning: no merge, and no job.
#[tokio::test]
async fn with_merge_off_nothing_is_queued() {
    let h = harness(|cfg| cfg.merge_enabled = false);
    h.worker().await;
    let pushed = h.conflict().await;
    assert!(!pushed.merged && pushed.merge_job.is_none());
    assert!(h.jobs().await.is_empty());
}

/// A push from the base that is stored is the next edit, not a conflict,
/// with or without a worker.
#[tokio::test]
async fn the_next_edit_is_not_queued() {
    let h = harness(|_| {});
    h.worker().await;
    ok::<PushResponse>(h.push("A", None).await);
    let pushed: PushResponse = ok(h.push("B", Some(&recall_wire::content_sha256("A"))).await);
    assert_eq!(pushed.merge_job, None);
    assert!(h.jobs().await.is_empty());
}

// ---------------------------------------------------------------------------
// claims, leases and results
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claimed_job_is_merged_and_the_merge_arrives_with_the_next_pull() {
    let h = harness(|_| {});
    let w = h.worker().await;
    let job_id = h.conflict().await.merge_job.unwrap();

    let job = h.claim(&w, 0).await.job.expect("a job");
    assert_eq!(
        (job.id.as_str(), job.kind.as_str(), job.attempt),
        (job_id.as_str(), "merge", 1)
    );
    let m = job.merge.clone().unwrap();
    assert_eq!((m.project_key.as_str(), m.file_path.as_str()), (P, F));
    assert_eq!(
        (m.stored.content.as_str(), m.incoming.content.as_str()),
        ("A", "B")
    );
    assert!(job.lease_id.starts_with("lse_"));
    // Leased: the next claim finds nothing.
    assert!(h.claim(&w, 0).await.job.is_none());

    let body = json!({"lease_id": job.lease_id, "merge": {"content": "A and B"}});
    let first: ResultResponse = ok(h.result(&w, &job.id, body.clone()).await);
    assert_eq!(
        first,
        ResultResponse {
            id: job.id.clone(),
            state: "done".into(),
            applied: true,
            follow_up: None
        }
    );
    // Attributed to the worker that merged it.
    assert_eq!(h.stored().await, ("A and B".into(), "worker".into()));

    // Posted again, as a worker whose first answer was lost would: the
    // same answer, and nothing changes, even after another push.
    ok::<PushResponse>(
        h.push("C", Some(&recall_wire::content_sha256("A and B")))
            .await,
    );
    let again: ResultResponse = ok(h.result(&w, &job.id, body).await);
    assert_eq!(again, first);
    assert_eq!(h.stored().await.0, "C");
    assert_eq!(h.jobs().await.len(), 1);

    let health: Health = ok(h.call("GET", "/health", None, None).await);
    assert!(!health.merge.last_merge_at.is_empty());
}

/// The fencing token: a worker that stalled past its lease cannot
/// overwrite the one that took over.
#[tokio::test]
async fn a_result_under_a_superseded_lease_is_409_and_changes_nothing() {
    let h = harness(|_| {});
    let w = h.worker().await;
    h.conflict().await;
    let first = h.claim(&w, 0).await.job.unwrap();

    // The lease runs out, and the retry delay passes.
    h.sql("UPDATE jobs SET lease_expires_at = '2000-01-01T00:00:00.000Z'");
    assert!(
        h.claim(&w, 0).await.job.is_none(),
        "waits out its retry delay"
    );
    h.sql("UPDATE jobs SET not_before = '2000-01-01T00:00:00.000Z'");
    let second = h.claim(&w, 0).await.job.expect("handed out again");
    assert_eq!((second.id.as_str(), second.attempt), (first.id.as_str(), 2));
    assert_ne!(second.lease_id, first.lease_id);

    assert_eq!(
        error_of(
            h.result(
                &w,
                &first.id,
                json!({"lease_id": first.lease_id, "merge": {"content": "late"}})
            )
            .await
        ),
        (
            StatusCode::CONFLICT,
            "this lease has ended; the job was handed out again".into()
        )
    );
    assert_eq!(h.stored().await.0, "B");
    let done: ResultResponse = ok(h
        .result(
            &w,
            &second.id,
            json!({"lease_id": second.lease_id, "merge": {"content": "AB"}}),
        )
        .await);
    assert!(done.applied);
    assert_eq!(h.stored().await.0, "AB");
}

/// The compare-and-swap: a push that landed while the job ran stands, and
/// the merge is queued again against it.
#[tokio::test]
async fn a_result_for_a_file_that_changed_meanwhile_makes_a_follow_up() {
    let h = harness(|_| {});
    let w = h.worker().await;
    h.conflict().await;
    let job = h.claim(&w, 0).await.job.unwrap();
    // The next edit on top of B: not a conflict, so no job of its own.
    let pushed: PushResponse = ok(h.push("C", Some(&recall_wire::content_sha256("B"))).await);
    assert_eq!(pushed.merge_job, None);

    let settled: ResultResponse = ok(h
        .result(
            &w,
            &job.id,
            json!({"lease_id": job.lease_id, "merge": {"content": "AB"}}),
        )
        .await);
    assert!(!settled.applied);
    let follow_up = settled.follow_up.expect("a follow-up");
    assert_eq!(h.stored().await.0, "C", "the push in between is not lost");

    let next = h.claim(&w, 0).await.job.expect("the follow-up");
    assert_eq!(next.id, follow_up);
    let m = next.merge.unwrap();
    assert_eq!(
        (m.stored.content.as_str(), m.incoming.content.as_str()),
        ("AB", "C")
    );
    ok::<ResultResponse>(
        h.result(
            &w,
            &next.id,
            json!({"lease_id": next.lease_id, "merge": {"content": "ABC"}}),
        )
        .await,
    );
    assert_eq!(h.stored().await.0, "ABC");
}

#[tokio::test]
async fn an_error_is_retried_later_and_a_failed_job_can_be_retried_by_the_owner() {
    let h = harness(|_| {});
    let w = h.worker().await;
    h.conflict().await;
    let job = h.claim(&w, 0).await.job.unwrap();
    let settled: ResultResponse = ok(h
        .result(
            &w,
            &job.id,
            json!({"lease_id": job.lease_id, "error": "claude merge timed out after 45s"}),
        )
        .await);
    assert_eq!((settled.state.as_str(), settled.applied), ("queued", false));
    assert!(h.claim(&w, 0).await.job.is_none(), "not before a minute");

    // Out of attempts.
    h.sql("UPDATE jobs SET not_before = '2000-01-01T00:00:00.000Z', attempt = 3");
    let last = h.claim(&w, 0).await.job.unwrap();
    assert_eq!(last.attempt, 4);
    let failed: ResultResponse = ok(h
        .result(
            &w,
            &last.id,
            json!({"lease_id": last.lease_id, "error": "claude merge timed out after 45s"}),
        )
        .await);
    assert_eq!(failed.state, "failed");
    let health: Health = ok(h.call("GET", "/health", None, None).await);
    assert!(health
        .merge
        .last_merge_error
        .unwrap()
        .message
        .contains("timed out"));
    assert_eq!(health.merge.queue.unwrap().failed, 1);

    let listed: JobList = ok(h
        .call("GET", "/v1/jobs?state=failed", Some(TOKEN), None)
        .await);
    assert_eq!(listed.jobs.len(), 1);
    let retried: JobSummary = ok(h
        .call("POST", &wire_jobs::retry_path(&job.id), Some(TOKEN), None)
        .await);
    assert_eq!((retried.state.as_str(), retried.attempt), ("queued", 0));
    assert!(h.claim(&w, 0).await.job.is_some());
    assert_eq!(
        error_of(
            h.call("POST", &wire_jobs::retry_path(&job.id), Some(TOKEN), None)
                .await
        ),
        (
            StatusCode::CONFLICT,
            "only a failed job can be retried; this one is leased".into()
        )
    );
}

/// A claim waits, and wakes as soon as a push queues something.
#[tokio::test]
async fn a_waiting_claim_wakes_when_a_job_is_queued() {
    let h = Arc::new(harness(|_| {}));
    let w = Arc::new(h.worker().await);
    ok::<PushResponse>(h.push("A", None).await);
    let claim = {
        let (h, w) = (h.clone(), w.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            (h.claim(&w, 20).await, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    ok::<PushResponse>(
        h.push("B", Some(&recall_wire::content_sha256("an older A")))
            .await,
    );
    let (claimed, took) = claim.await.unwrap();
    assert!(claimed.job.is_some());
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[tokio::test]
async fn claims_and_results_are_checked() {
    let h = harness(|_| {});
    let w = h.worker().await;
    for (body, want) in [
        (
            json!({"kinds": ["merge"], "wait_seconds": 31}),
            "wait_seconds must be 0 to 30",
        ),
        (
            json!({"kinds": ["merge"], "lease_seconds": 29}),
            "lease_seconds must be 30 to 600",
        ),
        (
            json!({"kinds": ["merge"], "lease_seconds": 601}),
            "lease_seconds must be 30 to 600",
        ),
        (json!({"wait_seconds": 1}), "invalid json body"),
    ] {
        assert_eq!(
            error_of(h.signed(&w, "POST", CLAIM_PATH, Some(body)).await),
            (StatusCode::BAD_REQUEST, want.into())
        );
    }
    h.conflict().await;
    let job = h.claim(&w, 0).await.job.unwrap();
    for body in [
        json!({"lease_id": job.lease_id}),
        json!({"lease_id": job.lease_id, "merge": {"content": "x"}, "error": "y"}),
    ] {
        assert_eq!(
            error_of(h.result(&w, &job.id, body).await),
            (
                StatusCode::BAD_REQUEST,
                "a result carries exactly one of merge and error".into()
            )
        );
    }
    assert_eq!(
        error_of(
            h.result(
                &w,
                "job_nothing",
                json!({"lease_id": job.lease_id, "error": "x"})
            )
            .await
        ),
        (StatusCode::NOT_FOUND, "no job has that id".into())
    );
    assert_eq!(
        error_of(
            h.call("GET", "/v1/jobs?state=stuck", Some(TOKEN), None)
                .await
        )
        .0,
        StatusCode::BAD_REQUEST
    );
}

// ---------------------------------------------------------------------------
// who may do what
// ---------------------------------------------------------------------------

/// A worker may claim jobs and post their results, and nothing else: its
/// key cannot read memory, write it outright, or manage devices.
#[tokio::test]
async fn a_worker_can_do_nothing_but_its_jobs() {
    let h = harness(|_| {});
    let w = h.worker().await;
    ok::<PushResponse>(h.push("secret", None).await);
    let forbidden = |(status, body): (StatusCode, Bytes)| {
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let err: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("secret"));
        err.error
    };
    assert_eq!(
        forbidden(
            h.signed(&w, "GET", &format!("/sync?project_key={P}"), None)
                .await
        ),
        "forbidden: a worker device may only claim jobs and post their results"
    );
    forbidden(
        h.signed(
            &w,
            "POST",
            "/sync",
            Some(json!({"project_key": P, "file_path": F, "content": "overwritten"})),
        )
        .await,
    );
    forbidden(h.signed(&w, "GET", devices::DEVICES_ME_PATH, None).await);
    for (method, uri, body) in [
        ("GET", "/admin/stats", None),
        ("GET", devices::DEVICES_PATH, None),
        (
            "POST",
            devices::APPROVE_PATH,
            Some(json!({"user_code": "BCDF-GHJK", "scope": "admin"})),
        ),
        (
            "POST",
            devices::ENROLL_KEYS_PATH,
            Some(json!({"expires_in_days": 1})),
        ),
        ("GET", wire_jobs::JOBS_PATH, None),
        ("POST", "/v1/jobs/job_x/retry", None),
    ] {
        assert_eq!(
            forbidden(h.signed(&w, method, uri, body).await),
            "forbidden: this needs RECALL_TOKEN or a device with the admin scope",
            "{method} {uri}"
        );
    }
    assert_eq!(h.stored().await.0, "secret");
}

/// The job routes are a worker's alone: another scope, or the operator's
/// token, has proved who it is and is refused with 403.
#[tokio::test]
async fn only_a_worker_may_claim_or_post_a_result() {
    let h = harness(|_| {});
    h.worker().await;
    let mut laptop = Machine::new(41, "laptop");
    h.enrol(&mut laptop, SCOPE_SYNC).await;
    let mut admin = Machine::new(42, "admin");
    h.enrol(&mut admin, SCOPE_ADMIN).await;
    let claim = json!({"kinds": ["merge"]});
    let result = json!({"lease_id": "lse_x", "error": "x"});
    let refusal = (
        StatusCode::FORBIDDEN,
        "forbidden: this needs a device with the worker scope".to_string(),
    );
    for m in [&laptop, &admin] {
        assert_eq!(
            error_of(h.signed(m, "POST", CLAIM_PATH, Some(claim.clone())).await),
            refusal
        );
        assert_eq!(
            error_of(
                h.signed(m, "POST", "/v1/jobs/job_x/result", Some(result.clone()))
                    .await
            ),
            refusal
        );
    }
    assert_eq!(
        error_of(
            h.call("POST", CLAIM_PATH, Some(TOKEN), Some(claim.clone()))
                .await
        ),
        refusal
    );
    assert_eq!(
        error_of(h.call("POST", CLAIM_PATH, None, Some(claim)).await),
        (StatusCode::UNAUTHORIZED, "unauthorized".into())
    );
    // The owner's listing: token or admin device.
    ok::<JobList>(h.call("GET", "/v1/jobs", Some(TOKEN), None).await);
    ok::<JobList>(h.signed(&admin, "GET", "/v1/jobs", None).await);
    assert_eq!(
        h.signed(&laptop, "GET", "/v1/jobs", None).await.0,
        StatusCode::FORBIDDEN
    );
}

/// An enrolment key makes sync devices only, so a leaked one cannot mint a
/// worker; and approving takes the worker scope by name only.
#[tokio::test]
async fn a_worker_is_made_only_by_an_approval_that_names_it() {
    let h = harness(|_| {});
    let key: recall_wire::EnrollKeyCreated = ok(h
        .call(
            "POST",
            devices::ENROLL_KEYS_PATH,
            Some(TOKEN),
            Some(json!({"tag": "cloud", "expires_in_days": 1})),
        )
        .await);
    let m = Machine::new(43, "cloud");
    let approved: recall_wire::EnrollApproved = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "worker", "public_key": encode_public_key(&m.key.verifying_key()), "enroll_key": key.key})),
        )
        .await);
    assert_eq!(approved.scope, "sync");
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": "BCDF-GHJK", "scope": "root"}))
            )
            .await
        ),
        (
            StatusCode::BAD_REQUEST,
            "scope must be sync, admin or worker".into()
        )
    );
}

// ---------------------------------------------------------------------------
// what the server says about it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_shows_the_worker_and_the_queue_only_once_one_is_enrolled() {
    let h = harness_with_claude(0, "x");
    let (_, body) = h.call("GET", "/health", None, None).await;
    let raw: Value = serde_json::from_slice(&body).unwrap();
    let merge = raw["merge"].as_object().unwrap();
    assert!(!merge.contains_key("worker") && !merge.contains_key("queue"));
    assert_eq!(
        raw["merge"]["claude_cli"]["logged_in"],
        json!(true),
        "the server's own CLI"
    );

    let w = h.worker().await;
    let health: Health = ok(h.call("GET", "/health", None, None).await);
    let worker = health.merge.worker.expect("worker");
    assert_eq!(worker.last_claim_at, None, "no claim yet");
    assert_eq!(worker.agent, "recall-worker/test");
    assert_eq!(
        health.merge.claude_cli.logged_in, None,
        "the worker has not reported"
    );

    h.conflict().await;
    ok::<ClaimResponse>(
        h.signed(
            &w,
            "POST",
            CLAIM_PATH,
            Some(json!({"kinds": [], "claude_cli": {"checked_at": "2026-10-02T09:13:40.002Z", "available": true, "logged_in": false, "error": "not logged in"}})),
        )
        .await,
    );
    let health: Health = ok(h.call("GET", "/health", None, None).await);
    let worker = health.merge.worker.unwrap();
    assert!(worker.last_claim_at.is_some());
    assert_eq!(worker.agent, "recall-worker/test (linux-x86_64)");
    assert_eq!(health.merge.claude_cli.logged_in, Some(false));
    assert_eq!(health.merge.claude_cli.error, "not logged in");
    let queue = health.merge.queue.unwrap();
    assert_eq!((queue.queued, queue.leased, queue.failed), (1, 0, 0));
    assert!(queue.oldest_queued_at.is_some());
}

#[tokio::test]
async fn discovery_lists_the_merge_queue() {
    let h = harness(|_| {});
    let doc: recall_wire::Discovery = ok(h.call("GET", "/.well-known/recall", None, None).await);
    assert!(doc.can("merge_queue"));
}

// ---------------------------------------------------------------------------
// the real worker
// ---------------------------------------------------------------------------

/// End to end: the `recall-worker` library, enrolling by code and merging
/// with a stand-in `claude`, against this server on a real socket. A
/// conflict's merged file arrives with the next pull.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_a_conflicts_merge_arrives_with_the_next_pull() {
    let (_claude, bin) = fake_claude(0, "merged by the worker");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Server::new(
        Config {
            token: TOKEN.to_string(),
            rate_limit_max: 10_000,
            // The server's own CLI is not logged in: every merge here is
            // the worker's.
            claude_bin: "definitely-not-a-real-claude".into(),
            ..Config::default()
        },
        store,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_server, server_stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        server
            .serve_with_shutdown(listener, async {
                let _ = server_stopped.await;
            })
            .await
    });
    let url = format!("http://{addr}");
    let http = reqwest::Client::new();

    let worker_dir = tempfile::tempdir().unwrap();
    let cfg = recall_worker::config::Config {
        url: url.clone(),
        data_dir: worker_dir.path().to_path_buf(),
        claude_bin: bin,
        wait_seconds: 2,
        ..recall_worker::config::Config::default()
    };
    let worker = recall_worker::worker::Worker::new(cfg).unwrap();
    let (stop_worker, worker_stopped) = tokio::sync::oneshot::channel::<()>();
    let working = tokio::spawn(worker.run(async {
        let _ = worker_stopped.await;
    }));

    // The owner approves the code the worker printed, which it also keeps
    // in its identity file.
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        let id = recall_worker::identity::Identity::load_or_create(worker_dir.path()).unwrap();
        if let Some(code) = id.user_code {
            break code;
        }
        assert!(Instant::now() < deadline, "the worker never enrolled");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let approved = http
        .post(format!("{url}{}", devices::APPROVE_PATH))
        .bearer_auth(TOKEN)
        .json(&json!({"user_code": code, "scope": "worker"}))
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), 200);

    let push = |content: &'static str, base: Option<String>| {
        let http = http.clone();
        let url = url.clone();
        async move {
            let mut body = json!({"project_key": P, "file_path": F, "content": content, "source_env": "laptop"});
            if let Some(base) = base {
                body["base_sha256"] = json!(base);
            }
            let resp = http
                .post(format!("{url}/sync"))
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            resp.json::<PushResponse>().await.unwrap()
        }
    };
    // Pushed until the worker is approved and the conflict is queued
    // rather than stored as sent: the poll that collects the approval
    // takes a few seconds. `A` from `B`'s base is always a plain write,
    // and `B` from an older base always a conflict.
    let deadline = Instant::now() + Duration::from_secs(30);
    let job = loop {
        push("A", Some(recall_wire::content_sha256("B"))).await;
        let pushed = push("B", Some(recall_wire::content_sha256("an older A"))).await;
        if let Some(job) = pushed.merge_job {
            break job;
        }
        assert!(Instant::now() < deadline, "the worker was never approved");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let pulled: SyncResponse = http
            .get(format!("{url}/sync?project_key={P}"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let file = pulled.files.iter().find(|f| f.file_path == F).unwrap();
        if file.content.as_deref() == Some("merged by the worker") {
            assert_eq!(file.source_env, "worker");
            break;
        }
        assert!(Instant::now() < deadline, "job {job} was never merged");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let health: Health = http
        .get(format!("{url}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        health.merge.claude_cli.logged_in,
        Some(true),
        "the worker's CLI"
    );
    assert!(health
        .merge
        .worker
        .unwrap()
        .agent
        .starts_with("recall-worker/"));

    let _ = stop_worker.send(());
    tokio::time::timeout(Duration::from_secs(10), working)
        .await
        .expect("the worker stops on shutdown")
        .unwrap()
        .unwrap();
    let _ = stop_server.send(());
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .expect("the server stops on shutdown")
        .unwrap()
        .unwrap();
}
