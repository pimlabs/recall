//! Evaluation reports through the router: asking for one, the worker
//! claiming it with the files it reads, the report it posts back, and who
//! may read what. The properties `docs/design/part5-plan.md` pins for PR 6
//! at the server: a report's note text stays in `details`; a finding with
//! any other key is refused; an evaluation changes no memory row; the
//! contradiction check is asked for only by name, never by the schedule.
//! The last test runs the real `recall-worker` against this server on a
//! real socket, with a stand-in `claude` that counts its calls.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use recall_server::{Config, Server, Store};
use recall_wire::devices::{self, SCOPE_ADMIN, SCOPE_SYNC, SCOPE_WORKER};
use recall_wire::evaluations::{evaluation_path, EVALUATIONS_PATH};
use recall_wire::jobs::{self as wire_jobs, CLAIM_PATH};
use recall_wire::signature::{self, encode_public_key, SigningKey, Target};
use recall_wire::{
    ClaimResponse, Details, Device, EnrollPending, ErrorResponse, Evaluation, EvaluationCreated,
    EvaluationList, Health, Job, ResultResponse,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "evaluations-test-token";
const HOST: &str = "recall.test";
const P: &str = "acme/app";
const G: &str = "global:eko";

struct Harness {
    server: Server,
    dir: TempDir,
    store: Arc<Store>,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Server::new(
        Config {
            token: TOKEN.to_string(),
            rate_limit_max: 10_000,
            merge_enabled: false,
            ..Config::default()
        },
        store.clone(),
    );
    server.backdate_start(600);
    Harness { server, dir, store }
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

fn nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("eval-nonce-{}", NEXT.fetch_add(1, Ordering::Relaxed))
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

impl Harness {
    async fn send(&self, req: Request<Body>) -> (StatusCode, Bytes) {
        let resp = self.server.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, body)
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Bytes) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {TOKEN}"));
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
        let (path, query) = match uri.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (uri, None),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
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
            now,
            &nonce(),
        )
        .unwrap();
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", HOST)
            .header("content-type", "application/json")
            .header(recall_wire::PROTOCOL_HEADER, "1")
            .header(signature::CONTENT_DIGEST_HEADER, headers.content_digest)
            .header(signature::SIGNATURE_INPUT_HEADER, headers.signature_input)
            .header(signature::SIGNATURE_HEADER, headers.signature)
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }

    async fn enrol(&self, m: &mut Machine, scope: &str) {
        let pending: EnrollPending = ok(self
            .call(
                "POST",
                devices::ENROLL_PATH,
                Some(json!({"name": m.name, "public_key": encode_public_key(&m.key.verifying_key()), "agent": "recall-worker/test"})),
            )
            .await);
        let device: Device = ok(self
            .call(
                "POST",
                devices::APPROVE_PATH,
                Some(json!({"user_code": pending.user_code, "scope": scope})),
            )
            .await);
        m.id = device.id;
    }

    async fn worker(&self) -> Machine {
        let mut w = Machine::new(50, "worker");
        self.enrol(&mut w, SCOPE_WORKER).await;
        w
    }

    async fn put(&self, project_key: &str, file_path: &str, content: &str) {
        let _: Value = ok(self
            .call(
                "POST",
                "/sync",
                Some(json!({"project_key": project_key, "file_path": file_path,
                            "content": content, "source_env": "laptop"})),
            )
            .await);
    }

    /// Two projects and a global scope, with a secret, a duplicate and a
    /// dead link among them.
    async fn seed(&self) {
        self.put(P, "MEMORY.md", "- [Deploy](deploy.md)\n- [Gone](gone.md)\n")
            .await;
        self.put(P, "deploy.md", "# Deploy\n- key: SENTINEL-note-text\n")
            .await;
        self.put("acme/web", "notes.md", "- the web app runs on port 3000\n")
            .await;
        self.put(G, "tools.md", "- I use vim\n").await;
    }

    async fn request(&self, body: Value) -> EvaluationCreated {
        ok(self.call("POST", EVALUATIONS_PATH, Some(body)).await)
    }

    async fn claim(&self, w: &Machine) -> Job {
        let claimed: ClaimResponse = ok(self
            .signed(
                w,
                "POST",
                CLAIM_PATH,
                Some(json!({"kinds": ["evaluate"], "wait_seconds": 0, "lease_seconds": 60})),
            )
            .await);
        claimed.job.expect("a job to claim")
    }

    async fn result(&self, w: &Machine, id: &str, body: Value) -> (StatusCode, Bytes) {
        self.signed(w, "POST", &wire_jobs::result_path(id), Some(body))
            .await
    }

    async fn evaluation(&self, id: &str) -> Evaluation {
        ok(self.call("GET", &evaluation_path(id), None).await)
    }

    /// Every row of memory, every column, in order.
    fn memory(&self) -> Vec<Vec<String>> {
        let conn = rusqlite::Connection::open(self.dir.path().join("recall.db")).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT project_key, file_path, content, COALESCE(source_env, '<null>'), \
                 updated_at, deleted FROM memory_files ORDER BY project_key, file_path",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(vec![
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?.to_string(),
            ])
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }
}

/// A report such as the worker posts: a secret on line 2 of `deploy.md`,
/// the dead link on line 2 of `MEMORY.md`, and details that quote them,
/// with a suggested edit.
fn report(lease_id: &str) -> Value {
    json!({
        "lease_id": lease_id,
        "evaluate": {
            "findings": [
                {"id": "f1", "kind": "secret", "severity": "high", "project_key": P,
                 "file_path": "deploy.md", "lines": [2, 2], "related": []},
                {"id": "f2", "kind": "dead_link", "severity": "medium", "project_key": P,
                 "file_path": "MEMORY.md", "lines": [2, 2],
                 "related": [{"project_key": G, "file_path": "tools.md"}]}
            ],
            "details": {
                "findings": {
                    "f1": {"excerpt": "- key: SENTINEL-note-text\n", "reasoning": "a key",
                           "suggested_edit": {"project_key": P, "file_path": "deploy.md",
                               "base_sha256": recall_wire::content_sha256("# Deploy\n- key: SENTINEL-note-text\n"),
                               "lines": [2, 2], "replacement": ""}},
                    "f2": {"excerpt": "- [Gone](gone.md)\n", "reasoning": "gone",
                           "suggested_edit": null}
                },
                "skipped": []
            }
        }
    })
}

// ---------------------------------------------------------------------------
// asking, claiming, reporting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_evaluation_is_asked_for_claimed_and_reported() {
    let h = harness();
    h.seed().await;
    assert_eq!(
        error_of(h.call("POST", EVALUATIONS_PATH, Some(json!({}))).await).0,
        StatusCode::CONFLICT,
        "nothing would take it without a worker"
    );
    let w = h.worker().await;
    assert_eq!(
        error_of(
            h.call(
                "POST",
                EVALUATIONS_PATH,
                Some(json!({"projects": ["no/such"]}))
            )
            .await
        ),
        (
            StatusCode::BAD_REQUEST,
            "no project has the key \"no/such\"".into()
        )
    );
    let created = h
        .request(json!({"projects": [P, P], "contradictions": false}))
        .await;
    assert!(created.id.starts_with("eval_"), "{created:?}");
    assert_eq!(created.state, "queued");

    let listed: EvaluationList = ok(h.call("GET", EVALUATIONS_PATH, None).await);
    assert_eq!(listed.evaluations.len(), 1);
    assert_eq!(listed.evaluations[0].state, "queued");
    assert_eq!(
        listed.evaluations[0].projects,
        vec![P.to_string()],
        "deduplicated"
    );
    assert_eq!(listed.evaluations[0].finished_at, None);

    let job = h.claim(&w).await;
    assert_eq!(job.id, created.job);
    let input = job.evaluate.clone().unwrap();
    assert_eq!(input.evaluation_id, created.id);
    assert!(!input.contradictions);
    // The project asked for and the global scope; not the other project.
    let files: Vec<(&str, &str)> = input
        .files
        .iter()
        .map(|f| (f.project_key.as_str(), f.file_path.as_str()))
        .collect();
    assert_eq!(files, [(P, "MEMORY.md"), (P, "deploy.md"), (G, "tools.md")]);
    // An evaluation gets the longest lease, whatever the claim asked for.
    let expires = time::OffsetDateTime::parse(
        &job.lease_expires_at,
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert!(expires - time::OffsetDateTime::now_utc() > time::Duration::seconds(590));
    assert_eq!(h.evaluation(&created.id).await.state, "running");

    let settled: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
    assert_eq!((settled.state.as_str(), settled.applied), ("done", false));
    // The same report again is recorded once, and answered the same.
    let again: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
    assert_eq!(again, settled);

    let shown = h.evaluation(&created.id).await;
    assert_eq!(shown.state, "done");
    assert!(shown.finished_at.is_some());
    assert_eq!(shown.findings.len(), 2);
    let details: Details = serde_json::from_value(shown.details.unwrap()).unwrap();
    assert!(details.findings["f1"].suggested_edit.is_some());
    let listed: EvaluationList = ok(h.call("GET", EVALUATIONS_PATH, None).await);
    assert_eq!(
        serde_json::to_value(&listed.evaluations[0].counts).unwrap(),
        json!({"dead_link": 1, "secret": 1})
    );
    assert_eq!(
        error_of(h.call("GET", &evaluation_path("eval_none"), None).await),
        (StatusCode::NOT_FOUND, "no evaluation has that id".into())
    );
}

/// The API serves a report's note text only as `details`: the listing,
/// the findings, the job listing and the audit log carry none of it.
#[tokio::test]
async fn a_reports_note_text_stays_in_its_details() {
    let h = harness();
    h.seed().await;
    let w = h.worker().await;
    let created = h.request(json!({})).await;
    let job = h.claim(&w).await;
    let _: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);

    let shown: Value = ok(h.call("GET", &evaluation_path(&created.id), None).await);
    assert!(shown["details"].to_string().contains("SENTINEL"));
    let mut without = shown.clone();
    without["details"] = Value::Null;
    for (what, text) in [
        ("the evaluation, but for its details", without.to_string()),
        (
            "the listing",
            String::from_utf8(h.call("GET", EVALUATIONS_PATH, None).await.1.to_vec()).unwrap(),
        ),
        (
            "the jobs",
            String::from_utf8(h.call("GET", wire_jobs::JOBS_PATH, None).await.1.to_vec()).unwrap(),
        ),
        (
            "the audit log",
            String::from_utf8(
                h.call(
                    "GET",
                    &format!(
                        "/v1/audit/entries?start=0&end={}",
                        h.store.audit_checkpoint().0
                    ),
                    None,
                )
                .await
                .1
                .to_vec(),
            )
            .unwrap(),
        ),
    ] {
        assert!(!text.contains("SENTINEL"), "{what}: {text}");
    }
}

/// A finding holds its enums, its file, its lines and related files, and
/// nothing else: a result with any other key, anywhere in a finding, is
/// refused whole, and so is one naming a file the server does not hold.
#[tokio::test]
async fn a_finding_with_any_other_key_is_refused() {
    let h = harness();
    h.seed().await;
    let w = h.worker().await;
    let created = h.request(json!({})).await;
    let job = h.claim(&w).await;
    let refused = |edit: &dyn Fn(&mut Value)| {
        let mut body = report(&job.lease_id);
        edit(&mut body["evaluate"]["findings"][0]);
        body
    };
    let cases: Vec<(Value, &str)> = vec![
        (
            refused(&|f| f["excerpt"] = json!("- key: SENTINEL-note-text")),
            "finding f1 has a key \"excerpt\"",
        ),
        (
            refused(&|f| {
                f["related"] = json!([{"project_key": G, "file_path": "tools.md", "why": "x"}])
            }),
            "has a key \"why\"",
        ),
        (
            refused(&|f| f["id"] = json!("key: SENTINEL")),
            "an id is f and a number",
        ),
        (
            refused(&|f| f["file_path"] = json!("SENTINEL-note-text.md")),
            "finding f1 names a file this server does not hold",
        ),
        (
            refused(&|f| f["kind"] = json!("gossip")),
            "no kind \"gossip\" exists",
        ),
    ];
    for (body, want) in cases {
        let (status, why) = error_of(h.result(&w, &job.id, body).await);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(why.contains(want), "{why}");
        assert!(
            !why.contains("SENTINEL-note"),
            "the refusal quotes the note: {why}"
        );
    }
    let mut not_object = report(&job.lease_id);
    not_object["evaluate"]["details"] = json!("sealed later");
    assert_eq!(
        error_of(h.result(&w, &job.id, not_object).await),
        (StatusCode::BAD_REQUEST, "details is not an object".into())
    );
    // Nothing was recorded: the run is still the worker's to finish.
    let shown = h.evaluation(&created.id).await;
    assert_eq!((shown.state.as_str(), shown.findings.len()), ("running", 0));
    let _: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
}

/// Nothing an evaluation does changes memory: every row, every column, is
/// what it was before the run was asked for, its suggested edits included.
#[tokio::test]
async fn an_evaluation_changes_no_memory_row() {
    let h = harness();
    h.seed().await;
    let w = h.worker().await;
    let before = h.memory();
    let created = h.request(json!({"contradictions": true})).await;
    let job = h.claim(&w).await;
    let _: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
    assert_eq!(h.evaluation(&created.id).await.state, "done");
    assert_eq!(h.memory(), before);
}

/// The contradiction check is asked for by name: a request without it, and
/// every scheduled run, leave it off. A scheduled run waits for a worker,
/// and is not queued behind another run.
#[tokio::test]
async fn contradictions_are_off_unless_asked_for_and_never_scheduled() {
    let h = harness();
    h.seed().await;
    assert_eq!(
        h.server.run_scheduled_evaluation().unwrap(),
        None,
        "no worker"
    );
    let w = h.worker().await;
    let scheduled = h.server.run_scheduled_evaluation().unwrap().unwrap();
    assert_eq!(
        h.server.run_scheduled_evaluation().unwrap(),
        None,
        "one open at a time"
    );
    let job = h.claim(&w).await;
    let input = job.evaluate.clone().unwrap();
    assert_eq!(input.evaluation_id, scheduled);
    assert!(!input.contradictions);
    assert!(input.projects.is_empty(), "every project");
    let _: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
    let leaves = h.store.audit_entries(0, 100, usize::MAX).unwrap();
    let leaf = leaves
        .iter()
        .map(|e| serde_json::from_slice::<Value>(&e.leaf).unwrap())
        .find(|l| l["action"] == "evaluate")
        .unwrap();
    assert_eq!(leaf["actor"], json!({"kind": "server"}));
    assert_eq!(leaf["subject"]["contradictions"], json!(false));

    for (body, want) in [
        (json!({}), false),
        (json!({"projects": [P]}), false),
        (json!({"contradictions": true}), true),
    ] {
        let created = h.request(body).await;
        let job = h.claim(&w).await;
        let input = job.evaluate.clone().unwrap();
        assert_eq!(input.evaluation_id, created.id);
        assert_eq!(input.contradictions, want);
        assert_eq!(h.evaluation(&created.id).await.contradictions, want);
        let _: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
    }
}

/// One run at a time: while one is queued or running another is refused,
/// so evaluations never pile up in the queue merges share. Once it is done,
/// or has failed, the next may be asked for.
#[tokio::test]
async fn a_second_evaluation_waits_for_the_first() {
    let h = harness();
    h.seed().await;
    let w = h.worker().await;
    let first = h.request(json!({})).await;
    let busy = format!(
        "evaluation {} is still queued or running; ask for another once it is done",
        first.id
    );
    assert_eq!(
        error_of(h.call("POST", EVALUATIONS_PATH, Some(json!({}))).await),
        (StatusCode::CONFLICT, busy.clone())
    );
    let job = h.claim(&w).await;
    assert_eq!(
        error_of(
            h.call(
                "POST",
                EVALUATIONS_PATH,
                Some(json!({"contradictions": true}))
            )
            .await
        ),
        (StatusCode::CONFLICT, busy)
    );
    assert_eq!(h.server.run_scheduled_evaluation().unwrap(), None);
    let _: ResultResponse = ok(h.result(&w, &job.id, report(&job.lease_id)).await);
    let second = h.request(json!({})).await;
    assert_ne!(second.id, first.id);
    let listed: EvaluationList = ok(h.call("GET", EVALUATIONS_PATH, None).await);
    assert_eq!(listed.evaluations.len(), 2);
}

/// Who may ask and read: an admin device and the operator; not a sync
/// device, and not a worker, which takes the job and nothing else. The
/// `evaluate` leaf keeps an admin device's signed body.
#[tokio::test]
async fn only_an_admin_asks_for_or_reads_an_evaluation() {
    let h = harness();
    h.seed().await;
    let _w = h.worker().await;
    let mut admin = Machine::new(51, "laptop");
    h.enrol(&mut admin, SCOPE_ADMIN).await;
    let mut sync = Machine::new(52, "phone");
    h.enrol(&mut sync, SCOPE_SYNC).await;
    let mut worker = Machine::new(53, "worker-2");
    h.enrol(&mut worker, SCOPE_WORKER).await;

    let body = json!({"projects": [P]});
    let created: EvaluationCreated = ok(h
        .signed(&admin, "POST", EVALUATIONS_PATH, Some(body.clone()))
        .await);
    let _: EvaluationList = ok(h.signed(&admin, "GET", EVALUATIONS_PATH, None).await);
    let _: Evaluation = ok(h
        .signed(&admin, "GET", &evaluation_path(&created.id), None)
        .await);
    for m in [&sync, &worker] {
        for (method, uri, body) in [
            ("POST", EVALUATIONS_PATH.to_string(), Some(body.clone())),
            ("GET", EVALUATIONS_PATH.to_string(), None),
            ("GET", evaluation_path(&created.id), None),
        ] {
            assert_eq!(
                error_of(h.signed(m, method, &uri, body).await).0,
                StatusCode::FORBIDDEN,
                "{} {method} {uri}",
                m.name
            );
        }
    }
    let leaves = h.store.audit_entries(0, 100, usize::MAX).unwrap();
    let leaf = leaves
        .iter()
        .map(|e| serde_json::from_slice::<Value>(&e.leaf).unwrap())
        .find(|l| l["action"] == "evaluate")
        .unwrap();
    assert_eq!(leaf["actor"]["id"], json!(admin.id));
    assert_eq!(
        leaf["subject"],
        json!({"evaluation_id": created.id, "job_id": created.job,
               "projects": [P], "contradictions": false})
    );
    assert_eq!(leaf["request"]["body"], json!(body.to_string()));
}

/// An evaluation is the worker's: the drain that merges a revoked worker's
/// queue leaves it waiting rather than failing it, and `/health`'s merge
/// queue does not count it.
#[tokio::test]
async fn an_evaluation_waits_for_a_worker_and_is_not_a_merge() {
    let h = harness();
    h.seed().await;
    let w = h.worker().await;
    let created = h.request(json!({})).await;
    let health: Health = ok(h.call("GET", "/health", None).await);
    assert_eq!(health.merge.queue.unwrap().queued, 0);
    assert_eq!(
        h.call("POST", &devices::revoke_device_path(&w.id), Some(json!({})))
            .await
            .0,
        StatusCode::OK
    );
    h.server.drain_jobs().await.unwrap();
    assert_eq!(h.evaluation(&created.id).await.state, "queued");
}

#[tokio::test]
async fn discovery_lists_evaluation() {
    let h = harness();
    let doc: recall_wire::Discovery = ok(h.call("GET", "/.well-known/recall", None).await);
    assert!(doc.can(recall_wire::discovery::CAPABILITY_EVALUATION));
}

/// The evaluation request fixtures a release shipped are still understood.
#[tokio::test]
async fn every_evaluation_request_fixture_is_understood() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../recall-wire/fixtures/wire");
    let h = harness();
    h.seed().await;
    let w = h.worker().await;
    let mut sent = 0;
    for version in std::fs::read_dir(&root).unwrap() {
        let version = version.unwrap().path();
        let read = |name: &str| {
            std::fs::read(version.join(name))
                .ok()
                .map(|b| serde_json::from_slice::<Value>(&b).unwrap())
        };
        if let Some(mut body) = read("evaluation_request.json") {
            // The fixture names the capture's project; this server has P.
            body["projects"] = json!([P]);
            let _: EvaluationCreated = ok(h.call("POST", EVALUATIONS_PATH, Some(body)).await);
            sent += 1;
        }
        if let Some(body) = read("job_result_request_evaluate.json") {
            assert_eq!(
                error_of(h.result(&w, "job_scratch", body).await),
                (StatusCode::NOT_FOUND, "no job has that id".into())
            );
            sent += 1;
        }
    }
    assert!(sent >= 2, "found only {sent} evaluation request fixtures");
}

// ---------------------------------------------------------------------------
// the real worker
// ---------------------------------------------------------------------------

/// A stand-in `claude`, logged in, that counts its `-p` calls and answers
/// each with no contradictions.
fn counting_claude() -> (TempDir, String, std::path::PathBuf) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let calls = dir.path().join("calls");
    let path = dir.path().join("claude");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(
        f,
        r#"if [ "$1" = auth ]; then printf '%s' '{{"loggedIn":true}}'; exit 0; fi"#
    )
    .unwrap();
    writeln!(f, "echo call >> '{}'", calls.display()).unwrap();
    writeln!(f, "cat > /dev/null").unwrap();
    writeln!(
        f,
        r#"printf '%s' '{{"is_error":false,"result":"{{\"contradictions\":[]}}"}}'"#
    )
    .unwrap();
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..200 {
        match std::process::Command::new(&path).arg("auth").output() {
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(5)),
            _ => break,
        }
    }
    let bin = path.to_str().unwrap().to_string();
    (dir, bin, calls)
}

/// End to end: the `recall-worker` library, enrolled by code, makes the
/// reports a server asks for. Without the contradiction check it never
/// calls `claude`; with it, once for each project.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_to_end_the_worker_makes_the_report() {
    let (_claude, bin, calls) = counting_claude();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let server = Server::new(
        Config {
            token: TOKEN.to_string(),
            rate_limit_max: 10_000,
            merge_enabled: false,
            ..Config::default()
        },
        store,
    );
    server.backdate_start(600);
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
    let put = |project_key: &'static str, file_path: &'static str, content: &'static str| {
        let (http, url) = (http.clone(), url.clone());
        async move {
            let resp = http
                .post(format!("{url}/sync"))
                .bearer_auth(TOKEN)
                .json(&json!({"project_key": project_key, "file_path": file_path,
                              "content": content, "source_env": "laptop"}))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
        }
    };
    put(P, "MEMORY.md", "- [Gone](gone.md)\n").await;
    put(P, "a.md", "- the staging database listens on port 5433\n").await;
    put(
        "acme/web",
        "b.md",
        "- the staging database listens on port 5433\n",
    )
    .await;
    put(G, "tools.md", "- I use vim\n").await;

    let worker_dir = tempfile::tempdir().unwrap();
    let cfg = recall_worker::config::Config {
        server: url.clone(),
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
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        let id = recall_worker::identity::Identity::load(worker_dir.path()).unwrap();
        if let Some(code) = id.and_then(|id| id.user_code) {
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

    let run = |contradictions: bool| {
        let (http, url) = (http.clone(), url.clone());
        async move {
            let created: EvaluationCreated = http
                .post(format!("{url}{EVALUATIONS_PATH}"))
                .bearer_auth(TOKEN)
                .json(&json!({"contradictions": contradictions}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let shown: Evaluation = http
                    .get(format!("{url}{}", evaluation_path(&created.id)))
                    .bearer_auth(TOKEN)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if shown.state == "done" {
                    return shown;
                }
                assert!(Instant::now() < deadline, "{} was never made", created.id);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let count = || {
        std::fs::read_to_string(&calls)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };

    let shown = run(false).await;
    let kinds: Vec<&str> = shown.findings.iter().map(|f| f.kind.as_str()).collect();
    assert_eq!(kinds, ["dead_link", "duplicate"], "{:#?}", shown.findings);
    assert_eq!(
        count(),
        0,
        "a run without the contradiction check called claude"
    );

    let shown = run(true).await;
    assert_eq!(shown.findings.len(), 2);
    assert_eq!(count(), 2, "one call per project");

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
