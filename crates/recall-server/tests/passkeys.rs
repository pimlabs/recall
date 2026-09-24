//! The admin page's passkey sign-in, end to end through the router, with a
//! software authenticator standing in for a phone.
//!
//! 1Password's `passkey-client` plays the browser: it builds the client
//! data from the origin it is given, as a browser does, and its
//! authenticator signs with a real P-256 key. So what `webauthn-rs`
//! verifies here is what it would verify from a phone, and a mistake in
//! how the server hands challenges out or checks answers fails these.

#![cfg(feature = "passkeys")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use passkey_authenticator::{Authenticator, UiHint, UserCheck, UserValidationMethod};
use passkey_client::{Client, DefaultClientData};
use passkey_types::ctap2::{Aaguid, Ctap2Error};
use passkey_types::webauthn::{CredentialCreationOptions, CredentialRequestOptions};
use passkey_types::Passkey;
use recall_server::{Config, Server, Store};
use recall_wire::signature::{
    encode_public_key, fingerprint, sign_request, SigningKey, Target, CONTENT_DIGEST_HEADER,
    SIGNATURE_HEADER, SIGNATURE_INPUT_HEADER,
};
use recall_wire::{AuthkeyCreated, Device, DeviceList, EnrollPending, PendingEnrollment};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt;
use url::Url;

const TOKEN: &str = "admin-test-token";
const ORIGIN: &str = "https://recall.example.com";
const COOKIE: &str = "__Host-recall_admin";

/// A person who always approves, with a verified user: the phone's
/// biometric prompt, answered.
struct Approves;

// The trait is an async_trait, so its impl must be too, or the lifetimes
// of `check_user` do not line up (E0195).
#[async_trait::async_trait]
impl UserValidationMethod for Approves {
    type PasskeyItem = Passkey;

    async fn check_user<'a>(
        &self,
        _hint: UiHint<'a, Passkey>,
        presence: bool,
        verification: bool,
    ) -> Result<UserCheck, Ctap2Error> {
        Ok(UserCheck {
            presence,
            verification,
        })
    }

    fn is_presence_enabled(&self) -> bool {
        true
    }

    fn is_verification_enabled(&self) -> Option<bool> {
        Some(true)
    }
}

/// A phone holding at most one passkey, as a browser would drive it.
type Phone = Client<Option<Passkey>, Approves, public_suffix::PublicSuffixList, ()>;

fn phone() -> Phone {
    Client::new(Authenticator::new(Aaguid::new_empty(), None, Approves))
}

/// A hardware key that counts its signatures, where a synced passkey
/// reports zero every time.
fn counting_key() -> Phone {
    let mut authenticator = Authenticator::new(Aaguid::new_empty(), None, Approves);
    authenticator.set_make_credentials_with_signature_counter(true);
    Client::new(authenticator)
}

struct Harness {
    server: Server,
    _dir: TempDir,
    store: Arc<Store>,
}

fn harness_with(public_url: &str) -> Harness {
    harness_configured(public_url, 10_000)
}

/// A harness whose rate limit is `max` requests a minute.
fn harness_limited(max: u32) -> Harness {
    harness_configured(ORIGIN, max)
}

fn harness_configured(public_url: &str, rate_limit_max: u32) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let cfg = Config {
        token: TOKEN.to_string(),
        merge_enabled: false,
        rate_limit_max,
        public_url: public_url.to_string(),
        ..Config::default()
    };
    Harness {
        server: Server::new(cfg, store.clone()),
        _dir: dir,
        store,
    }
}

fn harness() -> Harness {
    harness_with(ORIGIN)
}

/// How a request authenticates.
#[derive(Clone, Copy)]
enum As<'a> {
    Nobody,
    Token,
    /// The session cookie, and the CSRF header when there is one.
    Session(&'a Session, Option<&'a str>),
}

#[derive(Debug, Clone)]
struct Session {
    cookie: String,
    csrf: String,
}

impl Session {
    fn with_csrf(&self) -> As<'_> {
        As::Session(self, Some(&self.csrf))
    }
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Value,
}

impl Harness {
    async fn call(&self, method: &str, uri: &str, who: As<'_>, body: Option<Value>) -> Reply {
        self.call_typed(method, uri, who, Some("application/json"), body)
            .await
    }

    /// [`Harness::call`], saying the body is `content_type`, or nothing.
    async fn call_typed(
        &self,
        method: &str,
        uri: &str,
        who: As<'_>,
        content_type: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            req = req.header("content-type", content_type);
        }
        match who {
            As::Nobody => {}
            As::Token => req = req.header("authorization", format!("Bearer {TOKEN}")),
            As::Session(session, csrf) => {
                req = req.header("cookie", format!("{COOKIE}={}", session.cookie));
                if let Some(csrf) = csrf {
                    req = req.header("x-recall-csrf", csrf);
                }
            }
        }
        let body = match body {
            Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
            None => Body::empty(),
        };
        self.raw(req.body(body).unwrap()).await
    }

    /// Sends a request built by hand.
    async fn raw(&self, req: Request<Body>) -> Reply {
        let resp = self.server.router().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            headers,
            body,
        }
    }

    /// Starts a sign-in as a request from `ip` would, behind the ingress
    /// that names it.
    async fn start_from(&self, ip: &str) -> Reply {
        self.raw(
            Request::post("/admin/login/start")
                .header("content-type", "application/json")
                .header("cf-connecting-ip", ip)
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// Finishes a sign-in with `credential`, an answer to `started`.
    async fn finish_sign_in(&self, started: &Reply, credential: Value) -> Reply {
        self.call(
            "POST",
            "/admin/login/finish",
            As::Nobody,
            Some(json!({
                "ceremony_id": started.body["ceremony_id"],
                "credential": credential,
            })),
        )
        .await
    }

    /// Enrols a machine and approves it with the token, with `scope`. Its
    /// id.
    async fn device(&self, key: &SigningKey, name: &str, scope: &str) -> String {
        let (code, _) = enrolling_with(self, key, name).await;
        let approved = self
            .call(
                "POST",
                "/v1/devices/approve",
                As::Token,
                Some(json!({ "user_code": code, "scope": scope })),
            )
            .await;
        assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
        approved.body["id"].as_str().unwrap().to_string()
    }

    /// Registers `phone`'s passkey as the first, with the token.
    async fn bootstrap(&self, phone: &mut Phone) -> Reply {
        let started = self
            .call("POST", "/admin/bootstrap/register", As::Token, None)
            .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        let credential = create(phone, &started.body["options"]).await;
        self.call(
            "POST",
            "/admin/bootstrap/register/finish",
            As::Token,
            Some(json!({
                "ceremony_id": started.body["ceremony_id"],
                "name": "iPhone",
                "credential": credential,
            })),
        )
        .await
    }

    /// Signs in with `phone`'s passkey. The reply to the finish.
    async fn try_sign_in(&self, phone: &mut Phone) -> Reply {
        let started = self
            .call("POST", "/admin/login/start", As::Nobody, None)
            .await;
        assert_eq!(started.status, StatusCode::OK, "{}", started.body);
        let credential = get(phone, &started.body["options"]).await;
        self.call(
            "POST",
            "/admin/login/finish",
            As::Nobody,
            Some(json!({
                "ceremony_id": started.body["ceremony_id"],
                "credential": credential,
            })),
        )
        .await
    }

    async fn sign_in(&self, phone: &mut Phone) -> Session {
        let reply = self.try_sign_in(phone).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
        session_from(&reply)
    }
}

/// `navigator.credentials.create()`, as a browser at [`ORIGIN`] runs it.
async fn create(phone: &mut Phone, options: &Value) -> Value {
    create_at(phone, ORIGIN, options)
        .await
        .expect("the authenticator registers")
}

/// `navigator.credentials.get()`.
async fn get(phone: &mut Phone, options: &Value) -> Value {
    get_at(phone, ORIGIN, options)
        .await
        .expect("the authenticator signs")
}

/// `navigator.credentials.create()` on a page at `at`, which the client
/// may refuse, as a browser would.
async fn create_at(phone: &mut Phone, at: &str, options: &Value) -> Result<Value, String> {
    let options: CredentialCreationOptions = serde_json::from_value(options.clone()).unwrap();
    phone
        .register(&Url::parse(at).unwrap(), options, DefaultClientData)
        .await
        .map(|c| serde_json::to_value(c).unwrap())
        .map_err(|e| format!("{e:?}"))
}

/// `navigator.credentials.get()` on a page at `at`.
async fn get_at(phone: &mut Phone, at: &str, options: &Value) -> Result<Value, String> {
    let options: CredentialRequestOptions = serde_json::from_value(options.clone()).unwrap();
    phone
        .authenticate(&Url::parse(at).unwrap(), options, DefaultClientData)
        .await
        .map(|a| serde_json::to_value(a).unwrap())
        .map_err(|e| format!("{e:?}"))
}

fn session_from(reply: &Reply) -> Session {
    let set_cookie = reply
        .headers
        .get(header::SET_COOKIE)
        .expect("signing in sets the cookie")
        .to_str()
        .unwrap();
    let value = set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.strip_prefix(&format!("{COOKIE}=")))
        .expect("the session cookie")
        .to_string();
    Session {
        cookie: value,
        csrf: reply.body["csrf_token"].as_str().unwrap().to_string(),
    }
}

/// A machine waiting to be approved: its code and its key's fingerprint.
async fn enrolling(h: &Harness, seed: u8, name: &str) -> (String, String) {
    enrolling_with(h, &SigningKey::from_bytes(&[seed; 32]), name).await
}

async fn enrolling_with(h: &Harness, key: &SigningKey, name: &str) -> (String, String) {
    let reply = h
        .call(
            "POST",
            "/v1/devices/enroll",
            As::Nobody,
            Some(json!({
                "name": name,
                "public_key": encode_public_key(&key.verifying_key()),
                "agent": "recall/test",
            })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let pending: EnrollPending = serde_json::from_value(reply.body).unwrap();
    (pending.user_code, fingerprint(&key.verifying_key()))
}

#[tokio::test]
async fn bootstrap_then_sign_in_then_the_session_manages_devices() {
    let h = harness();
    let status = h.call("GET", "/admin/session", As::Nobody, None).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.body["passkeys"]["enabled"], true);
    assert_eq!(status.body["passkeys"]["origin"], ORIGIN);
    assert_eq!(status.body["bootstrapped"], false);
    assert_eq!(status.body["session"], Value::Null);

    let mut phone = phone();
    let registered = h.bootstrap(&mut phone).await;
    assert_eq!(registered.status, StatusCode::OK, "{}", registered.body);
    assert_eq!(registered.body["name"], "iPhone");
    assert_eq!(
        h.call("GET", "/admin/session", As::Nobody, None).await.body["bootstrapped"],
        true
    );

    let reply = h.try_sign_in(&mut phone).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let set_cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
    for attr in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(set_cookie.contains(attr), "{set_cookie}");
    }
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let session = session_from(&reply);
    // Only the hash is stored.
    assert!(h.store.admin_session(&session.cookie).unwrap().is_none());
    assert!(h
        .store
        .admin_session(&recall_wire::content_sha256(&session.cookie))
        .unwrap()
        .is_some());

    // The page asks what it holds, and hears the same CSRF token.
    let status = h
        .call("GET", "/admin/session", As::Session(&session, None), None)
        .await;
    assert_eq!(status.body["session"]["csrf_token"], session.csrf.as_str());

    // Reading needs only the cookie.
    let stats = h
        .call("GET", "/admin/stats", As::Session(&session, None), None)
        .await;
    assert_eq!(stats.status, StatusCode::OK, "{}", stats.body);
    let passkeys = h
        .call("GET", "/admin/passkeys", As::Session(&session, None), None)
        .await;
    assert_eq!(passkeys.body["passkeys"][0]["current"], true);

    // Approve by code: look first, then approve with the fingerprint seen.
    let (code, print) = enrolling(&h, 1, "laptop").await;
    let looked = h
        .call(
            "GET",
            &format!("/v1/devices/pending/{code}"),
            As::Session(&session, None),
            None,
        )
        .await;
    assert_eq!(looked.status, StatusCode::OK, "{}", looked.body);
    let looked: PendingEnrollment = serde_json::from_value(looked.body).unwrap();
    assert_eq!(
        (looked.name.as_str(), looked.agent.as_str()),
        ("laptop", "recall/test")
    );
    assert_eq!(looked.fingerprint, print);
    let approved = h
        .call(
            "POST",
            "/v1/devices/approve",
            session.with_csrf(),
            Some(json!({ "user_code": code, "fingerprint": looked.fingerprint })),
        )
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let device: Device = serde_json::from_value(approved.body).unwrap();

    let listed = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    let listed: DeviceList = serde_json::from_value(listed.body).unwrap();
    assert_eq!(listed.devices.len(), 1);

    // An authkey, shown once, and revoked.
    let key = h
        .call(
            "POST",
            "/v1/authkeys",
            session.with_csrf(),
            Some(json!({ "tag": "cloud", "expires_in_days": 30 })),
        )
        .await;
    assert_eq!(key.status, StatusCode::OK, "{}", key.body);
    let key: AuthkeyCreated = serde_json::from_value(key.body).unwrap();
    let revoked = h
        .call(
            "POST",
            &format!("/v1/authkeys/{}/revoke", key.id),
            session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    let revoked = h
        .call(
            "POST",
            &format!("/v1/devices/{}/revoke", device.id),
            session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);

    // Signing out ends it at the server, not just in the browser.
    let out = h
        .call("POST", "/admin/logout", session.with_csrf(), None)
        .await;
    assert_eq!(out.status, StatusCode::OK);
    assert!(out.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));
    let after = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_bootstrap_is_refused_once_a_passkey_exists_whatever_the_token() {
    let h = harness();
    // Two bootstraps started while none exists; only one may finish.
    let first = h
        .call("POST", "/admin/bootstrap/register", As::Token, None)
        .await;
    let second = h
        .call("POST", "/admin/bootstrap/register", As::Token, None)
        .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(second.status, StatusCode::OK);

    let mut phone = phone();
    let credential = create(&mut phone, &first.body["options"]).await;
    let done = h
        .call(
            "POST",
            "/admin/bootstrap/register/finish",
            As::Token,
            Some(json!({ "ceremony_id": first.body["ceremony_id"], "credential": credential })),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);

    // Starting again, with the valid token: refused.
    let again = h
        .call("POST", "/admin/bootstrap/register", As::Token, None)
        .await;
    assert_eq!(again.status, StatusCode::FORBIDDEN, "{}", again.body);
    assert!(again.body["error"]
        .as_str()
        .unwrap()
        .contains("a passkey is registered already"));

    // Finishing the second ceremony, begun before the first finished, with
    // another authenticator: refused too.
    let mut intruder = phone_two();
    let credential = create(&mut intruder, &second.body["options"]).await;
    let late = h
        .call(
            "POST",
            "/admin/bootstrap/register/finish",
            As::Token,
            Some(json!({ "ceremony_id": second.body["ceremony_id"], "credential": credential })),
        )
        .await;
    assert_eq!(late.status, StatusCode::FORBIDDEN, "{}", late.body);
    assert_eq!(h.store.admin_credentials().unwrap().len(), 1);

    // No token at all is the usual 401, and a session is not the token.
    let none = h
        .call("POST", "/admin/bootstrap/register", As::Nobody, None)
        .await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    let session = h.sign_in(&mut phone).await;
    let with_session = h
        .call(
            "POST",
            "/admin/bootstrap/register",
            session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(with_session.status, StatusCode::UNAUTHORIZED);
}

fn phone_two() -> Phone {
    phone()
}

#[tokio::test]
async fn a_state_changing_request_without_the_right_csrf_token_is_403() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let session = h.sign_in(&mut phone).await;
    let (code, _) = enrolling(&h, 2, "laptop").await;
    let approve = json!({ "user_code": code });

    for (label, csrf) in [
        ("missing", None),
        ("wrong", Some("not-the-token")),
        ("empty", Some("")),
        ("another session's shape", Some(&session.cookie[..])),
        // Compared whole: a prefix of the right token is not the token.
        ("a prefix of it", Some(&session.csrf[..10])),
        (
            "all but its last character",
            Some(&session.csrf[..session.csrf.len() - 1]),
        ),
    ] {
        let reply = h
            .call(
                "POST",
                "/v1/devices/approve",
                As::Session(&session, csrf),
                Some(approve.clone()),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::FORBIDDEN,
            "{label}: {}",
            reply.body
        );
        assert_eq!(
            reply.body["error"],
            "forbidden: this needs the admin session's X-Recall-CSRF header"
        );
        for (method, uri) in [
            ("POST", "/v1/authkeys"),
            ("POST", "/admin/logout"),
            ("POST", "/admin/passkeys/register"),
        ] {
            let reply = h
                .call(method, uri, As::Session(&session, csrf), Some(json!({})))
                .await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{label} {uri}");
        }
    }
    // Nothing was approved.
    let pending = h
        .call(
            "GET",
            &format!("/v1/devices/pending/{code}"),
            As::Session(&session, None),
            None,
        )
        .await;
    assert_eq!(pending.status, StatusCode::OK, "still waiting");
    let ok = h
        .call(
            "POST",
            "/v1/devices/approve",
            session.with_csrf(),
            Some(approve),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn an_idle_or_expired_session_is_refused() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;

    // Idle: twelve hours without a request.
    let session = h.sign_in(&mut phone).await;
    h.server.set_clock_offset(11 * 3600);
    let used = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(used.status, StatusCode::OK, "within twelve hours");
    // Using it moved the idle limit on: 22h after sign-in is 11h after use.
    h.server.set_clock_offset(22 * 3600);
    let used = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(used.status, StatusCode::OK, "refreshed by use");
    h.server.set_clock_offset(22 * 3600 + 12 * 3600);
    let idle = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(idle.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        idle.body["error"],
        "unauthorized: the admin session has ended; sign in again"
    );

    // Absolute: thirty days, however often it is used.
    h.server.set_clock_offset(0);
    let session = h.sign_in(&mut phone).await;
    let mut offset = 0;
    while offset + 11 * 3600 < 30 * 24 * 3600 {
        offset += 11 * 3600;
        h.server.set_clock_offset(offset);
        let reply = h
            .call("GET", "/v1/devices", As::Session(&session, None), None)
            .await;
        assert_eq!(reply.status, StatusCode::OK, "at {offset}s");
    }
    h.server.set_clock_offset(30 * 24 * 3600);
    let expired = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(expired.status, StatusCode::UNAUTHORIZED);

    // A cookie the server never issued, and a malformed one.
    let forged = Session {
        cookie: "A".repeat(43),
        csrf: String::new(),
    };
    let reply = h
        .call("GET", "/v1/devices", As::Session(&forged, None), None)
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let status = h
        .call("GET", "/admin/session", As::Session(&forged, None), None)
        .await;
    assert_eq!(status.body["session"], Value::Null);
    assert!(status.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .contains("Max-Age=0"));
}

#[tokio::test]
async fn the_session_cookie_is_never_accepted_on_sync() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let session = h.sign_in(&mut phone).await;
    for (method, uri, body) in [
        ("GET", "/sync?project_key=a/b", None),
        (
            "POST",
            "/sync",
            Some(json!({ "project_key": "a/b", "file_path": "MEMORY.md", "content": "x" })),
        ),
        ("GET", "/v1/devices/me", None),
    ] {
        let reply = h.call(method, uri, session.with_csrf(), body).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(reply.body, json!({ "error": "unauthorized" }), "{uri}");
    }
    // While the same cookie does work where it should.
    let ok = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(ok.status, StatusCode::OK);
}

#[tokio::test]
async fn a_signature_counter_that_goes_back_is_refused() {
    let h = harness();
    let mut key = counting_key();
    let registered = h.bootstrap(&mut key).await;
    assert_eq!(registered.status, StatusCode::OK, "{}", registered.body);
    h.sign_in(&mut key).await;
    h.sign_in(&mut key).await;
    let id = registered.body["id"].as_str().unwrap().to_string();
    assert_eq!(
        h.store.admin_credential(&id).unwrap().unwrap().sign_count,
        2
    );

    // A copy of the key, taken before those two sign-ins, answers with a
    // counter the server has seen already.
    key.authenticator_mut()
        .store_mut()
        .as_mut()
        .unwrap()
        .counter = Some(0);
    let reply = h.try_sign_in(&mut key).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
    assert!(
        reply.body["error"]
            .as_str()
            .unwrap()
            .contains("signature counter did not move forward"),
        "{}",
        reply.body
    );
    assert!(reply.headers.get(header::SET_COOKIE).is_none());
    assert_eq!(
        h.store.admin_credential(&id).unwrap().unwrap().sign_count,
        2
    );

    // The key itself, carrying on from where the server last saw it, is
    // still fine.
    key.authenticator_mut()
        .store_mut()
        .as_mut()
        .unwrap()
        .counter = Some(2);
    h.sign_in(&mut key).await;
}

#[tokio::test]
async fn approving_by_code_with_another_fingerprint_is_409() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let session = h.sign_in(&mut phone).await;
    let (code, _) = enrolling(&h, 3, "laptop").await;
    let (_, other) = enrolling(&h, 4, "desktop").await;
    let reply = h
        .call(
            "POST",
            "/v1/devices/approve",
            session.with_csrf(),
            Some(json!({ "user_code": code, "fingerprint": other })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(
        reply.body["error"],
        "that code's key does not have the fingerprint given; nothing was approved"
    );
}

#[tokio::test]
async fn a_signed_in_owner_adds_and_removes_passkeys_but_never_the_last() {
    let h = harness();
    let mut phone = phone();
    let first = h.bootstrap(&mut phone).await;
    let first_id = first.body["id"].as_str().unwrap().to_string();
    let session = h.sign_in(&mut phone).await;

    // The token cannot manage passkeys: that would let a leaked one add its
    // own, which would outlast rotating it.
    for (method, uri) in [
        ("GET", "/admin/passkeys"),
        ("POST", "/admin/passkeys/register"),
        ("POST", "/admin/logout"),
    ] {
        let reply = h.call(method, uri, As::Token, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{uri}");
    }

    let only = h
        .call(
            "POST",
            &format!("/admin/passkeys/{first_id}/remove"),
            session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(only.status, StatusCode::CONFLICT, "{}", only.body);

    // A second passkey, on another phone, for the same owner.
    let started = h
        .call(
            "POST",
            "/admin/passkeys/register",
            session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    assert_eq!(
        started.body["options"]["publicKey"]["excludeCredentials"][0]["id"],
        first_id.as_str(),
        "the phone that has one is not asked to replace it"
    );
    assert_eq!(
        started.body["options"]["publicKey"]["authenticatorSelection"]["residentKey"],
        "required"
    );
    let mut tablet = phone_two();
    let credential = create(&mut tablet, &started.body["options"]).await;
    let added = h
        .call(
            "POST",
            "/admin/passkeys/register/finish",
            session.with_csrf(),
            Some(json!({
                "ceremony_id": started.body["ceremony_id"],
                "name": "iPad",
                "credential": credential,
            })),
        )
        .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let second_id = added.body["id"].as_str().unwrap().to_string();

    // It signs in, as the same owner.
    let tablet_session = h.sign_in(&mut tablet).await;
    let listed = h
        .call(
            "GET",
            "/admin/passkeys",
            As::Session(&tablet_session, None),
            None,
        )
        .await;
    let names: Vec<&str> = listed.body["passkeys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["iPhone", "iPad"]);

    // Removing the first ends the sessions it signed in, not the others.
    let removed = h
        .call(
            "POST",
            &format!("/admin/passkeys/{first_id}/remove"),
            tablet_session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let gone = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(gone.status, StatusCode::UNAUTHORIZED);
    let still = h
        .call(
            "GET",
            "/v1/devices",
            As::Session(&tablet_session, None),
            None,
        )
        .await;
    assert_eq!(still.status, StatusCode::OK);
    // And the removed passkey signs nobody in.
    let refused = h.try_sign_in(&mut phone).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{}", refused.body);

    // The last one stays.
    let last = h
        .call(
            "POST",
            &format!("/admin/passkeys/{second_id}/remove"),
            tablet_session.with_csrf(),
            None,
        )
        .await;
    assert_eq!(last.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_ceremony_is_answered_once_and_only_where_it_was_started() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let started = h.call("POST", "/admin/login/start", As::Nobody, None).await;
    let credential = get(&mut phone, &started.body["options"]).await;
    let finish = json!({ "ceremony_id": started.body["ceremony_id"], "credential": credential });
    let first = h
        .call(
            "POST",
            "/admin/login/finish",
            As::Nobody,
            Some(finish.clone()),
        )
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let replayed = h
        .call("POST", "/admin/login/finish", As::Nobody, Some(finish))
        .await;
    assert_eq!(
        replayed.status,
        StatusCode::BAD_REQUEST,
        "{}",
        replayed.body
    );

    // An answer made for another origin, such as a phishing copy of the
    // page, does not verify here.
    let started = h.call("POST", "/admin/login/start", As::Nobody, None).await;
    let options: CredentialRequestOptions =
        serde_json::from_value(started.body["options"].clone()).unwrap();
    let phished = phone
        .authenticate(
            &"https://recall.example.com.evil.test".parse().unwrap(),
            options,
            DefaultClientData,
        )
        .await;
    // The client itself refuses an origin the RP id is not a suffix of; a
    // browser does the same. That refusal is the phishing resistance.
    assert!(phished.is_err());

    // A sign-in ceremony cannot finish a registration.
    let started = h.call("POST", "/admin/login/start", As::Nobody, None).await;
    let reply = h
        .call(
            "POST",
            "/admin/bootstrap/register/finish",
            As::Token,
            Some(json!({ "ceremony_id": started.body["ceremony_id"], "credential": {} })),
        )
        .await;
    assert_ne!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn without_a_public_url_passkeys_are_off_and_say_why_and_the_token_still_works() {
    let h = harness_with("");
    let status = h.call("GET", "/admin/session", As::Nobody, None).await;
    assert_eq!(status.body["passkeys"]["enabled"], false);
    assert!(status.body["passkeys"]["reason"]
        .as_str()
        .unwrap()
        .contains("RECALL_PUBLIC_URL is not set"));
    let start = h.call("POST", "/admin/login/start", As::Nobody, None).await;
    assert_eq!(start.status, StatusCode::SERVICE_UNAVAILABLE);
    let bootstrap = h
        .call("POST", "/admin/bootstrap/register", As::Token, None)
        .await;
    assert_eq!(bootstrap.status, StatusCode::SERVICE_UNAVAILABLE);
    for uri in ["/admin/stats", "/v1/devices", "/v1/authkeys"] {
        let reply = h.call("GET", uri, As::Token, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn the_admin_page_is_served_with_its_hardening_headers() {
    let h = harness();
    let reply = h
        .server
        .router()
        .oneshot(Request::get("/admin").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let headers = reply.headers();
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(csp.contains("script-src 'sha256-"), "{csp}");
    assert!(
        !csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"),
        "{csp}"
    );
    assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
}

// ---------------------------------------------------------------------------
// From the security review. Each test names the finding it fixes, or the
// mutation of the code it was written to catch: every one of those mutations
// survived the tests above.
// ---------------------------------------------------------------------------

/// A request `key` signs as device `id`, as the client sends one, with
/// `session`'s cookie and CSRF header beside it when there is one.
fn signed(
    key: &SigningKey,
    id: &str,
    method: &str,
    path: &str,
    nonce: &str,
    session: Option<&Session>,
) -> Request<Body> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let target = Target {
        method,
        authority: "recall.test",
        path,
        query: None,
    };
    let headers = sign_request(key, id, &target, "1", b"", now, nonce).unwrap();
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "recall.test")
        .header(recall_wire::PROTOCOL_HEADER, "1")
        .header(CONTENT_DIGEST_HEADER, headers.content_digest)
        .header(SIGNATURE_INPUT_HEADER, headers.signature_input)
        .header(SIGNATURE_HEADER, headers.signature);
    if let Some(session) = session {
        req = req
            .header("cookie", format!("{COOKIE}={}", session.cookie))
            .header("x-recall-csrf", &session.csrf);
    }
    req.body(Body::empty()).unwrap()
}

/// Finding 1: starting a sign-in keeps nothing on the server, so strangers
/// starting thousands, from as many /64s as they have, cannot keep the
/// owner from signing in.
#[tokio::test]
async fn strangers_starting_sign_ins_cannot_keep_the_owner_out() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    // 512 /64s of one /48, ten unfinished sign-ins each: more than the 4096
    // in all, and the eight an address, that a table of them once held.
    for n in 0..512u32 {
        let ip = format!("2001:db8:1:{n:x}::1");
        for _ in 0..10 {
            assert_eq!(h.start_from(&ip).await.status, StatusCode::OK, "{ip}");
        }
    }
    // The owner, from an address of their own and from one of those.
    for ip in ["198.51.100.7", "2001:db8:1:7::2"] {
        let started = h.start_from(ip).await;
        assert_eq!(started.status, StatusCode::OK, "{ip}: {}", started.body);
        let credential = get(&mut phone, &started.body["options"]).await;
        let finished = h.finish_sign_in(&started, credential).await;
        assert_eq!(finished.status, StatusCode::OK, "{ip}: {}", finished.body);
    }
}

/// Finding 4: a ceremony's start and finish take JSON and nothing else, so a
/// page on another site cannot send one without the browser asking this
/// server first.
#[tokio::test]
async fn a_ceremony_request_that_is_not_json_is_415() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let session = h.sign_in(&mut phone).await;
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
    ] {
        for (uri, who) in [
            ("/admin/login/start", As::Nobody),
            ("/admin/login/finish", As::Nobody),
            ("/admin/bootstrap/register", As::Token),
            ("/admin/bootstrap/register/finish", As::Token),
            ("/admin/passkeys/register", session.with_csrf()),
            ("/admin/passkeys/register/finish", session.with_csrf()),
        ] {
            let reply = h
                .call_typed("POST", uri, who, content_type, Some(json!({})))
                .await;
            assert_eq!(
                reply.status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{uri} {content_type:?}: {}",
                reply.body
            );
            assert_eq!(
                reply.body["error"], "this needs Content-Type: application/json",
                "{uri}"
            );
        }
    }
    // Who is asking is still settled first: no token is the usual 401.
    let reply = h
        .call_typed(
            "POST",
            "/admin/bootstrap/register",
            As::Nobody,
            Some("text/plain"),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // With JSON, signing in works as ever.
    h.sign_in(&mut phone).await;
}

/// Mutation M2: adding a passkey is finished only by the session that
/// started it.
#[tokio::test]
async fn only_the_session_that_started_adding_a_passkey_finishes_it() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let mine = h.sign_in(&mut phone).await;
    let other = h.sign_in(&mut phone).await;
    let started = h
        .call("POST", "/admin/passkeys/register", mine.with_csrf(), None)
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let mut tablet = phone_two();
    let credential = create(&mut tablet, &started.body["options"]).await;
    let finish = json!({ "ceremony_id": started.body["ceremony_id"], "credential": credential });

    let theirs = h
        .call(
            "POST",
            "/admin/passkeys/register/finish",
            other.with_csrf(),
            Some(finish.clone()),
        )
        .await;
    assert_eq!(theirs.status, StatusCode::BAD_REQUEST, "{}", theirs.body);
    assert_eq!(
        theirs.body["error"],
        "that ceremony is not one this route finishes; start again"
    );
    assert_eq!(h.store.admin_credentials().unwrap().len(), 1);

    // Refusing another session did not use the ceremony up.
    let ours = h
        .call(
            "POST",
            "/admin/passkeys/register/finish",
            mine.with_csrf(),
            Some(finish),
        )
        .await;
    assert_eq!(ours.status, StatusCode::OK, "{}", ours.body);
}

/// Mutation M3: a passkey is the owner's only if it answers for the owner's
/// user handle as well as with a registered credential id.
#[tokio::test]
async fn a_passkey_answering_for_another_user_is_refused() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    // The same credential, now saying it is someone else's. The user
    // handle is not signed, so the signature still verifies.
    phone
        .authenticator_mut()
        .store_mut()
        .as_mut()
        .unwrap()
        .user_handle = Some(vec![7u8; 16].into());
    let reply = h.try_sign_in(&mut phone).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
    assert_eq!(
        reply.body["error"],
        "unauthorized: that passkey is not registered here"
    );
    assert!(reply.headers.get(header::SET_COOKIE).is_none());
}

/// Mutation M5: an answer that comes after its ceremony's five minutes, by
/// the server's clock, is refused.
#[tokio::test]
async fn an_answer_after_five_minutes_is_refused() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let started = h.call("POST", "/admin/login/start", As::Nobody, None).await;
    let credential = get(&mut phone, &started.body["options"]).await;

    h.server.set_clock_offset(5 * 60);
    let late = h.finish_sign_in(&started, credential.clone()).await;
    assert_eq!(late.status, StatusCode::BAD_REQUEST, "{}", late.body);
    assert_eq!(
        late.body["error"],
        "this ceremony has expired or was already used; start again"
    );
    assert!(late.headers.get(header::SET_COOKIE).is_none());

    // A second sooner, the same answer would have done.
    h.server.set_clock_offset(5 * 60 - 1);
    let in_time = h.finish_sign_in(&started, credential).await;
    assert_eq!(in_time.status, StatusCode::OK, "{}", in_time.body);
}

/// Mutations M6 and M7: an answer made at a subdomain of the configured
/// origin, or at another port of it, is refused, though a browser there
/// would make one.
#[tokio::test]
async fn an_answer_made_at_a_subdomain_or_another_port_is_refused() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let elsewhere = [
        "https://x.recall.example.com",
        "https://recall.example.com:8443",
    ];
    for at in elsewhere {
        let started = h.call("POST", "/admin/login/start", As::Nobody, None).await;
        let credential = get_at(&mut phone, at, &started.body["options"])
            .await
            .unwrap_or_else(|e| panic!("the client refused {at}: {e}"));
        let reply = h.finish_sign_in(&started, credential).await;
        assert_eq!(
            reply.status,
            StatusCode::UNAUTHORIZED,
            "{at}: {}",
            reply.body
        );
        assert!(reply.headers.get(header::SET_COOKIE).is_none(), "{at}");
    }
    let session = h.sign_in(&mut phone).await;
    for at in elsewhere {
        let started = h
            .call(
                "POST",
                "/admin/passkeys/register",
                session.with_csrf(),
                None,
            )
            .await;
        let mut other = phone_two();
        let credential = create_at(&mut other, at, &started.body["options"])
            .await
            .unwrap_or_else(|e| panic!("the client refused {at}: {e}"));
        let reply = h
            .call(
                "POST",
                "/admin/passkeys/register/finish",
                session.with_csrf(),
                Some(
                    json!({ "ceremony_id": started.body["ceremony_id"], "credential": credential }),
                ),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{at}: {}",
            reply.body
        );
    }
    assert_eq!(h.store.admin_credentials().unwrap().len(), 1);
}

/// Mutation M8: what the loser of two sign-ins racing sees. Both verified
/// against the passkey as it was, at counter 2; the winner has since
/// recorded 3. The loser's answer, 3 as well, passes webauthn-rs's check
/// against the copy it read, and only the store's conditional update can
/// refuse it.
#[tokio::test]
async fn a_sign_in_that_lost_the_counter_race_is_refused() {
    let h = harness();
    let mut key = counting_key();
    let registered = h.bootstrap(&mut key).await;
    let id = registered.body["id"].as_str().unwrap().to_string();
    h.sign_in(&mut key).await;
    h.sign_in(&mut key).await;
    let stored = h.store.admin_credential(&id).unwrap().unwrap();
    assert_eq!(stored.sign_count, 2);

    // The winner's write, as far as the loser can tell: the counter the
    // store compares moves on, the passkey the loser read does not.
    assert!(h
        .store
        .record_admin_sign_in(&id, 3, &stored.passkey, "2026-09-23T10:00:00.000Z")
        .unwrap());
    let reply = h.try_sign_in(&mut key).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
    assert!(
        reply.body["error"]
            .as_str()
            .unwrap()
            .contains("signature counter did not move forward"),
        "{}",
        reply.body
    );
    assert!(reply.headers.get(header::SET_COOKIE).is_none());
    assert_eq!(
        h.store.admin_credential(&id).unwrap().unwrap().sign_count,
        3
    );
}

/// Mutation M8 again, as it happens: two copies of a counting key answer two
/// sign-ins with the same counter, and both answers arrive at once. At most
/// one may get a session. Whether a round interleaves is up to the
/// scheduler, so the test above is the one that always reaches the check;
/// this one says the same of real concurrent requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_copies_of_a_key_signing_in_at_once_never_both_succeed() {
    let h = harness();
    let mut real = counting_key();
    assert_eq!(h.bootstrap(&mut real).await.status, StatusCode::OK);
    let finish = |started: &Reply, credential: Value| {
        Request::post("/admin/login/finish")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "ceremony_id": started.body["ceremony_id"],
                    "credential": credential,
                }))
                .unwrap(),
            ))
            .unwrap()
    };
    for round in 0..20 {
        let mut copy = counting_key();
        *copy.authenticator_mut().store_mut() = real.authenticator_mut().store_mut().clone();
        let s1 = h.call("POST", "/admin/login/start", As::Nobody, None).await;
        let s2 = h.call("POST", "/admin/login/start", As::Nobody, None).await;
        let c1 = get(&mut real, &s1.body["options"]).await;
        let c2 = get(&mut copy, &s2.body["options"]).await;
        let (r1, r2) = (h.server.router(), h.server.router());
        let (q1, q2) = (finish(&s1, c1), finish(&s2, c2));
        let a = tokio::spawn(async move { r1.oneshot(q1).await.unwrap().status() });
        let b = tokio::spawn(async move { r2.oneshot(q2).await.unwrap().status() });
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        assert!(
            !(a == StatusCode::OK && b == StatusCode::OK),
            "round {round}: both copies signed in"
        );
    }
}

/// Mutation M10: the routes only a session may use are rate limited like
/// every other, before the session is looked at.
#[tokio::test]
async fn the_session_only_routes_are_rate_limited() {
    let h = harness_limited(6);
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let session = h.sign_in(&mut phone).await;
    // Four requests so far, from the one address every request here has.
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let reply = h
            .call("GET", "/admin/passkeys", As::Session(&session, None), None)
            .await;
        statuses.push(reply.status);
    }
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );
    let out = h
        .call("POST", "/admin/logout", session.with_csrf(), None)
        .await;
    assert_eq!(out.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(out.headers.get(header::SET_COOKIE).is_none());
}

/// Mutation M11, and finding 6a: on the device routes, a request that
/// carries an `Authorization` header or a signature is judged by that
/// alone. A live session cookie beside it changes nothing.
#[tokio::test]
async fn a_token_or_signature_beside_a_session_cookie_is_judged_alone() {
    let h = harness();
    h.server.backdate_start(600);
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    let session = h.sign_in(&mut phone).await;
    let key = SigningKey::from_bytes(&[9; 32]);
    let id = h.device(&key, "syncer", "sync").await;

    // A sync device's signature: the device's 403, not the cookie's 200.
    let reply = h
        .raw(signed(
            &key,
            &id,
            "GET",
            "/v1/devices",
            "n1",
            Some(&session),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
    // A signature by another key: 401.
    let stranger = SigningKey::from_bytes(&[10; 32]);
    let reply = h
        .raw(signed(
            &stranger,
            &id,
            "GET",
            "/v1/devices",
            "n2",
            Some(&session),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{}", reply.body);
    // A wrong token, or another scheme: 401.
    for authorization in ["Bearer wrong", "Basic d3Jvbmc=", "Bearer "] {
        let reply = h
            .raw(
                Request::get("/v1/devices")
                    .header("authorization", authorization)
                    .header("cookie", format!("{COOKIE}={}", session.cookie))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{authorization}");
        assert_eq!(reply.body, json!({ "error": "unauthorized" }));
    }
    // The cookie alone still does.
    let reply = h
        .call("GET", "/v1/devices", As::Session(&session, None), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

/// Mutation M15: a session cookie that cannot be one of ours is an ended
/// session, refused as one, not taken for no cookie at all.
#[tokio::test]
async fn a_malformed_session_cookie_is_refused_not_ignored() {
    let h = harness();
    let mut phone = phone();
    h.bootstrap(&mut phone).await;
    for value in [
        "short".to_string(),
        "A".repeat(44),
        "!".repeat(43),
        String::new(),
    ] {
        let malformed = Session {
            cookie: value.clone(),
            csrf: String::new(),
        };
        for uri in ["/v1/devices", "/admin/passkeys"] {
            let reply = h
                .call("GET", uri, As::Session(&malformed, None), None)
                .await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{value:?} {uri}");
            assert_eq!(
                reply.body["error"], "unauthorized: the admin session has ended; sign in again",
                "{value:?} {uri}"
            );
        }
        let status = h
            .call("GET", "/admin/session", As::Session(&malformed, None), None)
            .await;
        assert_eq!(status.body["session"], Value::Null);
        assert!(
            status.headers[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("Max-Age=0"),
            "{value:?}: the page is told to forget it"
        );
    }
}

/// Finding 6c: the store's copy of the signature counter starts where the
/// registration left it, not at zero.
#[tokio::test]
async fn the_stored_counter_starts_at_the_registration_counter() {
    let h = harness();
    let started = h
        .call("POST", "/admin/bootstrap/register", As::Token, None)
        .await;
    let mut key = counting_key();
    let mut credential = create(&mut key, &started.body["options"]).await;
    // As if the key had signed seven times elsewhere. The counter is in the
    // authenticator data, after the RP id's hash and the flags, and with
    // "none" attestation nothing signs it at registration. passkey-client
    // writes its bytes as an array of numbers, which webauthn-rs reads as
    // readily as base64url.
    let field = &mut credential["response"]["attestationObject"];
    let mut attestation: Vec<u8> = serde_json::from_value(field.clone()).unwrap();
    let rp_id_hash = Sha256::digest(b"recall.example.com");
    let at = attestation
        .windows(32)
        .position(|w| w == rp_id_hash.as_slice())
        .expect("the authenticator data");
    attestation[at + 33..at + 37].copy_from_slice(&7u32.to_be_bytes());
    *field = json!(attestation);
    let done = h
        .call(
            "POST",
            "/admin/bootstrap/register/finish",
            As::Token,
            Some(json!({ "ceremony_id": started.body["ceremony_id"], "credential": credential })),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    let id = done.body["id"].as_str().unwrap();
    assert_eq!(h.store.admin_credential(id).unwrap().unwrap().sign_count, 7);
}
