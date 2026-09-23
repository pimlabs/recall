//! The audit log, end to end: every authenticated route appends exactly
//! one leaf, atomically with what it recorded; unauthenticated and refused
//! requests append nothing; the audit routes themselves append nothing;
//! and an exported log verifies offline with `scripts/audit-verify.py`,
//! which catches tampering.

use std::process::Command;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use recall_server::{Config, Server, Store};
use recall_wire::devices::{self, revoke_authkey_path, revoke_device_path};
use recall_wire::signature::{encode_public_key, SigningKey};
use recall_wire::{
    AuditCheckpoint, AuditEntriesResponse, AuthkeyCreated, Device, EnrollApproved, EnrollPending,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "audit-test-token";

struct Harness {
    server: Server,
    store: Arc<Store>,
    #[allow(dead_code)] // keeps the temp directory alive for the store's file
    dir: TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let cfg = Config {
        token: TOKEN.to_string(),
        merge_enabled: false,
        rate_limit_max: 10_000,
        ..Config::default()
    };
    Harness {
        server: Server::new(cfg, store.clone()),
        store,
        dir,
    }
}

impl Harness {
    async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let resp = self.server.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body)
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, Bytes) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = body.map(|b| Body::from(b.to_string())).unwrap_or_default();
        let (status, _, bytes) = self.send(req.body(body).unwrap()).await;
        (status, bytes)
    }

    /// The tree's current size, read straight from the store — the same
    /// number `GET /v1/audit/checkpoint` would answer with, without the
    /// noise of going through JSON for every assertion below.
    fn size(&self) -> u64 {
        self.store.audit_checkpoint().0
    }
}

fn ok<T: DeserializeOwned>((status, body): (StatusCode, Bytes)) -> T {
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

/// A machine, for the enrolment steps this file needs. Mirrors
/// `tests/devices.rs`'s own — kept separate since integration tests are
/// each their own crate and cannot share it.
struct Machine {
    key: SigningKey,
}

impl Machine {
    fn new(seed: u8) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
        }
    }
    fn public_key(&self) -> String {
        encode_public_key(&self.key.verifying_key())
    }
}

/// Every authenticated route the design's PR 1 table names appends exactly
/// one leaf; the two unauthenticated enrolment steps, the audit routes
/// themselves, and a refused request append none.
#[tokio::test]
async fn every_authenticated_route_appends_exactly_one_leaf() {
    let h = harness();
    // Server::new already appended one `start` leaf.
    assert_eq!(h.size(), 1, "start");

    let n = h.size();
    let (status, _) = h
        .call(
            "POST",
            "/sync",
            Some(TOKEN),
            Some(json!({"project_key":"a/b","file_path":"m.md","content":"hi"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "push");

    let n = h.size();
    let (status, _) = h
        .call("GET", "/sync?project_key=a/b", Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "pull");

    let n = h.size();
    let (status, _) = h
        .call(
            "POST",
            "/sync",
            Some(TOKEN),
            Some(json!({"project_key":"a/b","file_path":"m.md","deleted":true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "delete");

    let laptop = Machine::new(1);
    let n = h.size();
    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name":"laptop","public_key": laptop.public_key(), "agent":"t"})),
        )
        .await);
    assert_eq!(h.size(), n, "enrolling (unauthenticated) appends nothing");

    let n = h.size();
    let device: Device = ok(h
        .call(
            "POST",
            devices::APPROVE_PATH,
            Some(TOKEN),
            Some(json!({"user_code": pending.user_code})),
        )
        .await);
    assert_eq!(h.size(), n + 1, "approve");

    let phone = Machine::new(2);
    let pending2: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name":"phone","public_key": phone.public_key(), "agent":"t"})),
        )
        .await);
    let n = h.size();
    let (status, _) = h
        .call(
            "POST",
            devices::DENY_PATH,
            Some(TOKEN),
            Some(json!({"user_code": pending2.user_code})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "deny");

    let n = h.size();
    let key: AuthkeyCreated = ok(h
        .call(
            "POST",
            devices::AUTHKEYS_PATH,
            Some(TOKEN),
            Some(json!({"tag":"cloud","expires_in_days":90})),
        )
        .await);
    assert_eq!(h.size(), n + 1, "authkey_create");

    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_authkey_path(&key.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "authkey_revoke");

    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_device_path(&device.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "revoke");

    // Revoking the same device again is not a new fact: no second leaf.
    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_device_path(&device.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.size(),
        n,
        "revoking twice appends nothing the second time"
    );

    let n = h.size();
    let (status, _) = h
        .call("GET", "/v1/audit/checkpoint", Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n, "the checkpoint route appends nothing");
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/audit/entries?start=0&end={n}"),
            Some(TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n, "the entries route appends nothing");
    let (status, _) = h
        .call(
            "GET",
            &format!("/v1/audit/consistency?first=1&second={n}"),
            Some(TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n, "the consistency route appends nothing");

    let n = h.size();
    let (status, _) = h
        .call("GET", "/sync?project_key=a/b", Some("wrong-token"), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(h.size(), n, "an unauthorized request appends nothing");

    let (status, _) = h
        .call(
            "POST",
            devices::APPROVE_PATH,
            Some(TOKEN),
            Some(json!({"user_code":"ZZZZ-ZZZZ"})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(h.size(), n, "a refused approve appends nothing");
}

/// The per-device sweep appends one leaf per device it actually removes.
#[tokio::test]
async fn the_sweep_appends_one_leaf_per_device_removed() {
    let h = harness();
    let key: AuthkeyCreated = ok(h
        .call(
            "POST",
            devices::AUTHKEYS_PATH,
            Some(TOKEN),
            Some(json!({"tag":"cloud","expires_in_days":90,"ephemeral":true})),
        )
        .await);
    let ephemeral = Machine::new(3);
    let _approved: EnrollApproved = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({
                "name": "ignored",
                "public_key": ephemeral.public_key(),
                "agent": "t",
                "authkey": key.key,
            })),
        )
        .await);

    // Age it out: the same technique tests/devices.rs uses, direct SQL
    // against the store's file, for what a test cannot wait a day for.
    {
        let conn = rusqlite::Connection::open(h.dir.path().join("recall.db")).unwrap();
        conn.execute(
            "UPDATE devices SET created_at = '2000-01-01T00:00:00.000Z', last_seen = NULL",
            [],
        )
        .unwrap();
    }

    let n = h.size();
    let (removed, _) = h.server.sweep_devices().unwrap();
    assert_eq!(removed, 1);
    assert_eq!(h.size(), n + 1, "one device removed, one leaf");

    // Sweeping again finds nothing left to remove: no more leaves.
    let n = h.size();
    let (removed, _) = h.server.sweep_devices().unwrap();
    assert_eq!(removed, 0);
    assert_eq!(h.size(), n, "nothing left to sweep, nothing appended");
}

/// A log exported the way `GET /v1/audit/entries` hands it back — a
/// checkpoint line, then one leaf per line — verifies with
/// `scripts/audit-verify.py`, and a single changed byte, dropped leaf, or
/// swapped pair of leaves makes it fail.
#[tokio::test]
async fn an_export_verifies_offline_and_tampering_is_caught() {
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/audit-verify.py");
    if !script.exists() {
        panic!("scripts/audit-verify.py not found at {}", script.display());
    }

    let h = harness();
    for i in 0..6 {
        h.call(
            "POST",
            "/sync",
            Some(TOKEN),
            Some(json!({"project_key":"a/b","file_path": format!("m{i}.md"), "content":"x"})),
        )
        .await;
    }

    let cp: AuditCheckpoint = ok(h
        .call("GET", "/v1/audit/checkpoint", Some(TOKEN), None)
        .await);
    let entries: AuditEntriesResponse = ok(h
        .call(
            "GET",
            &format!("/v1/audit/entries?start=0&end={}", cp.tree_size),
            Some(TOKEN),
            None,
        )
        .await);
    assert_eq!(entries.entries.len(), cp.tree_size as usize);

    let mut export = format!("{} {}\n", cp.tree_size, cp.root_hash);
    for e in &entries.entries {
        export.push_str(e);
        export.push('\n');
    }

    let export_dir = tempfile::tempdir().unwrap();
    let path = export_dir.path().join("audit.jsonl");

    let run = |contents: &str| -> bool {
        std::fs::write(&path, contents).unwrap();
        Command::new("python3")
            .arg(&script)
            .arg(&path)
            .status()
            .expect("python3 must be on PATH")
            .success()
    };

    assert!(run(&export), "a clean export must verify");

    let changed_byte = export.replacen("m2.md", "m9.md", 1);
    assert!(!run(&changed_byte), "a changed byte must be caught");

    let lines: Vec<&str> = export.lines().collect();
    let mut dropped = lines[..3].to_vec();
    dropped.extend_from_slice(&lines[4..]);
    let dropped = dropped.join("\n") + "\n";
    assert!(!run(&dropped), "a missing leaf must be caught");

    let mut swapped = lines.clone();
    swapped.swap(3, 4);
    let swapped = swapped.join("\n") + "\n";
    assert!(!run(&swapped), "two swapped leaves must be caught");

    // A saved checkpoint the log does not extend: one from an earlier,
    // different tree of the same size.
    let bogus = format!(
        "--checkpoint={}:{}",
        cp.tree_size, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    );
    std::fs::write(&path, &export).unwrap();
    let ok_status = Command::new("python3")
        .arg(&script)
        .arg(&bogus)
        .arg(&path)
        .status()
        .unwrap();
    assert!(
        !ok_status.success(),
        "a checkpoint the log does not extend must be caught"
    );
}
