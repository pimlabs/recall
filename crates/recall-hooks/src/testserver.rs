//! A real HTTP server for the tests, standing in for the Recall server.
//!
//! Deliberately not a mocked client trait: the Go tests used a real
//! `httptest` server and that is what caught serialization bugs — a
//! tombstone that serialized without its `deleted` flag, an empty file
//! indistinguishable from a delete. A mock agreeing with the code under test
//! would have found neither.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use recall_wire::{File, Health, PushRequest, PushResponse, SyncResponse};
use serde::Deserialize;
use tokio::task::JoinHandle;

#[derive(Default)]
struct Inner {
    pushes: Vec<PushRequest>,
    /// What `GET /sync` serves for a given `project_key`. The empty key is
    /// the fallback, so a test that does not care about scopes can keep
    /// calling `set_files`.
    files: HashMap<String, Vec<File>>,
    pulled_keys: Vec<String>,
    /// Every `POST /sync` that arrived, refused ones included. `pushes`
    /// records only the ones that were accepted, which makes "it stopped
    /// after the first refusal" and "it kept going and was refused every
    /// time" indistinguishable — the exact difference a backfill's stop rule
    /// turns on.
    push_attempts: usize,
    fail_with: Option<(u16, String)>,
    /// Failing *only* pushes, which `fail_with` cannot express. A backfill
    /// reads what the server holds before it sends anything, so the case
    /// worth testing — the read succeeds, a write is refused partway — needs
    /// the two halves to be able to disagree.
    fail_pushes_with: Option<(u16, String)>,
    last_authorization: Option<String>,
    /// The `User-Agent` and protocol header of the last request, so a test
    /// can see what the client says about itself.
    last_user_agent: Option<String>,
    last_protocol: Option<String>,
    /// When set, `/admin/stats` answers 401 to any other bearer token —
    /// the one place the fake enforces auth, because `recall connect` has
    /// to be able to tell a server that is up from a token that is right.
    required_token: Option<String>,
    /// Every request that arrived, on any path, answered or not.
    requests: usize,
    /// When set, every request is answered with this redirect: a status
    /// and a `Location`.
    redirect: Option<(u16, String)>,
    /// The fake's audit log, once a test turns it on: each leaf's bytes. A
    /// pull appends one and answers with the checkpoint over them, as the
    /// real server does, and the checkpoint and consistency routes answer
    /// from them. Rewriting one is what a server that rewrote history
    /// looks like from outside.
    audit: Option<Vec<Vec<u8>>>,
    /// Consistency proofs answered before the fake's rate limit refuses
    /// every one after: [`None`] for no limit.
    proofs_before_limit: Option<usize>,
    /// Whether a consistency proof is never answered at all, as by a
    /// server too slow to wait for.
    hang_proofs: bool,
}

pub struct FakeServer {
    pub url: String,
    inner: Arc<Mutex<Inner>>,
    handle: JoinHandle<()>,
}

impl FakeServer {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let app = Router::new()
            .route("/sync", get(pull).post(push))
            .route("/health", get(health))
            .route("/admin/stats", get(admin_stats))
            .route(recall_wire::audit::CHECKPOINT_PATH, get(audit_checkpoint))
            .route(recall_wire::audit::CONSISTENCY_PATH, get(audit_consistency))
            .fallback(elsewhere)
            .with_state(inner.clone());

        // Port 0: the OS picks a free port, so tests can run in parallel.
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind a test port");
        let addr = listener.local_addr().expect("read the test port");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            url: format!("http://{addr}"),
            inner,
            handle,
        }
    }

    /// Every push the server received, in order.
    pub fn pushes(&self) -> Vec<PushRequest> {
        self.inner.lock().expect("test lock").pushes.clone()
    }

    /// What `GET /sync` will serve for any key that has nothing of its own.
    pub fn set_files(&self, files: Vec<File>) {
        self.inner
            .lock()
            .expect("test lock")
            .files
            .insert(String::new(), files);
    }

    /// What `GET /sync` will serve for one specific key.
    ///
    /// Scopes are only visible from outside as different keys, so a test that
    /// wants to prove routing has to be able to serve them differently.
    pub fn set_files_for(&self, project_key: &str, files: Vec<File>) {
        self.inner
            .lock()
            .expect("test lock")
            .files
            .insert(project_key.to_string(), files);
    }

    /// Every key that has been fetched, in order — so a test can assert that
    /// a scope was *not* consulted.
    pub fn pulled_keys(&self) -> Vec<String> {
        self.inner.lock().expect("test lock").pulled_keys.clone()
    }

    /// Make every subsequent request fail with this status and body.
    pub fn fail_with(&self, code: u16, body: &str) {
        self.inner.lock().expect("test lock").fail_with = Some((code, body.to_string()));
    }

    /// Answers every request as before [`FakeServer::fail_with`] again.
    pub fn stop_failing(&self) {
        self.inner.lock().expect("test lock").fail_with = None;
    }

    /// How many pushes were attempted, whether or not they were accepted.
    pub fn push_attempts(&self) -> usize {
        self.inner.lock().expect("test lock").push_attempts
    }

    /// How many requests arrived, on any path.
    pub fn requests(&self) -> usize {
        self.inner.lock().expect("test lock").requests
    }

    /// Answer every subsequent request with a redirect to `location`.
    pub fn redirect_to(&self, code: u16, location: &str) {
        self.inner.lock().expect("test lock").redirect = Some((code, location.to_string()));
    }

    /// Make every subsequent `POST /sync` fail, leaving `GET /sync` working.
    pub fn fail_pushes_with(&self, code: u16, body: &str) {
        self.inner.lock().expect("test lock").fail_pushes_with = Some((code, body.to_string()));
    }

    /// The `User-Agent` and `Recall-Protocol` of the last request.
    pub fn last_identity(&self) -> (Option<String>, Option<String>) {
        let inner = self.inner.lock().expect("test lock");
        (inner.last_user_agent.clone(), inner.last_protocol.clone())
    }

    pub fn last_authorization(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("test lock")
            .last_authorization
            .clone()
    }
}

impl FakeServer {
    /// Starts keeping an audit log of `leaves` leaves, each pull adding
    /// one.
    pub fn keep_audit_log(&self, leaves: usize) {
        let log = (0..leaves)
            .map(|i| format!("leaf {i}").into_bytes())
            .collect();
        self.inner.lock().expect("test lock").audit = Some(log);
    }

    /// Appends `n` more leaves, as other machines' requests would.
    pub fn grow_audit_log(&self, n: usize) {
        let mut inner = self.inner.lock().expect("test lock");
        let log = inner.audit.as_mut().expect("an audit log");
        for _ in 0..n {
            let i = log.len();
            log.push(format!("leaf {i}").into_bytes());
        }
    }

    /// Rewrites leaf `i`: every checkpoint from before, at a size past it,
    /// no longer holds.
    pub fn rewrite_audit_leaf(&self, i: usize) {
        let mut inner = self.inner.lock().expect("test lock");
        inner.audit.as_mut().expect("an audit log")[i] = format!("rewritten {i}").into_bytes();
    }

    /// Cuts the log back to `n` leaves, as restoring a backup does.
    pub fn truncate_audit_log(&self, n: usize) {
        let mut inner = self.inner.lock().expect("test lock");
        inner.audit.as_mut().expect("an audit log").truncate(n);
    }

    /// Answers `n` more consistency proofs, then refuses each after as its
    /// rate limit would; [`None`] lifts the limit.
    pub fn limit_proofs_after(&self, n: Option<usize>) {
        self.inner.lock().expect("test lock").proofs_before_limit = n;
    }

    /// Never answers a consistency proof from now on.
    pub fn hang_proofs(&self) {
        self.inner.lock().expect("test lock").hang_proofs = true;
    }

    /// The log's checkpoint now.
    pub fn audit_checkpoint(&self) -> recall_wire::AuditCheckpoint {
        checkpoint_of(
            self.inner
                .lock()
                .expect("test lock")
                .audit
                .as_deref()
                .unwrap_or_default(),
        )
    }
}

fn leaf_hashes(log: &[Vec<u8>]) -> Vec<recall_wire::audit::merkle::Hash> {
    log.iter()
        .map(|l| recall_wire::audit::merkle::hash_leaf(l))
        .collect()
}

fn checkpoint_of(log: &[Vec<u8>]) -> recall_wire::AuditCheckpoint {
    use base64::Engine;
    recall_wire::AuditCheckpoint {
        tree_size: log.len() as u64,
        root_hash: base64::engine::general_purpose::STANDARD
            .encode(recall_wire::audit::merkle::root(&leaf_hashes(log))),
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

type Shared = Arc<Mutex<Inner>>;

impl FakeServer {
    /// Makes `/admin/stats` refuse every token but `token`.
    pub fn require_token(&self, token: &str) {
        self.inner.lock().expect("test lock").required_token = Some(token.to_string());
    }
}

/// Returns the configured failure, if the test asked for one, and records
/// the credentials that arrived.
fn intercept(state: &Shared, headers: &HeaderMap) -> Option<Response> {
    let mut inner = state.lock().expect("test lock");
    inner.requests += 1;
    inner.last_authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    inner.last_user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    inner.last_protocol = headers
        .get(recall_wire::PROTOCOL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some((code, location)) = inner.redirect.clone() {
        return Some(
            (
                StatusCode::from_u16(code).expect("a valid test status"),
                [(axum::http::header::LOCATION, location)],
            )
                .into_response(),
        );
    }
    inner.fail_with.clone().map(|(code, body)| {
        (
            StatusCode::from_u16(code).expect("a valid test status"),
            body,
        )
            .into_response()
    })
}

#[derive(Deserialize)]
struct PullQuery {
    #[serde(default)]
    project_key: String,
}

async fn pull(
    State(state): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<PullQuery>,
) -> Response {
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    let (files, checkpoint) = {
        let mut inner = state.lock().expect("test lock");
        inner.pulled_keys.push(q.project_key.clone());
        let checkpoint = inner.audit.as_mut().map(|log| {
            let i = log.len();
            log.push(format!("pull {i}").into_bytes());
            checkpoint_of(log)
        });
        let files = inner
            .files
            .get(&q.project_key)
            .or_else(|| inner.files.get(""))
            .cloned()
            .unwrap_or_default();
        (files, checkpoint)
    };
    let mut response = Json(SyncResponse {
        project_key: q.project_key,
        files,
    })
    .into_response();
    if let Some(cp) = checkpoint {
        response.headers_mut().insert(
            recall_wire::audit::CHECKPOINT_HEADER,
            cp.to_header_value().parse().expect("a header value"),
        );
    }
    response
}

async fn audit_checkpoint(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    match &state.lock().expect("test lock").audit {
        Some(log) => Json(checkpoint_of(log)).into_response(),
        None => (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#).into_response(),
    }
}

#[derive(Deserialize)]
struct ConsistencyQuery {
    first: u64,
    second: u64,
}

async fn audit_consistency(
    State(state): State<Shared>,
    headers: HeaderMap,
    Query(q): Query<ConsistencyQuery>,
) -> Response {
    use base64::Engine;
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    if state.lock().expect("test lock").hang_proofs {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
    let mut inner = state.lock().expect("test lock");
    if let Some(left) = inner.proofs_before_limit.as_mut() {
        if *left == 0 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":"too many requests"}"#,
            )
                .into_response();
        }
        *left -= 1;
    }
    let Some(log) = &inner.audit else {
        return (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#).into_response();
    };
    if q.first < 1 || q.first > q.second || q.second > log.len() as u64 {
        return (StatusCode::BAD_REQUEST, r#"{"error":"bad range"}"#).into_response();
    }
    let proof = recall_wire::audit::merkle::consistency(q.first, q.second, &leaf_hashes(log));
    Json(recall_wire::AuditConsistencyResponse {
        first: q.first,
        second: q.second,
        proof: proof
            .iter()
            .map(|h| base64::engine::general_purpose::STANDARD.encode(h))
            .collect(),
    })
    .into_response()
}

async fn push(
    State(state): State<Shared>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    let refused = {
        let mut inner = state.lock().expect("test lock");
        inner.push_attempts += 1;
        inner.fail_pushes_with.clone()
    }
    .map(|(code, body)| {
        (
            StatusCode::from_u16(code).expect("a valid test status"),
            body,
        )
            .into_response()
    });
    if let Some(failure) = refused {
        return failure;
    }
    // Decoded from raw bytes rather than through an extractor so a
    // malformed body shows up here as a 400 rather than as a confusing
    // rejection message.
    let req: PushRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let response = PushResponse {
        ok: true,
        project_key: req.project_key.clone(),
        file_path: req.file_path.clone(),
        deleted: req.deleted,
        merged: false,
        updated_at: "2026-01-01T00:00:00.000Z".into(),
        merge_job: None,
    };
    state.lock().expect("test lock").pushes.push(req);
    Json(response).into_response()
}

async fn admin_stats(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    let inner = state.lock().expect("test lock");
    if let Some(want) = &inner.required_token {
        if inner.last_authorization.as_deref() != Some(format!("Bearer {want}").as_str()) {
            return (StatusCode::UNAUTHORIZED, r#"{"error":"unauthorized"}"#).into_response();
        }
    }
    Json(serde_json::json!({ "projects": [], "totals": {} })).into_response()
}

async fn health(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    Json(Health {
        status: "ok".into(),
        ..Default::default()
    })
    .into_response()
}

/// Any other path: counted, and answered as configured, or with a 404.
async fn elsewhere(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(failure) = intercept(&state, &headers) {
        return failure;
    }
    (StatusCode::NOT_FOUND, r#"{"error":"not found"}"#).into_response()
}
