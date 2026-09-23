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
use recall_wire::signature::{encode_public_key, fingerprint, SigningKey};
use recall_wire::{Device, DeviceList, EnrollKeyCreated, EnrollPending, PendingEnrollment};
use serde_json::{json, Value};
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
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("recall.db")).unwrap());
    let cfg = Config {
        token: TOKEN.to_string(),
        merge_enabled: false,
        rate_limit_max: 10_000,
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
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
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
        let resp = self
            .server
            .router()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
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

fn origin() -> Url {
    Url::parse(ORIGIN).unwrap()
}

/// `navigator.credentials.create()`, as a browser at [`ORIGIN`] runs it.
async fn create(phone: &mut Phone, options: &Value) -> Value {
    let options: CredentialCreationOptions = serde_json::from_value(options.clone()).unwrap();
    let created = phone
        .register(&origin(), options, DefaultClientData)
        .await
        .expect("the authenticator registers");
    serde_json::to_value(created).unwrap()
}

/// `navigator.credentials.get()`.
async fn get(phone: &mut Phone, options: &Value) -> Value {
    let options: CredentialRequestOptions = serde_json::from_value(options.clone()).unwrap();
    let asserted = phone
        .authenticate(&origin(), options, DefaultClientData)
        .await
        .expect("the authenticator signs");
    serde_json::to_value(asserted).unwrap()
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
    let key = SigningKey::from_bytes(&[seed; 32]);
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

    // An enrolment key, shown once, and revoked.
    let key = h
        .call(
            "POST",
            "/v1/enroll-keys",
            session.with_csrf(),
            Some(json!({ "tag": "cloud", "expires_in_days": 30 })),
        )
        .await;
    assert_eq!(key.status, StatusCode::OK, "{}", key.body);
    let key: EnrollKeyCreated = serde_json::from_value(key.body).unwrap();
    let revoked = h
        .call(
            "POST",
            &format!("/v1/enroll-keys/{}/revoke", key.id),
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
            ("POST", "/v1/enroll-keys"),
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
            &Url::parse("https://recall.example.com.evil.test").unwrap(),
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
    for uri in ["/admin/stats", "/v1/devices", "/v1/enroll-keys"] {
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
