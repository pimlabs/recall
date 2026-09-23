//! Devices, end to end through the router: enrolling by code and by
//! enrolment key, signed requests and every way one is refused, the admin
//! scope, the ephemeral sweep, and the legacy bearer token still working
//! everywhere beside all of it.
//!
//! Requests are signed with `recall_wire::signature`, the implementation
//! the client will sign with, so these tests are the two halves meeting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use recall_server::{Config, Server, Store};
use recall_wire::devices::{self, ENROLL_KEY_PREFIX};
use recall_wire::signature::{self, encode_public_key, fingerprint, SigningKey, Target};
use recall_wire::{
    Device, DeviceIdentity, DeviceList, EnrollApproved, EnrollKey, EnrollKeyCreated, EnrollKeyList,
    EnrollPending, EnrollPollResponse, ErrorResponse, PendingEnrollment, PushResponse,
    SyncResponse,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

const TOKEN: &str = "device-test-token";
const HOST: &str = "recall.test";

struct Harness {
    server: Server,
    dir: TempDir,
    store: Arc<Store>,
}

fn harness(tweak: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let mut cfg = Config {
        token: TOKEN.to_string(),
        merge_enabled: false,
        rate_limit_max: 10_000,
        ..Config::default()
    };
    tweak(&mut cfg);
    let server = Server::new(cfg, store.clone());
    // As if it had been up ten minutes: a server refuses every signature
    // dated in the first few seconds after it started, which only the
    // restart tests are about.
    server.backdate_start(600);
    Harness { server, dir, store }
}

/// A new process on `h`'s database, just started, as a deploy leaves it.
fn restarted(h: &Harness) -> Harness {
    Harness {
        server: Server::new(
            Config {
                token: TOKEN.to_string(),
                merge_enabled: false,
                rate_limit_max: 10_000,
                ..Config::default()
            },
            h.store.clone(),
        ),
        dir: tempfile::tempdir().unwrap(),
        store: h.store.clone(),
    }
}

/// What a server that has just started says to a signature the process
/// before it could have accepted.
const TOO_SOON: &str = "unauthorized: signature created before this server started, or too soon \
                        after; sign the request again in a few seconds";

/// A machine: a key pair, the name it enrols as, and the device id once it
/// has one.
struct Machine {
    key: SigningKey,
    name: String,
    id: String,
}

impl Machine {
    fn new(seed: u8) -> Self {
        Self::named(seed, &format!("machine-{seed}"))
    }

    fn named(seed: u8, name: &str) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
            name: name.to_string(),
            id: String::new(),
        }
    }

    fn public_key(&self) -> String {
        encode_public_key(&self.key.verifying_key())
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn fresh_nonce() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!("nonce-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// How a test wants a signed request built, so each refusal case changes
/// one thing.
struct Signing<'a> {
    machine: &'a Machine,
    keyid: Option<&'a str>,
    created: i64,
    nonce: String,
    /// Sent instead of the body that was signed.
    tamper_body: Option<Vec<u8>>,
}

impl<'a> Signing<'a> {
    fn by(machine: &'a Machine) -> Self {
        Self {
            machine,
            keyid: None,
            created: unix_now(),
            nonce: fresh_nonce(),
            tamper_body: None,
        }
    }
}

fn signed_request(method: &str, uri: &str, body: Vec<u8>, s: &Signing<'_>) -> Request<Body> {
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
    let headers = signature::sign_request(
        &s.machine.key,
        s.keyid.unwrap_or(&s.machine.id),
        &target,
        "1",
        &body,
        s.created,
        &s.nonce,
    )
    .unwrap();
    Request::builder()
        .method(method)
        .uri(uri)
        .header("host", HOST)
        .header("content-type", "application/json")
        .header(recall_wire::PROTOCOL_HEADER, "1")
        .header(signature::CONTENT_DIGEST_HEADER, headers.content_digest)
        .header(signature::SIGNATURE_INPUT_HEADER, headers.signature_input)
        .header(signature::SIGNATURE_HEADER, headers.signature)
        .body(Body::from(s.tamper_body.clone().unwrap_or(body)))
        .unwrap()
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

    /// A JSON request with the operator's token, or none.
    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, Bytes) {
        self.call_from("", method, uri, token, body).await
    }

    /// [`Harness::call`], from the address `ip` as the rate limiter sees it,
    /// or from the default one when `ip` is empty.
    async fn call_from(
        &self,
        ip: &str,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, Bytes) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if !ip.is_empty() {
            req = req.header("cf-connecting-ip", ip);
        }
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = body.map(|b| Body::from(b.to_string())).unwrap_or_default();
        let (status, _, bytes) = self.send(req.body(body).unwrap()).await;
        (status, bytes)
    }

    async fn signed(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        s: &Signing<'_>,
    ) -> (StatusCode, Bytes) {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let (status, _, bytes) = self.send(signed_request(method, uri, body, s)).await;
        (status, bytes)
    }

    /// Enrols `machine` by code and approves it with the operator's token.
    async fn enrol(&self, machine: &mut Machine, scope: &str) -> Device {
        let pending: EnrollPending = ok(self
            .call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": machine.name, "public_key": machine.public_key(), "agent": "recall/test"})),
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
        let polled: EnrollPollResponse = ok(self.poll(&pending.enrollment_id).await);
        assert_eq!(polled.device_id, device.id);
        machine.id = device.id.clone();
        device
    }

    async fn poll(&self, enrollment_id: &str) -> (StatusCode, Bytes) {
        self.call(
            "POST",
            devices::ENROLL_POLL_PATH,
            None,
            Some(json!({ "enrollment_id": enrollment_id })),
        )
        .await
    }

    async fn create_enroll_key(&self, ephemeral: bool) -> EnrollKeyCreated {
        ok(self
            .call(
                "POST",
                devices::ENROLL_KEYS_PATH,
                Some(TOKEN),
                Some(json!({"tag": "cloud", "expires_in_days": 90, "ephemeral": ephemeral})),
            )
            .await)
    }

    async fn enrol_with_key(&self, machine: &Machine, key: &str) -> (StatusCode, Bytes) {
        self.call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "cloud-session", "public_key": machine.public_key(), "agent": "recall/test", "enroll_key": key})),
        )
        .await
    }

    /// Runs SQL against the server's database file, for what a test cannot
    /// wait for: a code or key reaching its expiry, a device going idle.
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

fn push_body(content: &str) -> serde_json::Value {
    json!({"project_key": "acme/app", "file_path": "MEMORY.md", "content": content, "source_env": "laptop"})
}

// ---------------------------------------------------------------------------
// enrolling by code
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_machine_enrols_by_code_approved_with_the_operator_token() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(1);

    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "laptop", "public_key": laptop.public_key(), "agent": "recall/0.4.1 (linux-x86_64)"})),
        )
        .await);
    assert!(pending.enrollment_id.starts_with("enr_"));
    assert_eq!(
        devices::normalize_user_code(&pending.user_code).as_deref(),
        Some(pending.user_code.as_str()),
        "the code is shown already normalized"
    );
    assert_eq!((pending.expires_in, pending.interval), (900, 5));

    // Nobody has approved it yet; and asking again at once is too soon.
    assert_eq!(
        error_of(h.poll(&pending.enrollment_id).await),
        (StatusCode::BAD_REQUEST, "authorization_pending".into())
    );
    assert_eq!(
        error_of(h.poll(&pending.enrollment_id).await),
        (StatusCode::BAD_REQUEST, "slow_down".into())
    );

    // The owner types the code the way a person does.
    let typed = pending.user_code.to_lowercase().replace('-', " ");
    let device: Device = ok(h
        .call(
            "POST",
            devices::APPROVE_PATH,
            Some(TOKEN),
            Some(json!({"user_code": typed})),
        )
        .await);
    assert!(
        device.id.starts_with("dev_") && device.id.len() == 30,
        "{}",
        device.id
    );
    assert_eq!(device.name, "laptop");
    assert_eq!(device.scope, "sync", "sync unless asked otherwise");
    assert_eq!(device.agent, "recall/0.4.1 (linux-x86_64)");
    assert_eq!(device.public_key, laptop.public_key());
    assert_eq!(device.fingerprint, fingerprint(&laptop.key.verifying_key()));
    assert!(!device.ephemeral && device.revoked_at.is_none() && device.last_seen.is_none());

    // Approval wins over the pace of polling.
    let polled: EnrollPollResponse = ok(h.poll(&pending.enrollment_id).await);
    assert_eq!(
        (polled.device_id.as_str(), polled.scope.as_str()),
        (device.id.as_str(), "sync")
    );
    laptop.id = device.id.clone();

    // And from now on it signs.
    let s = Signing::by(&laptop);
    let pushed: PushResponse = ok(h
        .signed("POST", "/sync", Some(push_body("# signed\n")), &s)
        .await);
    assert!(pushed.ok);

    // The same code cannot be approved twice.
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": pending.user_code})),
            )
            .await
        ),
        (
            StatusCode::CONFLICT,
            "that code was already approved or denied".into()
        )
    );
}

/// RFC 8628 §5.4: before approving, the approver sees what the code would
/// approve, and can compare the name and fingerprint with the machine's own
/// screen. The lookup judges the code exactly as approving it would.
#[tokio::test]
async fn the_approver_sees_what_it_is_approving_first() {
    let h = harness(|_| {});
    let laptop = Machine::new(19);
    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "laptop", "public_key": laptop.public_key(), "agent": "recall/0.4.1 (linux-x86_64)"})),
        )
        .await);

    // Typed the way a person does: lowercase, no hyphen.
    let typed = pending.user_code.to_lowercase().replace('-', "");
    let seen: PendingEnrollment = ok(h
        .call("GET", &devices::pending_path(&typed), Some(TOKEN), None)
        .await);
    assert_eq!(seen.user_code, pending.user_code, "normalized");
    assert_eq!(seen.name, "laptop");
    assert_eq!(seen.agent, "recall/0.4.1 (linux-x86_64)");
    assert_eq!(seen.fingerprint, fingerprint(&laptop.key.verifying_key()));
    assert!(
        (890..=900).contains(&seen.expires_in),
        "{}",
        seen.expires_in
    );

    // Looking is not deciding: the machine is still waiting.
    assert_eq!(
        error_of(h.poll(&pending.enrollment_id).await).1,
        "authorization_pending"
    );

    // Admin only, like approving.
    assert_eq!(
        error_of(
            h.call("GET", &devices::pending_path(&typed), None, None)
                .await
        ),
        (StatusCode::UNAUTHORIZED, "unauthorized".into())
    );
    let mut phone = Machine::new(20);
    h.enrol(&mut phone, "sync").await;
    let s = Signing::by(&phone);
    assert_eq!(
        h.signed("GET", &devices::pending_path(&typed), None, &s)
            .await
            .0,
        StatusCode::FORBIDDEN
    );

    for (code, want) in [
        ("ZZZZ-ZZZZ", StatusCode::NOT_FOUND),
        ("abc", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            h.call("GET", &devices::pending_path(code), Some(TOKEN), None)
                .await
                .0,
            want,
            "{code}"
        );
    }

    // Once decided, it is no longer pending.
    let _: Device = ok(h
        .call(
            "POST",
            devices::APPROVE_PATH,
            Some(TOKEN),
            Some(json!({"user_code": pending.user_code})),
        )
        .await);
    assert_eq!(
        error_of(
            h.call(
                "GET",
                &devices::pending_path(&pending.user_code),
                Some(TOKEN),
                None
            )
            .await
        ),
        (
            StatusCode::CONFLICT,
            "that code was already approved or denied".into()
        )
    );

    // And once expired, it says so, as approving would.
    let late: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "late", "public_key": laptop.public_key()})),
        )
        .await);
    h.sql(&format!(
        "UPDATE device_enrollments SET expires_at = '2020-01-01T00:00:00.000Z' WHERE user_code = '{}'",
        late.user_code
    ));
    assert_eq!(
        error_of(
            h.call(
                "GET",
                &devices::pending_path(&late.user_code),
                Some(TOKEN),
                None
            )
            .await
        ),
        (
            StatusCode::GONE,
            "that code has expired; start the enrolment again".into()
        )
    );
}

/// The name belongs to the key: whatever a signed push says it came from,
/// it is stored under the name the device enrolled as. A bearer push keeps
/// the label it sent, as it always has.
#[tokio::test]
async fn a_signed_push_is_recorded_under_the_devices_own_name() {
    let h = harness(|_| {});
    let mut laptop = Machine::named(21, "laptop");
    h.enrol(&mut laptop, "sync").await;

    let s = Signing::by(&laptop);
    let mut claim = push_body("# signed\n");
    claim["source_env"] = json!("the-other-machine");
    let _: PushResponse = ok(h.signed("POST", "/sync", Some(claim), &s).await);

    let s = Signing::by(&laptop);
    let delete = json!({"project_key": "acme/app", "file_path": "gone.md", "deleted": true, "source_env": "someone-else"});
    let _: PushResponse = ok(h.signed("POST", "/sync", Some(delete), &s).await);

    let mut bearer = push_body("# bearer\n");
    bearer["file_path"] = json!("bearer.md");
    bearer["source_env"] = json!("cloud");
    assert_eq!(
        h.call("POST", "/sync", Some(TOKEN), Some(bearer)).await.0,
        StatusCode::OK
    );

    let (_, body) = h
        .call("GET", "/sync?project_key=acme%2Fapp", Some(TOKEN), None)
        .await;
    let pulled: SyncResponse = serde_json::from_slice(&body).unwrap();
    let source = |path: &str| {
        pulled
            .files
            .iter()
            .find(|f| f.file_path == path)
            .map(|f| f.source_env.clone())
            .unwrap()
    };
    assert_eq!(source("MEMORY.md"), "laptop", "a signed write");
    assert_eq!(source("gone.md"), "laptop", "a signed delete");
    assert_eq!(source("bearer.md"), "cloud", "a bearer push is unchanged");
}

#[tokio::test]
async fn a_denied_or_revoked_enrolment_polls_as_access_denied() {
    let h = harness(|_| {});
    let enrol = |seed: u8| {
        let h = &h;
        async move {
            let m = Machine::new(seed);
            let p: EnrollPending = ok(h
                .call(
                    "POST",
                    devices::ENROLL_PATH,
                    None,
                    Some(json!({"name": "phone", "public_key": m.public_key()})),
                )
                .await);
            p
        }
    };

    let denied = enrol(2).await;
    let answer: recall_wire::DenyResponse = ok(h
        .call(
            "POST",
            devices::DENY_PATH,
            Some(TOKEN),
            Some(json!({"user_code": denied.user_code})),
        )
        .await);
    assert_eq!(
        (
            answer.name.as_str(),
            answer.denied,
            answer.user_code.as_str()
        ),
        ("phone", true, denied.user_code.as_str())
    );
    assert_eq!(
        error_of(h.poll(&denied.enrollment_id).await),
        (StatusCode::BAD_REQUEST, "access_denied".into())
    );

    // Approved by mistake, then revoked before the machine collected it.
    let revoked = enrol(3).await;
    let device: Device = ok(h
        .call(
            "POST",
            devices::APPROVE_PATH,
            Some(TOKEN),
            Some(json!({"user_code": revoked.user_code})),
        )
        .await);
    let _: Device = ok(h
        .call(
            "POST",
            &devices::revoke_device_path(&device.id),
            Some(TOKEN),
            None,
        )
        .await);
    assert_eq!(
        error_of(h.poll(&revoked.enrollment_id).await),
        (StatusCode::BAD_REQUEST, "access_denied".into())
    );
}

#[tokio::test]
async fn an_expired_or_unknown_enrolment_says_so() {
    let h = harness(|_| {});
    let m = Machine::new(4);
    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "old", "public_key": m.public_key()})),
        )
        .await);
    h.sql("UPDATE device_enrollments SET expires_at = '2020-01-01T00:00:00.000Z'");

    assert_eq!(
        error_of(h.poll(&pending.enrollment_id).await),
        (StatusCode::BAD_REQUEST, "expired_token".into())
    );
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": pending.user_code})),
            )
            .await
        ),
        (
            StatusCode::GONE,
            "that code has expired; start the enrolment again".into()
        )
    );
    assert_eq!(
        error_of(h.poll("enr_neverissued").await),
        (StatusCode::BAD_REQUEST, "invalid_grant".into())
    );
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": "ZZZZ-ZZZZ"})),
            )
            .await
        )
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": "abc"})),
            )
            .await
        )
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_bad_enrolment_request_is_400_and_nothing_is_stored() {
    let h = harness(|_| {});
    let good = Machine::new(5).public_key();
    for (body, want) in [
        (
            json!({"name": "x", "public_key": "not-a-key"}),
            "public_key must be an Ed25519 public key: 32 bytes, base64url without padding",
        ),
        (
            json!({"name": "", "public_key": good}),
            "name and public_key are required",
        ),
        (
            json!({"name": "a\u{7}b", "public_key": good}),
            "name must be at most 64 characters, with no control, format or invisible characters",
        ),
        (json!({"public_key": good}), "invalid json body"),
    ] {
        assert_eq!(
            error_of(
                h.call("POST", devices::ENROLL_PATH, None, Some(body.clone()))
                    .await
            ),
            (StatusCode::BAD_REQUEST, want.to_string()),
            "{body}"
        );
    }
    let list: DeviceList = ok(h
        .call("GET", devices::DEVICES_PATH, Some(TOKEN), None)
        .await);
    assert!(list.devices.is_empty());
}

// ---------------------------------------------------------------------------
// enrolment keys
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_enrolment_key_enrols_a_sync_device_at_once() {
    let h = harness(|_| {});
    let created = h.create_enroll_key(true).await;
    assert!(
        created.key.starts_with(ENROLL_KEY_PREFIX),
        "{}",
        created.key
    );
    assert!(created.id.starts_with("ek_"));
    assert_eq!((created.tag.as_str(), created.ephemeral), ("cloud", true));

    // The list never shows the key again.
    let (status, raw) = h
        .call("GET", devices::ENROLL_KEYS_PATH, Some(TOKEN), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&raw).contains(&created.key));
    let list: EnrollKeyList = serde_json::from_slice(&raw).unwrap();
    assert_eq!(list.enroll_keys.len(), 1);

    let mut session = Machine::new(6);
    let approved: EnrollApproved = ok(h.enrol_with_key(&session, &created.key).await);
    assert_eq!(approved.scope, "sync");
    assert!(approved.ephemeral);
    session.id = approved.device_id;

    let s = Signing::by(&session);
    let pulled: SyncResponse = ok(h
        .signed("GET", "/sync?project_key=acme%2Fapp", None, &s)
        .await);
    assert_eq!(pulled.project_key, "acme/app");

    let devices: DeviceList = ok(h
        .call("GET", devices::DEVICES_PATH, Some(TOKEN), None)
        .await);
    assert_eq!(
        devices.devices[0].enroll_key_id.as_deref(),
        Some(created.id.as_str())
    );

    // A sync device from a key cannot make itself more.
    let s = Signing::by(&session);
    assert_eq!(
        h.signed(
            "POST",
            devices::ENROLL_KEYS_PATH,
            Some(json!({"expires_in_days": 1})),
            &s
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn an_expired_revoked_or_unknown_enrolment_key_enrols_nothing() {
    let h = harness(|_| {});
    let machine = Machine::new(7);

    let expired = h.create_enroll_key(false).await;
    h.sql(&format!(
        "UPDATE enroll_keys SET expires_at = '2020-01-01T00:00:00.000Z' WHERE id = '{}'",
        expired.id
    ));
    assert_eq!(
        error_of(h.enrol_with_key(&machine, &expired.key).await),
        (
            StatusCode::UNAUTHORIZED,
            "unauthorized: this enrolment key has expired".into()
        )
    );

    let revoked = h.create_enroll_key(false).await;
    // Revoking stops new enrolments, and leaves a device it already made.
    let mut earlier = Machine::new(8);
    let approved: EnrollApproved = ok(h.enrol_with_key(&earlier, &revoked.key).await);
    earlier.id = approved.device_id;
    let key: EnrollKey = ok(h
        .call(
            "POST",
            &devices::revoke_enroll_key_path(&revoked.id),
            Some(TOKEN),
            None,
        )
        .await);
    assert!(key.revoked_at.is_some());
    assert_eq!(
        error_of(h.enrol_with_key(&machine, &revoked.key).await),
        (
            StatusCode::UNAUTHORIZED,
            "unauthorized: this enrolment key has been revoked".into()
        )
    );
    let s = Signing::by(&earlier);
    assert_eq!(
        h.signed("GET", "/sync?project_key=a", None, &s).await.0,
        StatusCode::OK,
        "revoking a key does not revoke what it enrolled"
    );

    assert_eq!(
        error_of(
            h.enrol_with_key(&machine, "recall-ek-thisisnotakeythisserverissued")
                .await
        ),
        (
            StatusCode::UNAUTHORIZED,
            "unauthorized: this enrolment key is not one this server issued".into()
        )
    );

    for body in [
        json!({"expires_in_days": 0}),
        json!({"expires_in_days": 366}),
        json!({"tag": "x"}),
    ] {
        assert_eq!(
            h.call(
                "POST",
                devices::ENROLL_KEYS_PATH,
                Some(TOKEN),
                Some(body.clone())
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "{body}"
        );
    }
}

// ---------------------------------------------------------------------------
// signed requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signed_pushes_and_pulls_are_accepted_and_seen() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(9);
    h.enrol(&mut laptop, "sync").await;

    let s = Signing::by(&laptop);
    let pushed: PushResponse = ok(h
        .signed("POST", "/sync", Some(push_body("# one\n")), &s)
        .await);
    assert!(pushed.ok && !pushed.merged);

    let s = Signing::by(&laptop);
    let pulled: SyncResponse = ok(h
        .signed("GET", "/sync?project_key=acme%2Fapp", None, &s)
        .await);
    assert_eq!(pulled.files[0].content.as_deref(), Some("# one\n"));

    // A device checks the server knows it here; the stats are for admins.
    let s = Signing::by(&laptop);
    let me: DeviceIdentity = ok(h.signed("GET", devices::DEVICES_ME_PATH, None, &s).await);
    assert_eq!(
        (me.device_id.as_str(), me.name.as_str(), me.scope.as_str()),
        (laptop.id.as_str(), "machine-9", "sync")
    );
    assert!(!me.ephemeral);
    assert_eq!(
        error_of(
            h.call("GET", devices::DEVICES_ME_PATH, Some(TOKEN), None)
                .await
        ),
        (
            StatusCode::NOT_FOUND,
            "not a device: this request was authenticated with RECALL_TOKEN".into()
        )
    );
    assert_eq!(
        h.call("GET", devices::DEVICES_ME_PATH, None, None).await.0,
        StatusCode::UNAUTHORIZED
    );

    let list: DeviceList = ok(h
        .call("GET", devices::DEVICES_PATH, Some(TOKEN), None)
        .await);
    assert!(
        list.devices[0].last_seen.is_some(),
        "last_seen was recorded"
    );
}

#[tokio::test]
async fn a_signature_is_refused_for_each_thing_that_is_wrong_with_it() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(10);
    h.enrol(&mut laptop, "sync").await;
    let stranger = Machine::new(11);

    let refused = |(status, body): (StatusCode, Bytes)| {
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice::<ErrorResponse>(&body)
            .unwrap()
            .error
    };

    // A key the server has never enrolled.
    let s = Signing {
        keyid: Some("dev_notenrolled"),
        ..Signing::by(&stranger)
    };
    assert_eq!(
        refused(h.signed("GET", "/sync?project_key=a", None, &s).await),
        "unauthorized: unknown device"
    );

    // An enrolled id, signed with some other key.
    let s = Signing {
        keyid: Some(&laptop.id),
        ..Signing::by(&stranger)
    };
    assert_eq!(
        refused(h.signed("GET", "/sync?project_key=a", None, &s).await),
        "unauthorized: the signature does not verify"
    );

    // Too old, and from too far in the future.
    for skew in [-120, 120] {
        let s = Signing {
            created: unix_now() + skew,
            ..Signing::by(&laptop)
        };
        let why = refused(h.signed("GET", "/sync?project_key=a", None, &s).await);
        assert!(why.contains("check this machine's clock"), "{why}");
    }

    // The same request twice: the second is a replay.
    let s = Signing::by(&laptop);
    assert_eq!(
        h.signed("POST", "/sync", Some(push_body("# once\n")), &s)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        refused(
            h.signed("POST", "/sync", Some(push_body("# once\n")), &s)
                .await
        ),
        "unauthorized: this request was already received once"
    );

    // A body changed after signing.
    let s = Signing {
        tamper_body: Some(push_body("# evil\n").to_string().into_bytes()),
        ..Signing::by(&laptop)
    };
    assert_eq!(
        refused(
            h.signed("POST", "/sync", Some(push_body("# good\n")), &s)
                .await
        ),
        "unauthorized: content-digest does not match the body"
    );

    // Half a signature.
    let mut req = signed_request("GET", "/sync?project_key=a", vec![], &Signing::by(&laptop));
    req.headers_mut().remove(signature::SIGNATURE_HEADER);
    assert_eq!(
        refused({
            let (s, _, b) = h.send(req).await;
            (s, b)
        }),
        "unauthorized: a signed request needs both signature-input and signature"
    );

    // Revoked.
    let _: Device = ok(h
        .call(
            "POST",
            &devices::revoke_device_path(&laptop.id),
            Some(TOKEN),
            None,
        )
        .await);
    let s = Signing::by(&laptop);
    assert_eq!(
        refused(h.signed("GET", "/sync?project_key=a", None, &s).await),
        "unauthorized: this device has been revoked"
    );

    // None of that stored anything.
    let (_, body) = h
        .call("GET", "/sync?project_key=acme%2Fapp", Some(TOKEN), None)
        .await;
    let files: SyncResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(files.files[0].content.as_deref(), Some("# once\n"));
}

#[tokio::test]
async fn a_sync_device_cannot_manage_devices_and_an_admin_device_can() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(12);
    h.enrol(&mut laptop, "sync").await;

    // Every admin route, the old stats among them: nothing gets a 401, a
    // sync device a 403, and neither reaches the handler.
    let key = h.create_enroll_key(true).await;
    let forbidden = "forbidden: this needs RECALL_TOKEN or a device with the admin scope";
    for (method, uri, body) in [
        ("GET", "/admin/stats".to_string(), None),
        ("GET", devices::DEVICES_PATH.to_string(), None),
        ("GET", devices::pending_path("BCDF-GHJK"), None),
        (
            "POST",
            devices::APPROVE_PATH.to_string(),
            Some(json!({"user_code": "BCDF-GHJK"})),
        ),
        (
            "POST",
            devices::DENY_PATH.to_string(),
            Some(json!({"user_code": "BCDF-GHJK"})),
        ),
        ("POST", devices::revoke_device_path(&laptop.id), None),
        ("GET", devices::ENROLL_KEYS_PATH.to_string(), None),
        (
            "POST",
            devices::ENROLL_KEYS_PATH.to_string(),
            Some(json!({"expires_in_days": 1})),
        ),
        ("POST", devices::revoke_enroll_key_path(&key.id), None),
    ] {
        assert_eq!(
            error_of(h.call(method, &uri, None, body.clone()).await),
            (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            "no credential: {method} {uri}"
        );
        let s = Signing::by(&laptop);
        assert_eq!(
            error_of(h.signed(method, &uri, body, &s).await),
            (StatusCode::FORBIDDEN, forbidden.to_string()),
            "sync device: {method} {uri}"
        );
    }
    // And nothing was done: the key is not revoked, the laptop is not.
    let keys: EnrollKeyList = ok(h
        .call("GET", devices::ENROLL_KEYS_PATH, Some(TOKEN), None)
        .await);
    assert!(keys.enroll_keys[0].revoked_at.is_none());
    assert_eq!(keys.enroll_keys.len(), 1);

    // An admin device approves the next machine, with no token involved.
    let mut desktop = Machine::new(13);
    h.enrol(&mut desktop, "admin").await;
    let phone = Machine::new(14);
    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "phone", "public_key": phone.public_key()})),
        )
        .await);
    let s = Signing::by(&desktop);
    let approved: Device = ok(h
        .signed(
            "POST",
            devices::APPROVE_PATH,
            Some(json!({"user_code": pending.user_code, "scope": "sync"})),
            &s,
        )
        .await);
    assert_eq!(approved.name, "phone");

    let s = Signing::by(&desktop);
    let list: DeviceList = ok(h.signed("GET", devices::DEVICES_PATH, None, &s).await);
    assert_eq!(list.devices.len(), 3);
    let s = Signing::by(&desktop);
    assert_eq!(
        h.signed("GET", "/admin/stats", None, &s).await.0,
        StatusCode::OK,
        "an admin device reads the stats"
    );

    let s = Signing::by(&desktop);
    let revoked: Device = ok(h
        .signed("POST", &devices::revoke_device_path(&laptop.id), None, &s)
        .await);
    assert!(revoked.revoked_at.is_some());
}

/// The legacy path is untouched: the operator's token works on every
/// authenticated route, old and new, and nothing else changed about how
/// it is refused.
#[tokio::test]
async fn the_bearer_token_still_works_everywhere() {
    let h = harness(|_| {});
    assert_eq!(
        h.call("POST", "/sync", Some(TOKEN), Some(push_body("x")))
            .await
            .0,
        StatusCode::OK
    );
    for uri in [
        "/sync?project_key=acme%2Fapp",
        "/admin/stats",
        devices::DEVICES_PATH,
        devices::ENROLL_KEYS_PATH,
    ] {
        assert_eq!(
            h.call("GET", uri, Some(TOKEN), None).await.0,
            StatusCode::OK,
            "{uri}"
        );
        assert_eq!(
            error_of(h.call("GET", uri, None, None).await),
            (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            "{uri}"
        );
        assert_eq!(
            error_of(h.call("GET", uri, Some("wrong"), None).await),
            (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            "{uri}"
        );
    }
    let _ = h.create_enroll_key(false).await;
    assert_eq!(
        h.call(
            "POST",
            &devices::revoke_device_path("dev_nosuch"),
            Some(TOKEN),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// ephemeral devices, and the limits every route shares
// ---------------------------------------------------------------------------

#[tokio::test]
async fn idle_ephemeral_devices_are_swept_and_others_are_kept() {
    let h = harness(|c| c.ephemeral_device_ttl = Duration::from_secs(3600));
    let key = h.create_enroll_key(true).await;
    let mut session = Machine::new(15);
    session.id = ok::<EnrollApproved>(h.enrol_with_key(&session, &key.key).await).device_id;
    let mut laptop = Machine::new(16);
    h.enrol(&mut laptop, "sync").await;

    assert_eq!(
        h.server.sweep_devices().unwrap(),
        (0, 0),
        "nobody is idle yet"
    );
    h.sql("UPDATE devices SET created_at = '2020-01-01T00:00:00.000Z', last_seen = NULL");
    assert_eq!(h.server.sweep_devices().unwrap().0, 1);

    let s = Signing::by(&session);
    assert_eq!(
        error_of(h.signed("GET", "/sync?project_key=a", None, &s).await),
        (
            StatusCode::UNAUTHORIZED,
            "unauthorized: unknown device".into()
        )
    );
    let s = Signing::by(&laptop);
    assert_eq!(
        h.signed("GET", "/sync?project_key=a", None, &s).await.0,
        StatusCode::OK,
        "a device that is not ephemeral is never swept"
    );
}

#[tokio::test]
async fn enrolling_is_rate_limited_and_protocol_checked_like_everything_else() {
    let h = harness(|c| c.rate_limit_max = 2);
    let m = Machine::new(17);
    let body = json!({"name": "x", "public_key": m.public_key()});
    for _ in 0..2 {
        assert_eq!(
            h.call("POST", devices::ENROLL_PATH, None, Some(body.clone()))
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        h.call(
            "POST",
            devices::ENROLL_POLL_PATH,
            None,
            Some(json!({"enrollment_id": "x"}))
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let h = harness(|_| {});
    let req = Request::builder()
        .method("POST")
        .uri(devices::ENROLL_PATH)
        .header(recall_wire::PROTOCOL_HEADER, "2")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(h.send(req).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn device_routes_answer_other_methods_with_404_json() {
    let h = harness(|_| {});
    for (method, uri) in [
        ("GET", devices::ENROLL_PATH),
        ("GET", devices::ENROLL_POLL_PATH),
        ("DELETE", devices::DEVICES_PATH),
        ("GET", devices::APPROVE_PATH),
    ] {
        let (status, body) = h.call(method, uri, Some(TOKEN), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(
            serde_json::from_slice::<ErrorResponse>(&body)
                .unwrap()
                .error,
            "not found"
        );
    }
}

/// Every device request fixture is read by today's server. The answers
/// differ, since the ids and keys in them belong to the scratch server
/// they were captured against, but none is that the body did not parse.
#[tokio::test]
async fn every_device_request_fixture_is_understood() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../recall-wire/fixtures/wire");
    let h = harness(|_| {});
    let mut sent = 0;
    for version in std::fs::read_dir(&root).unwrap() {
        let version = version.unwrap().path();
        for (name, uri, want, error) in [
            ("enroll_request.json", devices::ENROLL_PATH, 200, None),
            (
                "enroll_request_with_key.json",
                devices::ENROLL_PATH,
                401,
                Some("unauthorized: this enrolment key is not one this server issued"),
            ),
            (
                "enroll_poll_request.json",
                devices::ENROLL_POLL_PATH,
                400,
                Some("invalid_grant"),
            ),
            (
                "device_approve_request.json",
                devices::APPROVE_PATH,
                404,
                Some("no enrolment is waiting with that code"),
            ),
            (
                "device_deny_request.json",
                devices::DENY_PATH,
                404,
                Some("no enrolment is waiting with that code"),
            ),
            (
                "enroll_key_create_request.json",
                devices::ENROLL_KEYS_PATH,
                200,
                None,
            ),
            (
                "enroll_key_revoke_request.json",
                "/v1/enroll-keys/ek_scratch/revoke",
                404,
                Some("no enrolment key has that id"),
            ),
        ] {
            let Ok(body) = std::fs::read(version.join(name)) else {
                continue;
            };
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let (status, resp) = h.call("POST", uri, Some(TOKEN), Some(body)).await;
            assert_eq!(
                status.as_u16(),
                want,
                "{name}: {}",
                String::from_utf8_lossy(&resp)
            );
            if let Some(error) = error {
                assert_eq!(error_of((status, resp)).1, error, "{name}");
            }
            sent += 1;
        }
    }
    assert!(sent >= 7, "found only {sent} device request fixtures");
}

/// Every push a released client has sent, signed by a device instead of
/// carrying the token, is accepted: signing changes how a request proves
/// who sent it, not what it may say.
#[tokio::test]
async fn every_released_push_is_accepted_when_signed() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../recall-wire/fixtures/wire");
    let h = harness(|_| {});
    let mut laptop = Machine::new(18);
    h.enrol(&mut laptop, "sync").await;
    let mut sent = 0;
    for version in std::fs::read_dir(&root).unwrap() {
        let version = version.unwrap().path();
        for name in ["push_request.json", "push_request_delete.json"] {
            let Ok(body) = std::fs::read(version.join(name)) else {
                continue;
            };
            let req = signed_request("POST", "/sync", body, &Signing::by(&laptop));
            let (status, _, resp) = h.send(req).await;
            assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&resp));
            sent += 1;
        }
    }
    assert!(sent >= 4);
}

// ---------------------------------------------------------------------------
// what the security review found
// ---------------------------------------------------------------------------

/// A device stored directly, for tests that must not spend requests on
/// enrolling it.
fn stored_device(h: &Harness, machine: &mut Machine, scope: &str) {
    let id = format!("dev_{}", machine.name);
    let inserted = h
        .store
        .insert_device(
            &recall_server::store::NewDevice {
                id: &id,
                name: &machine.name,
                public_key: &machine.public_key(),
                scope,
                agent: "",
                ephemeral: false,
                enroll_key_id: None,
                created_at: &recall_server::now(),
            },
            None,
        )
        .unwrap();
    assert!(matches!(inserted, recall_server::store::Inserted::Done(_)));
    machine.id = id;
}

/// M1: the nonce is recorded only once the request has proved itself. A
/// forgery naming the device's id, or the device's own request with its
/// body swapped, carrying nonce N, must not use N up for the real one.
#[tokio::test]
async fn a_refused_request_does_not_use_up_its_nonce() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(30);
    stored_device(&h, &mut laptop, "sync");
    let stranger = Machine::new(31);

    let forged = Signing {
        keyid: Some(&laptop.id),
        nonce: "N".into(),
        ..Signing::by(&stranger)
    };
    assert_eq!(
        h.signed("POST", "/sync", Some(push_body("# forged\n")), &forged)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let tampered = Signing {
        nonce: "N".into(),
        tamper_body: Some(push_body("# swapped\n").to_string().into_bytes()),
        ..Signing::by(&laptop)
    };
    assert_eq!(
        h.signed("POST", "/sync", Some(push_body("# real\n")), &tampered)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    let genuine = Signing {
        nonce: "N".into(),
        ..Signing::by(&laptop)
    };
    assert_eq!(
        h.signed("POST", "/sync", Some(push_body("# real\n")), &genuine)
            .await
            .0,
        StatusCode::OK,
        "the refused requests used up the genuine one's nonce"
    );
}

/// M2: the window is sixty seconds behind and five ahead, not a second
/// more, and the nonce is remembered for as long as the window accepts its
/// request, so the request is refused again after the window rather than
/// accepted.
#[tokio::test]
async fn the_window_is_sixty_seconds_and_a_replay_after_it_is_refused() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(32);
    stored_device(&h, &mut laptop, "sync");

    // Checked a second or so inside each edge, so a clock that ticks while
    // the request is in flight cannot move it across.
    for skew in [7, 8, 30, 60, 61, -62, -63, -64, -65] {
        let s = Signing {
            created: unix_now() + skew,
            ..Signing::by(&laptop)
        };
        let (status, why) = error_of(h.signed("GET", "/sync?project_key=a", None, &s).await);
        assert_eq!(status, StatusCode::UNAUTHORIZED, "skew {skew}");
        assert!(
            why.contains("check this machine's clock"),
            "skew {skew}: {why}"
        );
    }
    for (skew, edge) in [(5, "five seconds ahead"), (-59, "a minute behind")] {
        let s = Signing {
            created: unix_now() + skew,
            ..Signing::by(&laptop)
        };
        assert_eq!(
            h.signed("GET", "/sync?project_key=a", None, &s).await.0,
            StatusCode::OK,
            "{edge} is inside"
        );
    }

    let once = Signing::by(&laptop);
    assert_eq!(
        h.signed("GET", "/sync?project_key=a", None, &once).await.0,
        StatusCode::OK
    );
    // Sixty-one seconds later, by the server's clock.
    h.server.set_clock_offset(61);
    let (status, why) = error_of(h.signed("GET", "/sync?project_key=a", None, &once).await);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(why.contains("check this machine's clock"), "{why}");
}

fn big_push() -> Vec<u8> {
    // Six MiB, over the five the server takes.
    let content = "x".repeat(6 << 20);
    serde_json::to_vec(&json!({"project_key": "a", "file_path": "big.md", "content": content}))
        .unwrap()
}

/// Review finding 1: a signed request's body is read only after its
/// headers have proved it is the device's. Before, any request with a
/// signature header made the server read up to 5 MiB.
#[tokio::test]
async fn nobody_without_a_device_key_makes_the_server_read_a_body() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(33);
    stored_device(&h, &mut laptop, "sync");
    let body = big_push();

    // A signature header alone: refused as unauthorized, not as too large,
    // because the body was never looked at.
    let req = Request::builder()
        .method("POST")
        .uri("/sync")
        .header("content-length", body.len())
        .header("signature", "x")
        .body(Body::from(body.clone()))
        .unwrap();
    assert_eq!(h.send(req).await.0, StatusCode::UNAUTHORIZED);

    // The device's id, a stranger's key: the same.
    let stranger = Machine::new(34);
    let forged = Signing {
        keyid: Some(&laptop.id),
        ..Signing::by(&stranger)
    };
    let req = signed_request("POST", "/sync", body.clone(), &forged);
    assert_eq!(h.send(req).await.0, StatusCode::UNAUTHORIZED);

    // The device itself, declaring too much: refused before it is read.
    let mut req = signed_request("POST", "/sync", body.clone(), &Signing::by(&laptop));
    req.headers_mut()
        .insert("content-length", body.len().to_string().parse().unwrap());
    let (status, _, resp) = h.send(req).await;
    assert_eq!(
        error_of((status, resp)),
        (
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body too large".into()
        )
    );
    // And not declaring it: refused once the limit is reached.
    let req = signed_request("POST", "/sync", body, &Signing::by(&laptop));
    assert_eq!(h.send(req).await.0, StatusCode::PAYLOAD_TOO_LARGE);
}

/// Review finding 1, the other half: the routes anyone may call take 8 KiB,
/// not 5 MiB, and say so in the usual JSON.
#[tokio::test]
async fn the_enrolment_routes_take_small_bodies_only() {
    let h = harness(|_| {});
    let padded = json!({"name": "x", "public_key": Machine::new(35).public_key(), "agent": "a".repeat(9 << 10)});
    for (uri, declared) in [
        (devices::ENROLL_PATH, true),
        (devices::ENROLL_PATH, false),
        (devices::ENROLL_POLL_PATH, true),
    ] {
        let body = padded.to_string();
        let mut req = Request::builder().method("POST").uri(uri);
        if declared {
            req = req.header("content-length", body.len());
        }
        let (status, _, resp) = h.send(req.body(Body::from(body)).unwrap()).await;
        assert_eq!(
            error_of((status, resp)),
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large".into()
            ),
            "{uri}, declared: {declared}"
        );
    }
}

/// Review finding 2: one device signing more than one address may is
/// refused on its own, and the others carry on.
#[tokio::test]
async fn a_device_that_signs_too_much_is_refused_alone() {
    // Two a minute per address: a device may have four nonces live, since
    // the sixty-five seconds one lives span two of the limiter's minutes.
    let h = harness(|c| c.rate_limit_max = 2);
    let mut greedy = Machine::new(36);
    stored_device(&h, &mut greedy, "sync");
    let mut other = Machine::new(37);
    stored_device(&h, &mut other, "sync");

    let from = |machine: &Machine, ip: &str| {
        let mut req = signed_request("GET", "/sync?project_key=a", vec![], &Signing::by(machine));
        req.headers_mut()
            .insert("cf-connecting-ip", ip.parse().unwrap());
        req
    };
    // Each from its own address, so the rate limiter lets every one by.
    for i in 0..4 {
        assert_eq!(
            h.send(from(&greedy, &format!("198.51.100.{i}"))).await.0,
            StatusCode::OK
        );
    }
    let (status, _, body) = h.send(from(&greedy, "198.51.100.9")).await;
    assert_eq!(
        error_of((status, body)),
        (
            StatusCode::TOO_MANY_REQUESTS,
            "too many signed requests from this device, try again later".into()
        )
    );
    assert_eq!(
        h.send(from(&other, "198.51.100.10")).await.0,
        StatusCode::OK
    );
}

/// Review finding 3: an approval can name the fingerprint the approver
/// saw, and then approves that key or nothing. The lookup before it is
/// never cached.
#[tokio::test]
async fn an_approval_can_be_bound_to_the_key_the_approver_saw() {
    let h = harness(|_| {});
    let laptop = Machine::named(38, "laptop");
    let pending: EnrollPending = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "laptop", "public_key": laptop.public_key()})),
        )
        .await);

    let req = Request::builder()
        .uri(devices::pending_path(&pending.user_code))
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, body) = h.send(req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    let seen: PendingEnrollment = serde_json::from_slice(&body).unwrap();

    let other = fingerprint(&Machine::new(39).key.verifying_key());
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": pending.user_code, "fingerprint": other})),
            )
            .await
        ),
        (
            StatusCode::CONFLICT,
            "that code's key does not have the fingerprint given; nothing was approved".into()
        )
    );
    assert_eq!(
        error_of(h.poll(&pending.enrollment_id).await).1,
        "authorization_pending",
        "a refused approval approves nothing"
    );

    let device: Device = ok(h
        .call(
            "POST",
            devices::APPROVE_PATH,
            Some(TOKEN),
            Some(json!({"user_code": pending.user_code, "fingerprint": seen.fingerprint})),
        )
        .await);
    assert_eq!(device.fingerprint, seen.fingerprint);
}

/// Review finding 4: a name cannot hide characters, two live devices
/// cannot share one, and a device an enrolment key enrols is named by the
/// server, not by itself.
#[tokio::test]
async fn names_are_plain_unique_and_not_chosen_by_key_enrolments() {
    let h = harness(|_| {});
    let key = Machine::new(40).public_key();
    for name in ["lap\u{200B}top", "\u{202E}potpal", "laptop\u{FEFF}"] {
        assert_eq!(
            h.call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": name, "public_key": key})),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "{name:?}"
        );
    }

    let mut laptop = Machine::named(41, "laptop");
    h.enrol(&mut laptop, "sync").await;
    let clash = "a device named Laptop already exists; revoke it first, or enrol with another name";
    assert_eq!(
        error_of(
            h.call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": "Laptop", "public_key": key})),
            )
            .await
        ),
        (StatusCode::CONFLICT, clash.into())
    );

    // Two waiting under one name: the first approved takes it.
    let enrol_as = |name: &'static str, seed: u8| {
        let h = &h;
        async move {
            let m = Machine::new(seed);
            let p: EnrollPending = ok(h
                .call(
                    "POST",
                    devices::ENROLL_PATH,
                    None,
                    Some(json!({"name": name, "public_key": m.public_key()})),
                )
                .await);
            p.user_code
        }
    };
    let first = enrol_as("desk", 42).await;
    let second = enrol_as("desk", 43).await;
    let approve = |code: String| {
        let h = &h;
        async move {
            h.call(
                "POST",
                devices::APPROVE_PATH,
                Some(TOKEN),
                Some(json!({"user_code": code})),
            )
            .await
        }
    };
    let desk: Device = ok(approve(first).await);
    assert_eq!(approve(second.clone()).await.0, StatusCode::CONFLICT);
    // Revoked, its name is free again.
    let _: Device = ok(h
        .call(
            "POST",
            &devices::revoke_device_path(&desk.id),
            Some(TOKEN),
            None,
        )
        .await);
    let _: Device = ok(approve(second).await);

    let created = h.create_enroll_key(true).await;
    let approved: EnrollApproved = ok(h
        .call(
            "POST",
            devices::ENROLL_PATH,
            None,
            Some(json!({"name": "laptop", "public_key": Machine::new(44).public_key(), "enroll_key": created.key})),
        )
        .await);
    assert!(
        approved.name.starts_with("cloud-") && approved.name.len() == "cloud-".len() + 8,
        "{}",
        approved.name
    );
    assert!(approved
        .device_id
        .contains(&approved.name["cloud-".len()..]));
}

/// Verification finding N2: a name that reads as a device's name is that
/// name, whether it differs in script (a Cyrillic `а`), in normal form
/// (`é` as one character or two) or by case folding (`ß` and `SS`). And a
/// name is stored composed, however it was sent.
#[tokio::test]
async fn a_name_that_looks_like_a_devices_is_that_name() {
    let h = harness(|_| {});
    for (seed, taken, stored, lookalike) in [
        (60, "laptop", "laptop", "l\u{0430}ptop"),
        (62, "cafe\u{0301}", "caf\u{00E9}", "caf\u{00E9}"),
        (64, "STRASSE", "STRASSE", "Stra\u{00DF}e"),
    ] {
        let mut first = Machine::named(seed, taken);
        assert_eq!(h.enrol(&mut first, "sync").await.name, stored);
        let (status, why) = error_of(
            h.call(
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": lookalike, "public_key": Machine::new(seed + 1).public_key()})),
            )
            .await,
        );
        assert_eq!(status, StatusCode::CONFLICT, "{lookalike:?}: {why}");
    }
}

/// Review finding 5: nonces live in memory, so a server that has just
/// started refuses a signature made before it did, which the process
/// before it may already have accepted.
#[tokio::test]
async fn a_signature_made_before_a_restart_is_refused() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(45);
    stored_device(&h, &mut laptop, "sync");
    let before = Signing::by(&laptop);
    assert_eq!(
        h.signed("GET", "/sync?project_key=a", None, &before)
            .await
            .0,
        StatusCode::OK
    );

    // The next second, a new process on the same database.
    while unix_now() <= before.created {
        std::thread::sleep(Duration::from_millis(50));
    }
    let restarted = restarted(&h);
    let started = unix_now();
    assert_eq!(
        error_of(
            restarted
                .signed("GET", "/sync?project_key=a", None, &before)
                .await
        ),
        (StatusCode::UNAUTHORIZED, TOO_SOON.into())
    );

    // Signed again a second later, by a clock as far ahead as it may be,
    // the request is dated past what the process before could have
    // accepted, and goes through.
    while unix_now() <= started {
        std::thread::sleep(Duration::from_millis(50));
    }
    let after = Signing {
        created: unix_now() + 5,
        ..Signing::by(&laptop)
    };
    assert_eq!(
        restarted
            .signed("GET", "/sync?project_key=a", None, &after)
            .await
            .0,
        StatusCode::OK
    );
}

/// Verification finding N1: a signature dated ahead of the clock outlived
/// the process that accepted it. One dated thirty seconds ahead was
/// accepted, and accepted again by a new process started any time in
/// those thirty seconds, whose nonce cache was empty. Now `created` may be
/// only five seconds ahead, and a new process refuses everything dated up
/// to five seconds past its start.
#[tokio::test]
async fn a_signature_dated_ahead_is_not_replayed_after_a_restart() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(57);
    stored_device(&h, &mut laptop, "sync");

    let far_ahead = Signing {
        created: unix_now() + 30,
        ..Signing::by(&laptop)
    };
    let (status, why) = error_of(
        h.signed("POST", "/sync", Some(push_body("# ahead\n")), &far_ahead)
            .await,
    );
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(why.contains("ahead of the server's clock"), "{why}");

    let ahead = Signing {
        created: unix_now() + 5,
        ..Signing::by(&laptop)
    };
    assert_eq!(
        h.signed("POST", "/sync", Some(push_body("# ahead\n")), &ahead)
            .await
            .0,
        StatusCode::OK
    );
    let restarted = restarted(&h);
    assert_eq!(
        error_of(
            restarted
                .signed("POST", "/sync", Some(push_body("# ahead\n")), &ahead)
                .await
        ),
        (StatusCode::UNAUTHORIZED, TOO_SOON.into())
    );
}

/// N1's other half: a signature made in the very second a new process
/// starts. Refusing only what was made before the start let one that the
/// process before accepted, in that same second, be accepted again.
#[tokio::test]
async fn a_signature_from_the_second_of_a_restart_is_not_replayed() {
    let h = harness(|_| {});
    let mut laptop = Machine::new(58);
    stored_device(&h, &mut laptop, "sync");

    // A slow machine may tick over between the two; a few tries find a
    // second both land in.
    for _ in 0..5 {
        while SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_millis()
            > 300
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let once = Signing::by(&laptop);
        assert_eq!(
            h.signed("GET", "/sync?project_key=a", None, &once).await.0,
            StatusCode::OK
        );
        let restarted = restarted(&h);
        let same_second = unix_now() == once.created;
        assert_eq!(
            error_of(
                restarted
                    .signed("GET", "/sync?project_key=a", None, &once)
                    .await
            ),
            (StatusCode::UNAUTHORIZED, TOO_SOON.into())
        );
        if same_second {
            return;
        }
    }
    panic!("never managed to sign and restart inside one second");
}

/// Review finding 6: a key's devices are ephemeral unless asked, a key can
/// be capped, and revoking a key can revoke what it enrolled.
#[tokio::test]
async fn enrolment_keys_are_ephemeral_by_default_capped_and_revocable_with_their_devices() {
    let h = harness(|_| {});
    let created: EnrollKeyCreated = ok(h
        .call(
            "POST",
            devices::ENROLL_KEYS_PATH,
            Some(TOKEN),
            Some(json!({"tag": "ci", "expires_in_days": 7, "max_devices": 1})),
        )
        .await);
    assert!(created.ephemeral, "ephemeral unless asked");
    assert_eq!(created.max_devices, Some(1));

    let mut first = Machine::new(46);
    let approved: EnrollApproved = ok(h.enrol_with_key(&first, &created.key).await);
    assert!(approved.ephemeral);
    first.id = approved.device_id;
    assert_eq!(
        error_of(h.enrol_with_key(&Machine::new(47), &created.key).await),
        (
            StatusCode::FORBIDDEN,
            "forbidden: this enrolment key already has its 1 devices; revoke one, or make another key"
                .into()
        )
    );

    let _: EnrollKey = ok(h
        .call(
            "POST",
            &devices::revoke_enroll_key_path(&created.id),
            Some(TOKEN),
            Some(json!({"revoke_devices": true})),
        )
        .await);
    assert_eq!(
        error_of(
            h.signed("GET", "/sync?project_key=a", None, &Signing::by(&first))
                .await
        ),
        (
            StatusCode::UNAUTHORIZED,
            "unauthorized: this device has been revoked".into()
        )
    );
    assert_eq!(
        h.call(
            "POST",
            devices::ENROLL_KEYS_PATH,
            Some(TOKEN),
            Some(json!({"expires_in_days": 7, "max_devices": 0})),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

/// Review finding 7: one address may have five enrolments waiting, so it
/// cannot fill the queue and lock the owner's machines out.
#[tokio::test]
async fn one_address_cannot_hold_the_enrolment_queue() {
    let h = harness(|_| {});
    let enrol_from = |ip: &'static str, seed: u8| {
        let h = &h;
        async move {
            h.call_from(
                ip,
                "POST",
                devices::ENROLL_PATH,
                None,
                Some(json!({"name": format!("m{seed}"), "public_key": Machine::new(seed).public_key()})),
            )
            .await
        }
    };
    for seed in 50..55 {
        assert_eq!(enrol_from("203.0.113.1", seed).await.0, StatusCode::OK);
    }
    let (status, body) = enrol_from("203.0.113.1", 55).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        error_of((status, body)).1.contains("from this address"),
        "the refusal says why"
    );
    assert_eq!(enrol_from("203.0.113.2", 56).await.0, StatusCode::OK);
}
