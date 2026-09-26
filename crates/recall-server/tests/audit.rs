//! The audit log, end to end: every authenticated route appends exactly
//! one leaf, atomically with what it recorded; unauthenticated and refused
//! requests append nothing; the audit routes themselves append nothing;
//! who may read what; and an exported log verifies offline with
//! `scripts/audit-verify.py`, which refuses every forgery a server could
//! build from a log it was keeping honestly until then.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use recall_server::audit::merkle;
use recall_server::merge::Status;
use recall_server::{now, Config, Server, Store};
use recall_wire::devices::{self, revoke_authkey_path, revoke_device_path};
use recall_wire::jobs as wire_jobs;
use recall_wire::signature::{self, encode_public_key, SigningKey, Target};
use recall_wire::{
    AuditCheckpoint, AuditConsistencyResponse, AuditEntriesResponse, AuthkeyCreated, Device,
    EnrollApproved, EnrollPending,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "audit-test-token";
const HOST: &str = "recall.test";

struct Harness {
    server: Server,
    store: Arc<Store>,
    dir: TempDir,
}

fn harness_with(tweak: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let mut cfg = Config {
        token: TOKEN.to_string(),
        merge_enabled: false,
        rate_limit_max: 100_000,
        ..Config::default()
    };
    tweak(&mut cfg);
    let server = Server::new(cfg, store.clone());
    // Signatures made in the first seconds after a start are refused (see
    // `server/auth.rs`); these tests sign at once.
    server.backdate_start(600);
    Harness { server, store, dir }
}

fn harness() -> Harness {
    harness_with(|_| {})
}

/// A machine: a key pair, and the device id it was given.
struct Machine {
    key: SigningKey,
    id: String,
}

impl Machine {
    fn new(seed: u8) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
            id: String::new(),
        }
    }
    fn public_key(&self) -> String {
        encode_public_key(&self.key.verifying_key())
    }
}

fn fresh_nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("nonce-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn ok<T: DeserializeOwned>((status, body): (StatusCode, Bytes)) -> T {
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
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
        let (status, _, bytes) = self.send(req.body(body).unwrap()).await;
        (status, bytes)
    }

    /// A request `machine` signs, as the client signs one.
    async fn signed_raw(
        &self,
        machine: &Machine,
        method: &str,
        uri: &str,
        body: Vec<u8>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let (path, query) = match uri.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (uri, None),
        };
        let target = Target {
            method,
            authority: HOST,
            path,
            query,
        };
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let h = signature::sign_request(
            &machine.key,
            &machine.id,
            &target,
            "1",
            &body,
            created,
            &fresh_nonce(),
        )
        .unwrap();
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", HOST)
            .header("content-type", "application/json")
            .header(recall_wire::PROTOCOL_HEADER, "1")
            .header(signature::CONTENT_DIGEST_HEADER, h.content_digest)
            .header(signature::SIGNATURE_INPUT_HEADER, h.signature_input)
            .header(signature::SIGNATURE_HEADER, h.signature)
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }

    async fn signed(
        &self,
        machine: &Machine,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Bytes) {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let (status, _, bytes) = self.signed_raw(machine, method, uri, body).await;
        (status, bytes)
    }

    /// Enrols `machine` by code, approved by the operator with `scope`.
    async fn enrol(&self, machine: &mut Machine, name: &str, scope: &str) {
        let pending: EnrollPending = ok(self
            .call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": name, "public_key": machine.public_key(), "agent": "recall/test"})),
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
        machine.id = device.id;
    }

    async fn authkey(&self, ephemeral: bool) -> AuthkeyCreated {
        ok(self
            .call(
                "POST",
                devices::AUTHKEYS_PATH,
                Some(TOKEN),
                Some(json!({"tag": "cloud", "expires_in_days": 90, "ephemeral": ephemeral})),
            )
            .await)
    }

    /// Enrols `machine` with an authkey.
    async fn enrol_with(&self, machine: &mut Machine, authkey: &str) {
        let approved: EnrollApproved = ok(self
            .call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({
                    "name": "ignored",
                    "public_key": machine.public_key(),
                    "agent": "recall/test",
                    "authkey": authkey,
                })),
            )
            .await);
        machine.id = approved.device_id;
    }

    /// Ages every ephemeral device past the sweep's idle limit, directly in
    /// the database, for what a test cannot wait a day for.
    fn age_ephemeral_devices(&self) {
        let conn = rusqlite::Connection::open(self.dir.path().join("recall.db")).unwrap();
        conn.execute(
            "UPDATE devices SET created_at = '2000-01-01T00:00:00.000Z', last_seen = NULL \
             WHERE ephemeral = 1",
            [],
        )
        .unwrap();
    }

    /// The tree's current size, read straight from the store.
    fn size(&self) -> u64 {
        self.store.audit_checkpoint().0
    }

    /// Every leaf, parsed.
    async fn leaves(&self) -> Vec<Value> {
        self.leaf_strings()
            .await
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    async fn leaf_strings(&self) -> Vec<String> {
        let n = self.size();
        let mut out = Vec::new();
        while (out.len() as u64) < n {
            let page: AuditEntriesResponse = ok(self
                .call(
                    "GET",
                    &format!(
                        "/v1/audit/entries?start={}&end={}",
                        out.len(),
                        n.min(out.len() as u64 + 1000)
                    ),
                    Some(TOKEN),
                    None,
                )
                .await);
            assert!(!page.entries.is_empty());
            out.extend(page.entries);
        }
        out
    }

    /// An export as the verifier reads one: the checkpoint, then each leaf.
    async fn export(&self) -> String {
        let cp: AuditCheckpoint = ok(self
            .call("GET", "/v1/audit/checkpoint", Some(TOKEN), None)
            .await);
        let leaves = self.leaf_strings().await;
        assert_eq!(leaves.len() as u64, cp.tree_size);
        export_of(&leaves)
    }
}

/// An export of `leaves`, with a checkpoint line computed over them — what a
/// server that wrote those leaves would have answered.
fn export_of(leaves: &[String]) -> String {
    let hashes: Vec<merkle::Hash> = leaves
        .iter()
        .map(|l| merkle::hash_leaf(l.as_bytes()))
        .collect();
    let root = merkle::root(&hashes);
    use base64::Engine;
    let mut out = format!(
        "{} {}\n",
        leaves.len(),
        base64::engine::general_purpose::STANDARD.encode(root)
    );
    for l in leaves {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn checkpoint_of(leaves: &[String]) -> String {
    let line = export_of(leaves);
    let (size, root) = line.lines().next().unwrap().split_once(' ').unwrap();
    format!("--checkpoint={size}:{root}")
}

fn script() -> PathBuf {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/audit-verify.py");
    assert!(script.exists(), "no {}", script.display());
    script
}

/// Runs the verifier over `export` with `args`: its exit code and all it
/// printed. The built-in Ed25519 unless `args` says otherwise, so what is
/// tested does not depend on what this machine has installed.
fn verify(export: &str, args: &[&str]) -> (i32, String) {
    verify_with(export, args, &[])
}

fn verify_with(export: &str, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    std::fs::write(&path, export).unwrap();
    let mut cmd = Command::new("python3");
    cmd.arg(script()).arg(&path);
    if !args.iter().any(|a| a.starts_with("--ed25519")) {
        cmd.arg("--ed25519=builtin");
    }
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("python3 must be on PATH");
    let (code, out) = (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    );
    agrees_with_the_script(export, args, code, &out);
    (code, out)
}

/// `recall audit verify`'s verifier, `recall_wire::audit::verify`, gives
/// the script's verdict on the same export and the same saved checkpoints:
/// every honest log and every forgery in this file is also a test of the
/// second verifier, and a check dropped from either one shows up here as
/// the two disagreeing. Not asked when the script was told to skip
/// signatures, which the Rust one never does, or only to test its own
/// Ed25519, or could not run at all.
fn agrees_with_the_script(export: &str, args: &[&str], code: i32, out: &str) {
    let skipped = |a: &&str| matches!(*a, "--no-signatures" | "--self-test");
    if !matches!(code, 0 | 1) || args.iter().any(skipped) {
        return;
    }
    let saved: Vec<(u64, merkle::Hash)> = args
        .iter()
        .filter_map(|a| a.strip_prefix("--checkpoint="))
        .map(|c| {
            recall_wire::audit::verify::parse_checkpoint_arg(c)
                .unwrap_or_else(|| panic!("not a checkpoint: {c}"))
        })
        .collect();
    let verdict = recall_wire::audit::verify::verify_export(export.as_bytes(), &saved);
    assert_eq!(
        verdict.ok(),
        code == 0,
        "the script said {code}:\n{out}\nand recall audit verify said {:#?}",
        verdict.problems
    );
}

// ---------------------------------------------------------------------------
// what appends a leaf, and what does not
// ---------------------------------------------------------------------------

/// Every authenticated route the design's PR 1 table names appends exactly
/// one leaf; the two unauthenticated enrolment steps, the audit routes
/// themselves, and a refused request append none, and neither does
/// repeating a revoke that changes nothing.
#[tokio::test]
async fn every_authenticated_route_appends_exactly_one_leaf() {
    let h = harness();
    assert_eq!(h.size(), 0, "nothing until the server serves");

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
    let key = h.authkey(true).await;
    assert_eq!(h.size(), n + 1, "authkey_create");

    let n = h.size();
    let mut cloud = Machine::new(3);
    h.enrol_with(&mut cloud, &key.key).await;
    assert_eq!(h.size(), n + 1, "an authkey enrolment is an enroll leaf");

    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_authkey_path(&key.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "authkey_revoke");

    // The same revoke again changes nothing: no second leaf.
    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_authkey_path(&key.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n, "revoking a key twice appends nothing");

    // Its devices, asked for now, are a change: one leaf, naming them.
    let n = h.size();
    let (status, _) = h
        .call(
            "POST",
            &revoke_authkey_path(&key.id),
            Some(TOKEN),
            Some(json!({"revoke_devices": true})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "the cascade after the fact");
    let leaves = h.leaves().await;
    let last = leaves.last().unwrap();
    assert_eq!(last["action"], "authkey_revoke");
    assert_eq!(last["subject"]["revoked_devices"], json!([cloud.id]));
    let n = h.size();
    h.call(
        "POST",
        &revoke_authkey_path(&key.id),
        Some(TOKEN),
        Some(json!({"revoke_devices": true})),
    )
    .await;
    assert_eq!(h.size(), n, "and nothing when nothing is left to revoke");

    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_device_path(&device.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n + 1, "revoke");

    let n = h.size();
    let (status, _) = h
        .call("POST", &revoke_device_path(&device.id), Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h.size(), n, "revoking a device twice appends nothing");

    let n = h.size();
    for uri in [
        "/v1/audit/checkpoint".to_string(),
        format!("/v1/audit/entries?start=0&end={n}"),
        format!("/v1/audit/consistency?first=1&second={n}"),
    ] {
        let (status, _) = h.call("GET", &uri, Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(h.size(), n, "{uri} appends nothing");
    }

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

    // Every leaf the operator's requests made carries no request, and the
    // whole log verifies.
    let (code, out) = verify(&h.export().await, &[]);
    assert_eq!(code, 0, "{out}");
}

/// The `start` leaf is appended by the server that got its port and is
/// serving, not by building one.
#[tokio::test]
async fn the_start_leaf_is_appended_once_serving() {
    let h = harness();
    assert_eq!(h.size(), 0, "Server::new appends nothing");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Server::new(
        Config {
            token: TOKEN.to_string(),
            merge_enabled: false,
            ..Config::default()
        },
        h.store.clone(),
    );
    let serving = tokio::spawn(async move {
        server
            .serve_with_shutdown(listener, async {
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await
    });
    drop(tokio::net::TcpStream::connect(addr).await.unwrap());
    tokio::time::timeout(Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let leaves = h.leaves().await;
    assert_eq!(leaves.len(), 1);
    assert_eq!(leaves[0]["action"], "start");
    assert_eq!(leaves[0]["actor"], json!({"kind": "server"}));
    assert_eq!(leaves[0]["request"], Value::Null);
}

/// The per-device sweep appends one leaf per device it removes, and none
/// when nothing is idle.
#[tokio::test]
async fn the_sweep_appends_one_leaf_per_device_removed() {
    let h = harness();
    let key = h.authkey(true).await;
    let (mut a, mut b) = (Machine::new(3), Machine::new(4));
    h.enrol_with(&mut a, &key.key).await;
    h.enrol_with(&mut b, &key.key).await;
    h.age_ephemeral_devices();

    let n = h.size();
    let (removed, _) = h.server.sweep_devices().unwrap();
    assert_eq!(removed, 2);
    assert_eq!(h.size(), n + 2, "two devices removed, two leaves");
    let leaves = h.leaves().await;
    let mut swept: Vec<&str> = leaves[n as usize..]
        .iter()
        .map(|l| {
            assert_eq!(l["action"], "sweep");
            l["subject"]["device_id"].as_str().unwrap()
        })
        .collect();
    swept.sort();
    let mut want = vec![a.id.as_str(), b.id.as_str()];
    want.sort();
    assert_eq!(swept, want);

    let n = h.size();
    let (removed, _) = h.server.sweep_devices().unwrap();
    assert_eq!(removed, 0);
    assert_eq!(h.size(), n, "nothing left to sweep, nothing appended");
}

// ---------------------------------------------------------------------------
// what a leaf says
// ---------------------------------------------------------------------------

/// Enrolling with an authkey appends exactly one leaf: `enroll`, by the key
/// (its id and tag, never the key), carrying the device's public key.
#[tokio::test]
async fn an_authkey_enrolment_appends_one_leaf_carrying_the_public_key() {
    let h = harness();
    let key = h.authkey(true).await;
    let mut cloud = Machine::new(9);
    let n = h.size();
    h.enrol_with(&mut cloud, &key.key).await;
    assert_eq!(h.size(), n + 1);

    let leaves = h.leaf_strings().await;
    let leaf: Value = serde_json::from_str(leaves.last().unwrap()).unwrap();
    assert_eq!(leaf["action"], "enroll");
    assert_eq!(
        leaf["actor"],
        json!({"kind": "authkey", "id": key.id, "tag": "cloud"})
    );
    assert_eq!(leaf["subject"]["device_id"], json!(cloud.id));
    assert_eq!(leaf["subject"]["public_key"], json!(cloud.public_key()));
    assert_eq!(leaf["subject"]["authkey_id"], json!(key.id));
    assert_eq!(leaf["subject"]["ephemeral"], json!(true));
    assert_eq!(leaf["subject"]["scope"], json!("sync"));
    assert_eq!(leaf["request"], Value::Null);
    for l in &leaves {
        assert!(!l.contains(&key.key), "the authkey itself is in the log");
    }
}

/// A device's own requests are recorded with what it signed, and the leaf
/// that made it with its public key — enough to check the signature from
/// the log alone. Checked here with `recall_wire`'s own verifier; the
/// offline one below checks the same.
#[tokio::test]
async fn a_signed_leaf_keeps_its_request_and_its_device_leaf_the_key() {
    let h = harness();
    let mut laptop = Machine::new(5);
    h.enrol(&mut laptop, "laptop", "admin").await;
    let (status, _) = h
        .signed(
            &laptop,
            "POST",
            "/sync",
            Some(json!({"project_key":"a/b","file_path":"m.md","content":"hi"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let deny_body = json!({"user_code": "ZZZZ-ZZZZ"}).to_string();
    let (status, _, _) = h
        .signed_raw(&laptop, "POST", devices::DENY_PATH, deny_body.into_bytes())
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "refused, so no leaf");

    let leaves = h.leaves().await;
    let approve = leaves.iter().find(|l| l["action"] == "approve").unwrap();
    assert_eq!(approve["subject"]["device_id"], json!(laptop.id));
    assert_eq!(approve["subject"]["public_key"], json!(laptop.public_key()));
    let key =
        signature::parse_public_key(approve["subject"]["public_key"].as_str().unwrap()).unwrap();

    let push = leaves.iter().find(|l| l["action"] == "push").unwrap();
    assert_eq!(push["actor"]["kind"], "device");
    assert_eq!(push["actor"]["id"], json!(laptop.id));
    let request = &push["request"];
    assert_eq!(
        request["body"],
        Value::Null,
        "a push does not keep its body"
    );
    let base = request["signature_base"].as_str().unwrap();
    assert!(base.contains(&format!("keyid=\"{}\"", laptop.id)), "{base}");
    assert!(base.contains(&format!(
        "\"content-digest\": sha-256=:{}:",
        request["body_sha256"].as_str().unwrap()
    )));
    use base64::Engine;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(request["signature"].as_str().unwrap())
        .unwrap();
    signature::verify(&key, base, &sig).expect("the kept signature verifies");
}

/// The device-management actions keep their body beside the signature, so
/// what was asked for is bound to what the leaf says was done; their body
/// is capped, since it is kept.
#[tokio::test]
async fn a_device_management_leaf_keeps_its_small_body() {
    let h = harness();
    let mut admin = Machine::new(6);
    h.enrol(&mut admin, "admin", "admin").await;
    let body = json!({"tag": "ci", "expires_in_days": 1, "ephemeral": false}).to_string();
    let (status, _, resp) = h
        .signed_raw(
            &admin,
            "POST",
            devices::AUTHKEYS_PATH,
            body.clone().into_bytes(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
    let leaves = h.leaves().await;
    let leaf = leaves.last().unwrap();
    assert_eq!(leaf["action"], "authkey_create");
    assert_eq!(leaf["request"]["body"], json!(body));
    let created: AuthkeyCreated = serde_json::from_slice(&resp).unwrap();
    assert!(
        !leaf.to_string().contains(&created.key),
        "the key made in answer is not in the log"
    );

    let n = h.size();
    let (status, body) = h
        .call(
            "POST",
            devices::AUTHKEYS_PATH,
            Some(TOKEN),
            Some(json!({"tag": "x", "expires_in_days": 1, "padding": "p".repeat(9000)})),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap(),
        json!({"error": "request body too large"})
    );
    assert_eq!(h.size(), n);
}

/// A push says whether the server merged it, which is why its
/// `stored_sha256` may not be the hash of what was sent.
#[tokio::test]
async fn a_merged_push_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("claude");
    std::fs::write(
        &bin,
        "#!/bin/sh\ncat > /dev/null\nprintf '%s' '{\"is_error\":false,\"result\":\"A and B\"}'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let h = harness_with(|c| {
        c.merge_enabled = true;
        c.claude_bin = bin.to_str().unwrap().to_string();
        c.merge_timeout = Duration::from_secs(20);
    });
    h.server.set_claude_status(Status {
        checked_at: now(),
        available: true,
        logged_in: true,
        error: String::new(),
    });
    for content in ["A", "B"] {
        let (status, _) = h
            .call(
                "POST",
                "/sync",
                Some(TOKEN),
                Some(json!({"project_key": "a/b", "file_path": "m.md", "content": content})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let leaves = h.leaves().await;
    assert_eq!(leaves[0]["subject"]["merged"], json!(false));
    assert_eq!(leaves[1]["subject"]["merged"], json!(true));
    assert_eq!(
        leaves[1]["subject"]["stored_sha256"],
        json!(recall_wire::content_sha256("A and B"))
    );
}

/// What a push names goes into its leaf, so it is held to what a client
/// sends: a base that is a SHA-256 (kept lowercase), a key and a path no
/// longer than a path.
#[tokio::test]
async fn what_a_leaf_names_is_bounded() {
    let h = harness();
    let push = |body: Value| h.call("POST", "/sync", Some(TOKEN), Some(body));
    let base = recall_wire::content_sha256("x").to_uppercase();
    let (status, _) =
        push(json!({"project_key":"a/b","file_path":"m.md","content":"y","base_sha256": base}))
            .await;
    assert_eq!(status, StatusCode::OK);
    let leaves = h.leaves().await;
    assert_eq!(
        leaves[0]["subject"]["base_sha256"],
        json!(recall_wire::content_sha256("x")),
        "stored lowercase"
    );

    let n = h.size();
    for (body, want) in [
        (
            json!({"project_key":"a/b","file_path":"m.md","content":"y","base_sha256":"abc"}),
            "base_sha256 must be 64 hexadecimal characters",
        ),
        (
            json!({"project_key":"k".repeat(4097),"file_path":"m.md","content":"y"}),
            "project_key must be at most 4096 bytes",
        ),
        (
            json!({"project_key":"a/b","file_path":"f".repeat(4097),"content":"y"}),
            "file_path must be at most 4096 bytes",
        ),
    ] {
        let (status, resp) = push(body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_slice::<Value>(&resp).unwrap(),
            json!({ "error": want })
        );
    }
    let (status, _) = h
        .call(
            "GET",
            &format!("/sync?project_key={}", "k".repeat(4097)),
            Some(TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(h.size(), n, "no leaf for any of them");
}

/// Every pull leaves the client the checkpoint that includes its own leaf.
#[tokio::test]
async fn a_pull_carries_the_checkpoint_it_leaves() {
    let h = harness();
    let (status, headers, _) = h
        .send(
            Request::builder()
                .uri("/sync?project_key=a/b")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let header = headers
        .get(recall_wire::audit::CHECKPOINT_HEADER)
        .expect("a checkpoint header")
        .to_str()
        .unwrap();
    let cp: AuditCheckpoint = ok(h
        .call("GET", "/v1/audit/checkpoint", Some(TOKEN), None)
        .await);
    assert_eq!(header, cp.to_header_value());
    assert_eq!(cp.tree_size, 1, "the pull's own leaf");
}

/// The tree comes back after a restart: the same checkpoint, and the next
/// leaf takes the next seq.
#[tokio::test]
async fn the_log_survives_a_restart() {
    let h = harness();
    for i in 0..5 {
        h.call(
            "POST",
            "/sync",
            Some(TOKEN),
            Some(json!({"project_key":"a/b","file_path": format!("{i}.md"),"content":"x"})),
        )
        .await;
    }
    let before = h.store.audit_checkpoint();
    let reopened = Store::open(h.dir.path().join("recall.db")).unwrap();
    assert_eq!(reopened.audit_checkpoint(), before);
}

// ---------------------------------------------------------------------------
// who may read it
// ---------------------------------------------------------------------------

/// The leaves name every project, file, device and authkey, so only the
/// operator or an admin device reads them; the checkpoint and the proofs,
/// hashes only, are for any credential.
#[tokio::test]
async fn entries_needs_an_admin_and_the_hashes_do_not() {
    let h = harness();
    let mut sync = Machine::new(7);
    h.enrol(&mut sync, "sync", "sync").await;
    let mut admin = Machine::new(8);
    h.enrol(&mut admin, "admin", "admin").await;
    let n = h.size();

    let entries = format!("/v1/audit/entries?start=0&end={n}");
    let (status, body) = h.signed(&sync, "GET", &entries, None).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (status, _) = h.signed(&admin, "GET", &entries, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h.call("GET", &entries, Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = h.signed(&sync, "GET", "/v1/audit/checkpoint", None).await;
    assert_eq!(status, StatusCode::OK);
    let proof: AuditConsistencyResponse = ok(h
        .signed(
            &sync,
            "GET",
            &format!("/v1/audit/consistency?first=1&second={n}"),
            None,
        )
        .await);
    assert_eq!((proof.first, proof.second), (1, n));
    assert_eq!(h.size(), n, "reading appends nothing");
}

/// `end` past the log, a range the wrong way round, and a page over 1,000
/// are refused; a page whose leaves come to more than 2 MiB stops early and
/// says where.
#[tokio::test]
async fn entries_is_bounded_by_range_count_and_bytes() {
    let h = harness();
    // Leaves of about 8 KiB each: a key and a path of the longest allowed.
    let key = "k".repeat(4096);
    for i in 0..300 {
        let (status, _) = h
            .call(
                "POST",
                "/sync",
                Some(TOKEN),
                Some(json!({"project_key": key, "file_path": format!("{i:04}{}", "f".repeat(4000)), "content": "x"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let n = h.size();
    for uri in [
        format!("/v1/audit/entries?start=0&end={}", n + 1),
        "/v1/audit/entries?start=5&end=4".to_string(),
        "/v1/audit/entries?start=0".to_string(),
    ] {
        let (status, _) = h.call("GET", &uri, Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
    for i in n..1001 {
        let _ = i;
        h.call("GET", "/sync?project_key=a", Some(TOKEN), None)
            .await;
    }
    let (status, _) = h
        .call(
            "GET",
            "/v1/audit/entries?start=0&end=1001",
            Some(TOKEN),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "1,001 is over the page");

    let page: AuditEntriesResponse = ok(h
        .call(
            "GET",
            "/v1/audit/entries?start=0&end=300",
            Some(TOKEN),
            None,
        )
        .await);
    let bytes: usize = page.entries.iter().map(String::len).sum();
    assert!(page.entries.len() < 300, "{} leaves", page.entries.len());
    assert!(bytes <= recall_wire::audit::MAX_PAGE_BYTES, "{bytes} bytes");
    assert_eq!(page.end, page.start + page.entries.len() as u64);
    let next: AuditEntriesResponse = ok(h
        .call(
            "GET",
            &format!("/v1/audit/entries?start={}&end=300", page.end),
            Some(TOKEN),
            None,
        )
        .await);
    let first: Value = serde_json::from_str(&next.entries[0]).unwrap();
    assert_eq!(
        first["seq"],
        json!(page.end),
        "the next page goes on from end"
    );
}

// ---------------------------------------------------------------------------
// verifying offline
// ---------------------------------------------------------------------------

/// A log with every kind of leaf — a device approved by code, a device an
/// authkey enrolled, both signing, the second swept — verifies offline:
/// every signature against the key its own device leaf carries, even for a
/// device no longer in the `devices` table.
async fn a_full_log() -> (Harness, Vec<String>) {
    let h = harness();
    let mut laptop = Machine::new(11);
    h.enrol(&mut laptop, "laptop", "admin").await;
    let push = json!({"project_key":"a/b","file_path":"m.md","content":"hi"});
    assert_eq!(
        h.signed(&laptop, "POST", "/sync", Some(push)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        h.signed(&laptop, "GET", "/sync?project_key=a%2Fb", None)
            .await
            .0,
        StatusCode::OK
    );
    let delete = json!({"project_key":"a/b","file_path":"m.md","deleted":true});
    assert_eq!(
        h.signed(&laptop, "POST", "/sync", Some(delete)).await.0,
        StatusCode::OK
    );

    let key: AuthkeyCreated = ok(h
        .signed(
            &laptop,
            "POST",
            devices::AUTHKEYS_PATH,
            Some(json!({"tag": "cloud", "expires_in_days": 90, "max_devices": 3})),
        )
        .await);
    let mut cloud = Machine::new(12);
    h.enrol_with(&mut cloud, &key.key).await;
    assert_eq!(
        h.signed(&cloud, "GET", "/sync?project_key=a/b", None)
            .await
            .0,
        StatusCode::OK
    );
    let push = json!({"project_key":"a/b","file_path":"c.md","content":"from the cloud"});
    assert_eq!(
        h.signed(&cloud, "POST", "/sync", Some(push)).await.0,
        StatusCode::OK
    );

    let mut phone = Machine::new(13);
    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "phone", "public_key": phone.public_key()})),
        )
        .await);
    let approved: Device = ok(h
        .signed(
            &laptop,
            "POST",
            devices::APPROVE_PATH,
            Some(json!({"user_code": pending.user_code.to_lowercase(), "scope": "sync"})),
        )
        .await);
    phone.id = approved.id;
    assert_eq!(
        h.signed(&laptop, "POST", &revoke_device_path(&phone.id), None)
            .await
            .0,
        StatusCode::OK
    );
    let other: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "tablet", "public_key": Machine::new(14).public_key()})),
        )
        .await);
    assert_eq!(
        h.signed(
            &laptop,
            "POST",
            devices::DENY_PATH,
            Some(json!({"user_code": other.user_code}))
        )
        .await
        .0,
        StatusCode::OK
    );

    h.age_ephemeral_devices();
    assert_eq!(h.server.sweep_devices().unwrap().0, 1);
    assert_eq!(
        h.signed(
            &laptop,
            "POST",
            &revoke_authkey_path(&key.id),
            Some(json!({"revoke_devices": false}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let leaves = h.leaf_strings().await;
    (h, leaves)
}

#[tokio::test]
async fn a_signed_log_verifies_offline_even_after_a_sweep() {
    let (h, leaves) = a_full_log().await;
    let actions: Vec<String> = leaves
        .iter()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["action"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    for want in [
        "approve",
        "push",
        "pull",
        "delete",
        "authkey_create",
        "enroll",
        "revoke",
        "deny",
        "sweep",
        "authkey_revoke",
    ] {
        assert!(
            actions.iter().any(|a| a == want),
            "no {want} in {actions:?}"
        );
    }
    let swept: Value = serde_json::from_str(
        leaves
            .iter()
            .find(|l| l.contains("\"action\":\"sweep\""))
            .unwrap(),
    )
    .unwrap();
    assert!(h
        .store
        .device(swept["subject"]["device_id"].as_str().unwrap())
        .unwrap()
        .is_none());

    let export = h.export().await;
    let (code, out) = verify(&export, &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("10 signed leaves, every signature checked"),
        "{out}"
    );

    // And with every earlier checkpoint as one saved along the way.
    let mut args = Vec::new();
    for size in [1, 5, leaves.len() - 1] {
        args.push(checkpoint_of(&leaves[..size]));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, out) = verify(&export, &args);
    assert_eq!(code, 0, "{out}");
}

/// The tree check alone: a changed byte, a dropped leaf, two swapped ones,
/// or a saved checkpoint the log does not extend.
#[tokio::test]
async fn an_export_verifies_offline_and_tampering_is_caught() {
    let (h, _) = a_full_log().await;
    let export = h.export().await;
    assert_eq!(verify(&export, &[]).0, 0);

    let changed_byte = export.replacen("c.md", "d.md", 1);
    assert_eq!(verify(&changed_byte, &[]).0, 1, "a changed byte");

    let lines: Vec<&str> = export.lines().collect();
    let mut dropped = lines[..3].to_vec();
    dropped.extend_from_slice(&lines[4..]);
    assert_eq!(
        verify(&(dropped.join("\n") + "\n"), &[]).0,
        1,
        "a missing leaf"
    );

    let mut swapped = lines.clone();
    swapped.swap(3, 4);
    assert_eq!(
        verify(&(swapped.join("\n") + "\n"), &[]).0,
        1,
        "two swapped"
    );

    let size = lines.len() - 1;
    let bogus = format!("--checkpoint={size}:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    let (code, out) = verify(&export, &[&bogus]);
    assert_eq!(code, 1, "a checkpoint the log does not extend: {out}");
    assert!(out.contains("does not extend"), "{out}");
}

/// Each way a server could make a log that is internally consistent — its
/// own root recomputed, every earlier checkpoint still extended — yet says
/// something a device never asked for, and the verifier's answer to it.
#[tokio::test]
async fn forged_exports_are_refused() {
    let (_h, honest) = a_full_log().await;
    let saved = checkpoint_of(&honest);
    let parsed: Vec<Value> = honest
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let find = |action: &str, kind: &str| -> Value {
        parsed
            .iter()
            .find(|l| l["action"] == action && l["actor"]["kind"] == kind)
            .unwrap_or_else(|| panic!("no {action} by a {kind}"))
            .clone()
    };
    let (push, pull, approve) = (
        find("push", "device"),
        find("pull", "device"),
        find("approve", "operator"),
    );

    // The honest log with `extra` appended (renumbered to follow it, and
    // dated as the last leaf is), its root recomputed: a server that went on
    // writing, never rewriting, so the saved checkpoint still holds.
    let last_at = parsed.last().unwrap()["at"].clone();
    let appended = |extra: Vec<Value>| -> String {
        let mut leaves = honest.clone();
        for mut leaf in extra {
            leaf["seq"] = json!(leaves.len());
            leaf["at"] = last_at.clone();
            leaves.push(serde_json::to_string(&leaf).unwrap());
        }
        export_of(&leaves)
    };
    // The honest log with the leaf at `leaf`'s own seq replaced by it: a
    // rewrite, so no saved checkpoint survives it, but one a server with
    // no checkpoint against it could get away with were signatures and
    // requests not checked.
    let in_place = |leaf: Value| -> String {
        let mut leaves = honest.clone();
        let at = leaf["seq"].as_u64().unwrap() as usize;
        leaves[at] = serde_json::to_string(&leaf).unwrap();
        export_of(&leaves)
    };
    let refused = |name: &str, export: String, args: &[&str], want: &str| {
        let (code, out) = verify(&export, args);
        assert_eq!(code, 1, "{name} was accepted: {out}");
        assert!(out.contains(want), "{name}: wanted {want:?} in {out}");
    };

    // A device's action with no request at all.
    let mut a = push.clone();
    a["action"] = json!("delete");
    a["subject"]["file_path"] = json!("topics/secret.md");
    a["subject"]["deleted"] = json!(true);
    a["request"] = Value::Null;
    refused(
        "no request",
        appended(vec![a]),
        &[&saved],
        "a device's request is not an object",
    );

    // A genuine signed request copied onto another action and subject.
    let mut b = push.clone();
    b["action"] = json!("delete");
    b["subject"]["file_path"] = json!("topics/secret.md");
    b["subject"]["deleted"] = json!(true);
    refused(
        "replayed onto another subject",
        appended(vec![b]),
        &[&saved],
        "a replay",
    );

    // The same genuine request, again and again.
    refused(
        "the same request ten times",
        appended(vec![push.clone(); 10]),
        &[&saved],
        "a replay",
    );

    // A body_sha256 that is not the digest the device signed.
    let mut c = push.clone();
    c["request"]["body_sha256"] = json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    refused(
        "a digest the device never signed",
        in_place(c),
        &[],
        "content-digest",
    );

    // A pull's signed request under a push.
    let mut d = push.clone();
    d["request"] = pull["request"].clone();
    refused(
        "a pull's signature on a push",
        in_place(d),
        &[],
        "not a push",
    );

    // A pull whose subject is not the project the device asked for.
    let mut d2 = pull.clone();
    d2["subject"]["project_key"] = json!("someone/else");
    refused(
        "another project's pull",
        in_place(d2),
        &[],
        "does not name the pulled project",
    );

    // The operator approving an existing device id again, under a key the
    // server holds: every later "signature" would then check against it.
    let mut e = approve.clone();
    e["subject"]["user_code"] = json!("BCDF-GHJK");
    refused(
        "a second approve",
        appended(vec![e]),
        &[&saved],
        "approved or enrolled before",
    );

    // A keyid that is not the actor.
    let mut f = push.clone();
    f["actor"]["id"] = json!("dev_someoneelse");
    refused(
        "a keyid that is not the actor",
        in_place(f),
        &[],
        "not the actor",
    );

    // A signature changed by one bit: the Ed25519 check itself.
    let mut g = push.clone();
    let sig = g["request"]["signature"].as_str().unwrap().to_string();
    let flipped = if sig.starts_with('A') { "B" } else { "A" };
    g["request"]["signature"] = json!(format!("{flipped}{}", &sig[1..]));
    refused("a bad signature", in_place(g), &[], "does not verify");

    // A request signed before the device that signed it existed.
    let approve_at = honest
        .iter()
        .position(|l| l.contains("\"action\":\"approve\""))
        .unwrap();
    let push_at = honest
        .iter()
        .position(|l| l.contains("\"action\":\"push\""))
        .unwrap();
    assert!(approve_at < push_at);
    let mut early: Vec<Value> = parsed.clone();
    early.swap(approve_at, push_at);
    let early: Vec<String> = early
        .into_iter()
        .enumerate()
        .map(|(i, mut l)| {
            l["seq"] = json!(i);
            serde_json::to_string(&l).unwrap()
        })
        .collect();
    refused(
        "signed before approval",
        export_of(&early),
        &[],
        "no earlier approve or enroll leaf",
    );

    // A seq that is true rather than 1, or 3.0 rather than 3: the same
    // number to a loose reader.
    for (from, to) in [
        ("\"seq\":1,", "\"seq\":true,"),
        ("\"seq\":3,", "\"seq\":3.0,"),
    ] {
        let mut loose = honest.clone();
        let i = loose.iter().position(|l| l.contains(from)).unwrap();
        loose[i] = loose[i].replacen(from, to, 1);
        refused(to, export_of(&loose), &[], "seq is");
    }

    // A seq that is off by one, correctly hashed: the tree cannot tell.
    let mut renumbered = honest.clone();
    renumbered[2] = renumbered[2].replacen("\"seq\":2,", "\"seq\":7,", 1);
    refused(
        "a wrong seq",
        export_of(&renumbered),
        &[],
        "seq is 7 at position 2",
    );

    // A key given twice, which readers resolve differently.
    let mut dup = honest.clone();
    let i = dup
        .iter()
        .position(|l| l.contains("\"action\":\"pull\""))
        .unwrap();
    dup[i] = dup[i].replacen(
        "\"action\":\"pull\"",
        "\"action\":\"pull\",\"action\":\"push\"",
        1,
    );
    refused("a key given twice", export_of(&dup), &[], "appears twice");

    // An operator's leaf dressed with a request, and an authkey's.
    let mut h1 = approve.clone();
    h1["request"] = push["request"].clone();
    refused(
        "an operator with a request",
        appended(vec![h1]),
        &[],
        "signs nothing",
    );

    // A checkpoint line claiming more leaves than follow it.
    let export = export_of(&honest);
    let (first, rest) = export.split_once('\n').unwrap();
    let (size, root) = first.split_once(' ').unwrap();
    let more = format!("{} {root}\n{rest}", size.parse::<u64>().unwrap() + 1);
    refused(
        "a size that is not the count",
        more,
        &[],
        "checkpoint says tree_size",
    );

    // A request that is not an object at all.
    let mut j = push.clone();
    j["request"] = json!("x");
    refused(
        "a string for a request",
        appended(vec![j]),
        &[],
        "is not an object",
    );

    // `at` going back.
    let mut k = parsed.last().unwrap().clone();
    k["at"] = json!("2000-01-01T00:00:00.000Z");
    refused(
        "at going back",
        in_place(k),
        &[],
        "earlier than the leaf before it",
    );

    // A body that asked for something else than the leaf says was done.
    let mut m = find("approve", "device");
    m["subject"]["scope"] = json!("admin");
    refused(
        "an approve for another scope",
        in_place(m),
        &[],
        "another scope",
    );

    // A revoke of another device than the one the signed path names.
    let mut r = find("revoke", "device");
    r["subject"]["device_id"] = approve["subject"]["device_id"].clone();
    refused(
        "a revoke of another device",
        in_place(r),
        &[],
        "not a revoke",
    );

    // The honest log still verifies, so every refusal above is the forgery.
    assert_eq!(verify(&export_of(&honest), &[&saved]).0, 0);
}

/// The verifier never skips signatures unless told to: asked for a
/// `cryptography` it cannot load, it stops with exit 2; left to choose, it
/// uses its own Ed25519; only `--no-signatures` skips them, and says so.
#[tokio::test]
async fn the_verifier_never_skips_signatures_quietly() {
    let (_h, honest) = a_full_log().await;
    let mut forged = honest.clone();
    let at = forged
        .iter()
        .position(|l| l.contains("\"action\":\"push\""))
        .unwrap();
    let mut leaf: Value = serde_json::from_str(&forged[at]).unwrap();
    let sig = leaf["request"]["signature"].as_str().unwrap().to_string();
    let flipped = if sig.starts_with('A') { "B" } else { "A" };
    leaf["request"]["signature"] = json!(format!("{flipped}{}", &sig[1..]));
    forged[at] = serde_json::to_string(&leaf).unwrap();
    let bad = export_of(&forged);

    // A `cryptography` that cannot be imported, as on a machine without it.
    let fake = tempfile::tempdir().unwrap();
    std::fs::create_dir(fake.path().join("cryptography")).unwrap();
    std::fs::write(
        fake.path().join("cryptography/__init__.py"),
        "raise ImportError('not installed')\n",
    )
    .unwrap();
    let env = [("PYTHONPATH", fake.path().to_str().unwrap())];

    let (code, out) = verify_with(&bad, &["--ed25519=cryptography"], &env);
    assert_eq!(code, 2, "asked for, missing: {out}");
    let (code, out) = verify_with(&bad, &["--ed25519=auto"], &env);
    assert_eq!(code, 1, "left to choose, it still checks: {out}");
    assert!(out.contains("using the built-in Ed25519"), "{out}");
    let (code, out) = verify_with(&export_of(&honest), &["--ed25519=auto"], &env);
    assert_eq!(code, 0, "{out}");

    let (code, out) = verify(&bad, &["--no-signatures"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("NOT checked (--no-signatures)"), "{out}");

    let (code, out) = verify("", &["--self-test"]);
    assert_eq!(code, 0, "RFC 8032's vectors: {out}");
}

// ---------------------------------------------------------------------------
// the merge queue
// ---------------------------------------------------------------------------

/// A log with every job action in it, each the way it happens: a stale push
/// queued for a worker; the worker's claim, its lease running out, and the
/// server's `job_result` saying so; the worker's second claim and its merge;
/// another push queued, the worker revoked, and the server failing that job
/// with nothing left to merge it; an admin device's retry. Answers the job
/// ids beside the leaves.
async fn a_queue_log() -> (Harness, Vec<String>, String, String) {
    let h = harness_with(|cfg| cfg.merge_enabled = true);
    // Checked, and not logged in: nothing is merged inline, and a queue left
    // with no worker is failed rather than merged here.
    h.server.set_claude_status(Status {
        checked_at: "2026-10-02T09:13:40.002Z".into(),
        available: true,
        logged_in: false,
        error: String::new(),
    });
    let mut worker = Machine::new(21);
    h.enrol(&mut worker, "worker", "worker").await;
    let mut laptop = Machine::new(22);
    h.enrol(&mut laptop, "laptop", "admin").await;
    let older = recall_wire::content_sha256("an older version");
    let push = |content: &str| {
        json!({"project_key": "acme/app", "file_path": "topics/auth.md", "content": content,
               "source_env": "laptop", "base_sha256": older})
    };
    assert_eq!(
        h.call("POST", "/sync", Some(TOKEN), Some(push("A")))
            .await
            .0,
        StatusCode::OK
    );
    let queued: Value = ok(h.signed(&laptop, "POST", "/sync", Some(push("B"))).await);
    let first = queued["merge_job"].as_str().unwrap().to_string();

    let claim = json!({"kinds": ["merge"], "wait_seconds": 0, "lease_seconds": 60});
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim.clone()))
        .await);
    assert_eq!(claimed["job"]["id"], json!(first));
    // Its lease runs out, as if a minute had passed.
    let conn = rusqlite::Connection::open(h.dir.path().join("recall.db")).unwrap();
    conn.execute(
        "UPDATE jobs SET lease_expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        [&first],
    )
    .unwrap();
    // The next claim finds it run out, and puts the job back to wait out
    // its retry delay, which the next update waits out too.
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim.clone()))
        .await);
    assert_eq!(claimed["job"], Value::Null);
    conn.execute(
        "UPDATE jobs SET not_before = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
        [&first],
    )
    .unwrap();
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim))
        .await);
    assert_eq!(claimed["job"]["attempt"], json!(2));
    let lease = claimed["job"]["lease_id"].as_str().unwrap();
    let settled: Value = ok(h
        .signed(
            &worker,
            "POST",
            &wire_jobs::result_path(&first),
            Some(json!({"lease_id": lease, "merge": {"content": "A and B"}})),
        )
        .await);
    assert_eq!(settled["state"], json!("done"));

    let queued: Value = ok(h.signed(&laptop, "POST", "/sync", Some(push("C"))).await);
    let second = queued["merge_job"].as_str().unwrap().to_string();
    assert_eq!(
        h.signed(&laptop, "POST", &revoke_device_path(&worker.id), None)
            .await
            .0,
        StatusCode::OK
    );
    // The revocation starts a drain in the background.
    for _ in 0..200 {
        if !h.store.jobs(Some("failed"), 10).unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.signed(&laptop, "POST", &wire_jobs::retry_path(&second), None)
            .await
            .0,
        StatusCode::OK
    );
    let leaves = h.leaf_strings().await;
    (h, leaves, first, second)
}

/// Every job action appends its leaf: who acted, which job, what it came
/// to; and a push queued for the worker names its job.
#[tokio::test]
async fn every_job_action_appends_its_leaf() {
    let (_h, leaves, first, second) = a_queue_log().await;
    let parsed: Vec<Value> = leaves
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let jobs: Vec<String> = parsed
        .iter()
        .filter(|l| {
            l["action"].as_str().unwrap().starts_with("job_")
                || l["subject"]["merge_job"].is_string()
        })
        .map(|l| {
            let job = l["subject"]["job_id"]
                .as_str()
                .or(l["subject"]["merge_job"].as_str())
                .unwrap();
            let job = if job == first {
                "first"
            } else if job == second {
                "second"
            } else {
                job
            };
            format!(
                "{} {} {job}",
                l["action"].as_str().unwrap(),
                l["actor"]["kind"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        jobs,
        [
            "push device first",
            "job_claim device first",
            "job_result server first",
            "job_claim device first",
            "job_result device first",
            "push device second",
            "job_result server second",
            "job_retry device second",
        ],
        "{parsed:#?}"
    );
    let of = |action: &str, kind: &str, job: &str| -> &Value {
        parsed
            .iter()
            .find(|l| {
                l["action"] == action && l["actor"]["kind"] == kind && l["subject"]["job_id"] == job
            })
            .unwrap()
    };
    let claim = of("job_claim", "device", &first);
    assert_eq!(claim["subject"]["attempt"], json!(1));
    assert_eq!(claim["subject"]["file_path"], json!("topics/auth.md"));
    assert!(claim["request"]["signature_base"]
        .as_str()
        .unwrap()
        .contains("/v1/jobs/claim"));
    assert!(
        !leaves.iter().any(|l| l.contains("lse_")),
        "a lease id is in the log"
    );
    let expired = of("job_result", "server", &first);
    assert_eq!(expired["subject"]["state"], json!("queued"));
    assert_eq!(expired["request"], Value::Null);
    let merged = of("job_result", "device", &first);
    assert_eq!(merged["subject"]["state"], json!("done"));
    assert_eq!(
        merged["subject"]["stored_sha256"],
        json!(recall_wire::content_sha256("A and B"))
    );
    assert_eq!(
        merged["request"]["body"],
        Value::Null,
        "the merged file is not kept"
    );
    let failed = of("job_result", "server", &second);
    assert_eq!(failed["subject"]["state"], json!("failed"));
    assert_eq!(failed["subject"]["stored_sha256"], Value::Null);
}

/// A log with the queue in it verifies offline, and each way a server could
/// misstate a job is refused.
#[tokio::test]
async fn a_queue_log_verifies_offline_and_forged_job_leaves_are_refused() {
    let (h, honest, first, _) = a_queue_log().await;
    let (code, out) = verify(&h.export().await, &[]);
    assert_eq!(code, 0, "{out}");

    let parsed: Vec<Value> = honest
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let position = |action: &str, kind: &str, n: usize| -> usize {
        parsed
            .iter()
            .enumerate()
            .filter(|(_, l)| l["action"] == action && l["actor"]["kind"] == kind)
            .nth(n)
            .unwrap_or_else(|| panic!("no {action} by a {kind}"))
            .0
    };
    // `honest` with the leaf at `at` changed by `edit`, or, given no edit,
    // removed and the rest renumbered; the root recomputed.
    let forged = |at: usize, edit: Option<&dyn Fn(&mut Value)>| -> String {
        let mut leaves: Vec<Value> = parsed.clone();
        match edit {
            Some(edit) => edit(&mut leaves[at]),
            None => {
                leaves.remove(at);
            }
        }
        let leaves: Vec<String> = leaves
            .into_iter()
            .enumerate()
            .map(|(seq, mut l)| {
                l["seq"] = json!(seq);
                serde_json::to_string(&l).unwrap()
            })
            .collect();
        export_of(&leaves)
    };
    let refused = |name: &str, export: String, want: &str| {
        let (code, out) = verify(&export, &[]);
        assert_eq!(code, 1, "{name} was accepted: {out}");
        assert!(out.contains(want), "{name}: wanted {want:?} in {out}");
    };

    refused(
        "the lease that ran out, left out",
        forged(position("job_result", "server", 0), None),
        &format!("{first} was claimed while dev_"),
    );
    refused(
        "a worker's claim that was never leased to it, left out",
        forged(position("job_claim", "device", 1), None),
        "which does not hold it",
    );
    refused(
        "the worker approved as a sync device",
        forged(
            position("approve", "operator", 0),
            Some(&|l: &mut Value| l["subject"]["scope"] = json!("sync")),
        ),
        "whose scope is sync",
    );
    refused(
        "an attempt miscounted",
        forged(
            position("job_claim", "device", 1),
            Some(&|l: &mut Value| l["subject"]["attempt"] = json!(1)),
        ),
        "at attempt 1, not 2",
    );
    refused(
        "a worker's result moved onto another job",
        forged(
            position("job_result", "device", 0),
            Some(&|l: &mut Value| l["subject"]["job_id"] = json!("job_other")),
        ),
        "not a job_result",
    );
    refused(
        "a retry of a job that had not failed",
        forged(
            position("job_result", "server", 1),
            Some(&|l: &mut Value| l["subject"]["state"] = json!("queued")),
        ),
        "was retried while queued",
    );
    refused(
        "a merged push that queued a job too",
        forged(
            position("push", "device", 0),
            Some(&|l: &mut Value| l["subject"]["merged"] = json!(true)),
        ),
        "a merged push queued a merge job",
    );
    refused(
        "a job no push queued",
        forged(
            position("push", "device", 0),
            Some(&|l: &mut Value| l["subject"]["merge_job"] = Value::Null),
        ),
        "which no push or result queued",
    );
}

// ---------------------------------------------------------------------------
// evaluations
// ---------------------------------------------------------------------------

/// A log with evaluations in it: one an admin device asks for and a worker
/// makes, one on the schedule, whose attempt fails, and one the operator
/// asks for with the contradiction check. Answers the admin device's run's
/// job.
async fn an_evaluation_log() -> (Harness, Vec<String>, String) {
    let h = harness();
    let mut worker = Machine::new(31);
    h.enrol(&mut worker, "worker", "worker").await;
    let mut laptop = Machine::new(32);
    h.enrol(&mut laptop, "laptop", "admin").await;
    let push = json!({"project_key": "acme/app", "file_path": "MEMORY.md",
                      "content": "- [Gone](gone.md)\n", "source_env": "laptop"});
    let _: Value = ok(h.signed(&laptop, "POST", "/sync", Some(push)).await);

    let asked: Value = ok(h
        .signed(
            &laptop,
            "POST",
            recall_wire::evaluations::EVALUATIONS_PATH,
            Some(json!({"projects": ["acme/app", "acme/app"]})),
        )
        .await);
    let job = asked["job"].as_str().unwrap().to_string();
    let claim = json!({"kinds": ["evaluate"], "wait_seconds": 0, "lease_seconds": 60});
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim.clone()))
        .await);
    assert_eq!(claimed["job"]["id"], json!(job));
    let lease = claimed["job"]["lease_id"].as_str().unwrap();
    let report = json!({"lease_id": lease, "evaluate": {
        "findings": [{"id": "f1", "kind": "dead_link", "severity": "medium",
                      "project_key": "acme/app", "file_path": "MEMORY.md",
                      "lines": [1, 1], "related": []}],
        "details": {"findings": {"f1": {"excerpt": "- [Gone](gone.md)\n"}}}}});
    let _: Value = ok(h
        .signed(&worker, "POST", &wire_jobs::result_path(&job), Some(report))
        .await);

    // On the schedule: its first attempt fails, and its second, once the
    // retry delay is past, makes the report.
    assert!(h.server.run_scheduled_evaluation().unwrap().is_some());
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim.clone()))
        .await);
    let second = claimed["job"]["id"].as_str().unwrap().to_string();
    let lease = claimed["job"]["lease_id"].as_str().unwrap();
    let _: Value = ok(h
        .signed(
            &worker,
            "POST",
            &wire_jobs::result_path(&second),
            Some(json!({"lease_id": lease, "error": "claude timed out"})),
        )
        .await);
    rusqlite::Connection::open(h.dir.path().join("recall.db"))
        .unwrap()
        .execute(
            "UPDATE jobs SET not_before = '2000-01-01T00:00:00.000Z' WHERE id = ?1",
            [&second],
        )
        .unwrap();
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim))
        .await);
    assert_eq!(claimed["job"]["attempt"], json!(2));
    let lease = claimed["job"]["lease_id"].as_str().unwrap();
    let _: Value = ok(h
        .signed(
            &worker,
            "POST",
            &wire_jobs::result_path(&second),
            Some(json!({"lease_id": lease, "evaluate": {"findings": [], "details": {}}})),
        )
        .await);

    let _: Value = ok(h
        .call(
            "POST",
            recall_wire::evaluations::EVALUATIONS_PATH,
            Some(TOKEN),
            Some(json!({"contradictions": true})),
        )
        .await);
    let leaves = h.leaf_strings().await;
    (h, leaves, job)
}

/// A kept body the server reads as no body at all must be one both
/// verifiers read the same way: only spaces, tabs, carriage returns and
/// line feeds. A no-break space, a form feed, a vertical tab or a line
/// separator is refused as not JSON, never kept in a leaf the verifiers
/// would then refuse forever.
#[tokio::test]
async fn a_blank_body_is_blank_to_the_verifiers_too() {
    let h = harness();
    let mut worker = Machine::new(33);
    h.enrol(&mut worker, "worker", "worker").await;
    let mut laptop = Machine::new(34);
    h.enrol(&mut laptop, "laptop", "admin").await;
    let key = h.authkey(true).await;
    for body in ["\u{a0}", "\x0c", "\x0b", "\u{2028}", " \x0c\n"] {
        for path in [
            recall_wire::evaluations::EVALUATIONS_PATH.to_string(),
            revoke_authkey_path(&key.id),
        ] {
            let (status, _, _) = h
                .signed_raw(&laptop, "POST", &path, body.as_bytes().to_vec())
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path} took {body:?}");
        }
    }
    // What both read as blank still is.
    let (status, _, _) = h
        .signed_raw(
            &laptop,
            "POST",
            recall_wire::evaluations::EVALUATIONS_PATH,
            b" \t\r\n".to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = h
        .signed_raw(
            &laptop,
            "POST",
            &revoke_authkey_path(&key.id),
            b"\r\n".to_vec(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (code, out) = verify(&h.export().await, &[]);
    assert_eq!(code, 0, "{out}");
}

/// Asking for an evaluation appends one `evaluate` leaf, which names the
/// run, its job and what was asked, and keeps an admin device's body; the
/// job's claim and result are the leaves any job's are, and none of them
/// holds what the report found.
#[tokio::test]
async fn an_evaluation_appends_its_leaves_and_no_finding() {
    let (_h, leaves, job) = an_evaluation_log().await;
    let parsed: Vec<Value> = leaves
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let actions: Vec<String> = parsed
        .iter()
        .filter(|l| l["action"] == "evaluate" || l["subject"]["job_id"] == json!(job))
        .map(|l| {
            format!(
                "{} {}",
                l["action"].as_str().unwrap(),
                l["actor"]["kind"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        actions,
        [
            "evaluate device",
            "job_claim device",
            "job_result device",
            "evaluate server",
            "evaluate operator",
        ]
    );
    let asked = parsed.iter().find(|l| l["action"] == "evaluate").unwrap();
    assert_eq!(asked["subject"]["projects"], json!(["acme/app"]));
    assert_eq!(asked["subject"]["job_id"], json!(job));
    assert_eq!(
        asked["request"]["body"],
        json!(r#"{"projects":["acme/app","acme/app"]}"#)
    );
    let result = parsed
        .iter()
        .find(|l| l["action"] == "job_result" && l["subject"]["job_id"] == json!(job))
        .unwrap();
    assert_eq!(result["subject"]["state"], json!("done"));
    assert_eq!(result["subject"]["stored_sha256"], Value::Null);
    assert!(
        !leaves
            .iter()
            .any(|l| l.contains("dead_link") || l.contains("Gone")),
        "a finding is in the log"
    );
}

/// A log with evaluations verifies offline, and each way a server could
/// misstate one is refused.
#[tokio::test]
async fn an_evaluation_log_verifies_offline_and_forgeries_are_refused() {
    let (h, honest, job) = an_evaluation_log().await;
    let (code, out) = verify(&h.export().await, &[]);
    assert_eq!(code, 0, "{out}");

    let parsed: Vec<Value> = honest
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let first = parsed
        .iter()
        .position(|l| l["action"] == "evaluate")
        .unwrap();
    let forged = |at: usize, edit: Option<&dyn Fn(&mut Value)>| -> String {
        let mut leaves: Vec<Value> = parsed.clone();
        match edit {
            Some(edit) => edit(&mut leaves[at]),
            None => {
                leaves.remove(at);
            }
        }
        let leaves: Vec<String> = leaves
            .into_iter()
            .enumerate()
            .map(|(seq, mut l)| {
                l["seq"] = json!(seq);
                serde_json::to_string(&l).unwrap()
            })
            .collect();
        export_of(&leaves)
    };
    let refused = |name: &str, export: String, want: &str| {
        let (code, out) = verify(&export, &[]);
        assert_eq!(code, 1, "{name} was accepted: {out}");
        assert!(out.contains(want), "{name}: wanted {want:?} in {out}");
    };
    refused(
        "a job no evaluation queued",
        forged(first, None),
        &format!("{job}, which no push or result queued"),
    );
    refused(
        "other projects than the body asked for",
        forged(
            first,
            Some(&|l: &mut Value| l["subject"]["projects"] = json!(["acme/web"])),
        ),
        "the body asked for other projects",
    );
    refused(
        "the contradiction check, which the body did not ask for",
        forged(
            first,
            Some(&|l: &mut Value| l["subject"]["contradictions"] = json!(true)),
        ),
        "the body asked for another contradictions",
    );
    refused(
        "a run an authkey asked for",
        forged(
            first,
            Some(&|l: &mut Value| {
                l["actor"] = json!({"kind": "authkey", "id": "ak_x", "tag": "cloud"});
                l["request"] = Value::Null;
            }),
        ),
        "a authkey cannot evaluate",
    );
    refused(
        "the laptop approved as a sync device",
        forged(
            parsed
                .iter()
                .position(|l| l["action"] == "approve" && l["subject"]["name"] == "laptop")
                .unwrap(),
            Some(&|l: &mut Value| l["subject"]["scope"] = json!("sync")),
        ),
        "does not have the admin scope",
    );
}

// ---------------------------------------------------------------------------
// the host's admin commands
// ---------------------------------------------------------------------------

/// Runs `recall-server admin` on the harness's database file, the way the
/// owner runs it on the host beside the running server. Answers what it
/// printed; it must succeed.
fn admin(h: &Harness, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_recall-server"))
        .arg("admin")
        .args(args)
        .env_clear()
        .env("RECALL_DB_PATH", h.dir.path().join("recall.db"))
        .env("RECALL_BACKUP_DIR", h.dir.path().join("backups"))
        .env("RECALL_MERGE_TIMEOUT_MS", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("recall-server runs");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{args:?}: {said}");
    said
}

/// A log with the host's rename, restore and remove in it, each made by
/// another process while the server runs and appends before and after: a
/// stale push queued for the worker, which leases it; the rename, which
/// closes that job, and the worker's result for it, which is answered as
/// recorded and appends nothing; a push under the new key, queued too; a
/// restore of the old key from the backup the rename took; a remove of the
/// new key, which closes the second job; and a pull. Answers the leaves,
/// the size of the log before the rename, and the two jobs.
async fn an_admin_log() -> (Harness, Vec<String>, u64, String, String) {
    let h = harness_with(|cfg| cfg.merge_enabled = true);
    h.server.set_claude_status(Status {
        checked_at: "2026-10-02T09:13:40.002Z".into(),
        available: true,
        logged_in: false,
        error: String::new(),
    });
    let mut worker = Machine::new(31);
    h.enrol(&mut worker, "worker", "worker").await;
    let mut laptop = Machine::new(32);
    h.enrol(&mut laptop, "laptop", "admin").await;
    let older = recall_wire::content_sha256("an older version");
    let push = |key: &str, content: &str| {
        json!({"project_key": key, "file_path": "topics/auth.md", "content": content,
               "source_env": "laptop", "base_sha256": older})
    };
    assert_eq!(
        h.call("POST", "/sync", Some(TOKEN), Some(push("acme/app", "A")))
            .await
            .0,
        StatusCode::OK
    );
    let queued: Value = ok(h
        .signed(&laptop, "POST", "/sync", Some(push("acme/app", "B")))
        .await);
    let first = queued["merge_job"].as_str().unwrap().to_string();
    let claim = json!({"kinds": ["merge"], "wait_seconds": 0, "lease_seconds": 600});
    let claimed: Value = ok(h
        .signed(&worker, "POST", wire_jobs::CLAIM_PATH, Some(claim))
        .await);
    assert_eq!(claimed["job"]["id"], json!(first));
    let lease = claimed["job"]["lease_id"].as_str().unwrap().to_string();

    let before = h.size();
    let said = admin(&h, &["rename", "acme/app", "acme/renamed", "--yes"]);
    assert!(
        said.contains(&format!("leaf {before} (admin_rename)")),
        "{said}"
    );
    assert_eq!(h.size(), before, "the server has not looked yet");

    // The worker's result lands on a job the rename closed.
    let settled: Value = ok(h
        .signed(
            &worker,
            "POST",
            &wire_jobs::result_path(&first),
            Some(json!({"lease_id": lease, "merge": {"content": "A and B"}})),
        )
        .await);
    assert_eq!(settled["state"], json!("done"));
    assert_eq!(settled["applied"], json!(false));
    assert_eq!(
        h.size(),
        before,
        "a result already recorded appends nothing"
    );

    let queued: Value = ok(h
        .signed(&laptop, "POST", "/sync", Some(push("acme/renamed", "C")))
        .await);
    let second = queued["merge_job"].as_str().unwrap().to_string();
    assert_eq!(h.size(), before + 2, "the rename's leaf, then the push's");

    let backup = std::fs::read_dir(h.dir.path().join("backups/admin"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    admin(
        &h,
        &["restore", backup.to_str().unwrap(), "acme/app", "--yes"],
    );
    admin(&h, &["remove", "acme/renamed", "--yes"]);
    let (status, _) = h
        .call("GET", "/sync?project_key=acme/app", Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let leaves = h.leaf_strings().await;
    (h, leaves, before, first, second)
}

/// The host's changes are leaves in the one log the server keeps: each
/// takes the next seq, and the server's own next leaf follows it rather
/// than forking the tree, so the server's checkpoint and the file's agree.
#[tokio::test]
async fn the_hosts_changes_take_their_place_in_the_servers_log() {
    let (h, leaves, before, first, second) = an_admin_log().await;
    let parsed: Vec<Value> = leaves
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let after: Vec<String> = parsed[before as usize..]
        .iter()
        .map(|l| {
            format!(
                "{} {}",
                l["action"].as_str().unwrap(),
                l["actor"]["kind"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        after,
        [
            "admin_rename host",
            "push device",
            "admin_restore host",
            "admin_remove host",
            "pull operator"
        ]
    );
    for (i, l) in parsed.iter().enumerate() {
        assert_eq!(l["seq"], json!(i));
    }
    let rename = &parsed[before as usize]["subject"];
    assert_eq!(rename["from"], "acme/app");
    assert_eq!(rename["to"], "acme/renamed");
    assert_eq!(rename["rows"], 1);
    assert_eq!(rename["jobs_closed"], json!([first]));
    let remove = &parsed[before as usize + 3]["subject"];
    assert_eq!(remove["jobs_closed"], json!([second]));
    let restore = &parsed[before as usize + 2]["subject"];
    assert_eq!(restore["added"], 1);
    assert_eq!(restore["source"], rename["backup"]);

    assert_eq!(
        h.store.audit_checkpoint(),
        Store::open(h.dir.path().join("recall.db"))
            .unwrap()
            .audit_checkpoint(),
        "one tree"
    );
}

/// A log with the host's changes in it verifies offline through the real
/// script, and each way of misstating one is refused.
#[tokio::test]
async fn a_log_with_the_hosts_changes_verifies_offline_and_forgeries_are_refused() {
    let (h, honest, before, first, _) = an_admin_log().await;
    let (code, out) = verify(&h.export().await, &[]);
    assert_eq!(code, 0, "{out}");

    let parsed: Vec<Value> = honest
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let (rename, restore, remove) = (before as usize, before as usize + 2, before as usize + 3);
    let forged = |at: usize, edit: &dyn Fn(&mut Value)| -> String {
        let mut leaves = parsed.clone();
        edit(&mut leaves[at]);
        let leaves: Vec<String> = leaves
            .iter()
            .map(|l| serde_json::to_string(l).unwrap())
            .collect();
        export_of(&leaves)
    };
    let refused = |name: &str, export: String, want: &str| {
        let (code, out) = verify(&export, &[]);
        assert_eq!(code, 1, "{name} was accepted: {out}");
        assert!(out.contains(want), "{name}: wanted {want:?} in {out}");
    };

    refused(
        "a rename that left its key's job open",
        forged(rename, &|l| l["subject"]["jobs_closed"] = json!([])),
        &format!("left {first} open"),
    );
    refused(
        "a rename closing a job no push queued",
        forged(rename, &|l| {
            l["subject"]["jobs_closed"] = json!([first.clone(), "job_never"])
        }),
        "closed job_never, which no push or result queued",
    );
    refused(
        "a rename of another key closing this key's job",
        forged(rename, &|l| l["subject"]["from"] = json!("other/key")),
        "a job of 'acme/app'",
    );
    refused(
        "a remove closing a job already closed",
        forged(remove, &|l| {
            l["subject"]["jobs_closed"]
                .as_array_mut()
                .unwrap()
                .push(json!(first.clone()))
        }),
        &format!("closed {first} once it was done"),
    );
    refused(
        "a rename credited to the server",
        forged(rename, &|l| l["actor"] = json!({"kind": "server"})),
        "a server cannot admin_rename",
    );
    refused(
        "a restore carrying a request",
        forged(restore, &|l| {
            l["request"] = json!({"body_sha256": "", "signature_base": "", "signature": "",
                                  "body": null})
        }),
        "the host signs nothing",
    );
    refused(
        "a rename that moved nothing",
        forged(rename, &|l| l["subject"]["rows"] = json!(0)),
        "changed no row",
    );
    refused(
        "a restore that changed nothing",
        forged(restore, &|l| l["subject"]["added"] = json!(0)),
        "changed no row",
    );
    refused(
        "a negative count",
        forged(remove, &|l| l["subject"]["rows"] = json!(-1)),
        "subject.rows is not a count",
    );
    refused(
        "a backup named by its path",
        forged(remove, &|l| {
            l["subject"]["backup"] = json!("/backups/admin/recall-x.db")
        }),
        "subject.backup is not a file name",
    );
    refused(
        "a rename onto its own key",
        forged(rename, &|l| l["subject"]["to"] = json!("acme/app")),
        "a rename onto the key it renames",
    );
    refused(
        "a remove without its backup",
        forged(remove, &|l| {
            l["subject"].as_object_mut().unwrap().remove("backup");
        }),
        "the subject has the keys",
    );
    refused(
        "a job closed twice in one leaf",
        forged(rename, &|l| {
            l["subject"]["jobs_closed"] = json!([first.clone(), first.clone()])
        }),
        "names a job twice",
    );
}
