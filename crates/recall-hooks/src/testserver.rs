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
    fail_with: Option<(u16, String)>,
    last_authorization: Option<String>,
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

    pub fn last_authorization(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("test lock")
            .last_authorization
            .clone()
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

type Shared = Arc<Mutex<Inner>>;

/// Returns the configured failure, if the test asked for one, and records
/// the credentials that arrived.
fn intercept(state: &Shared, headers: &HeaderMap) -> Option<Response> {
    let mut inner = state.lock().expect("test lock");
    inner.last_authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
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
    let files = {
        let mut inner = state.lock().expect("test lock");
        inner.pulled_keys.push(q.project_key.clone());
        inner
            .files
            .get(&q.project_key)
            .or_else(|| inner.files.get(""))
            .cloned()
            .unwrap_or_default()
    };
    Json(SyncResponse {
        project_key: q.project_key,
        files,
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
    };
    state.lock().expect("test lock").pushes.push(req);
    Json(response).into_response()
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
