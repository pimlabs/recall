//! Talks to the Recall server.

use std::time::Duration;

use recall_wire::devices::{self as wire_devices, EnrollPollRequest, EnrollRequest};
use recall_wire::signature::{self, SignatureError, Target};
use recall_wire::{
    discovery, ApproveRequest, Authkey, AuthkeyCreated, AuthkeyList, AuthkeyRequest,
    AuthkeyRevokeRequest, Device, DeviceIdentity, DeviceList, Discovery, EnrollApproved,
    EnrollPending, EnrollPollResponse, ErrorResponse, Health, PendingEnrollment, PushRequest,
    PushResponse, SyncResponse, ValidationError, DISCOVERY_PATH, PROTOCOL, PROTOCOL_HEADER,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::device::Signer;

/// A generous but bounded timeout. The server may be running a semantic
/// merge through the `claude` CLI, which takes several seconds — but these
/// calls happen inside a user's session, so they cannot hang forever.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Why a call to the server did not succeed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server answered, and said no. Carries enough to tell the user
    /// *why* — a 401 from a stale token reads very differently from a 500.
    #[error("server returned {code}: {body}")]
    Status {
        /// The HTTP status.
        code: u16,
        /// The response body, trimmed.
        body: String,
    },
    /// Caught before sending, by the same rules the server would apply.
    #[error("refusing to send an invalid request: {0}")]
    Invalid(#[from] ValidationError),
    /// The server could not be reached, or timed out.
    #[error("could not reach the server: {0}")]
    Transport(#[from] reqwest::Error),
    /// The server answered with something this client cannot parse.
    #[error("server sent a response this client can't parse: {0}")]
    Decode(#[from] serde_json::Error),
    /// A request could not be signed. Only a bug gets here: what is signed
    /// is built by this client, not read from anywhere.
    #[error("could not sign the request: {0}")]
    Sign(#[from] SignatureError),
    /// No randomness for a signature's nonce.
    #[error("could not sign the request: no randomness for a nonce: {0}")]
    Nonce(String),
    /// The server answered with a redirect, which is never followed: see
    /// [`Client::new`].
    #[error("the server redirected to {location}; update RECALL_URL if that is where it is now")]
    Redirect {
        /// The HTTP status, a 3xx.
        code: u16,
        /// Where it pointed, as sent.
        location: String,
    },
}

/// What the server says when a signature names a device it has no record
/// of: one it removed after it sat idle (an ephemeral device, swept), or
/// one it never had.
const UNKNOWN_DEVICE: &str = "unauthorized: unknown device";

/// What the server says when a signature names a device the owner revoked.
const REVOKED_DEVICE: &str = "unauthorized: this device has been revoked";

impl Error {
    /// Whether the server no longer accepts the device this client signs
    /// as, for either reason: [`Error::device_unknown`] or
    /// [`Error::device_revoked`]. Either way the key this machine holds will
    /// never work there again. What may be done about it differs, which is
    /// why the two are also asked apart.
    pub fn device_gone(&self) -> bool {
        self.device_unknown() || self.device_revoked()
    }

    /// Whether the server has no record of the device this client signs
    /// as: an ephemeral one removed after sitting idle, which a cloud
    /// session holding an authkey may replace by enrolling again.
    pub fn device_unknown(&self) -> bool {
        self.refused_as(UNKNOWN_DEVICE)
    }

    /// Whether the owner revoked the device this client signs as. Nothing on
    /// this machine may undo that by itself: a revocation that a hook could
    /// answer by enrolling again would cut nothing off.
    pub fn device_revoked(&self) -> bool {
        self.refused_as(REVOKED_DEVICE)
    }

    /// A 401 whose reason is exactly `reason`. Every other 401, a signature
    /// that did not verify or a clock too far off, says nothing about
    /// whether the device still exists.
    fn refused_as(&self, reason: &str) -> bool {
        matches!(self, Error::Status { code: 401, .. }) && self.reason() == reason
    }

    /// Whether the server refused a signature for being dated before it
    /// started or in the few seconds after, which it does to every
    /// signature then, and which signing again a little later mends.
    pub fn signed_too_soon(&self) -> bool {
        matches!(self, Error::Status { code: 401, .. })
            && self
                .reason()
                .starts_with("unauthorized: signature created before this server started")
    }

    /// The server's own words, when it answered with an error body, and
    /// this error's otherwise: what a person reads.
    pub fn reason(&self) -> String {
        match self {
            Error::Status { body, .. } => serde_json::from_str::<ErrorResponse>(body)
                .map(|e| e.error)
                .unwrap_or_else(|_| body.clone()),
            other => other.to_string(),
        }
    }
}

/// What `POST /v1/devices/enroll` answered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
pub enum Enrolled {
    /// Approved at once, with an authkey.
    Approved(EnrollApproved),
    /// Waiting for someone to approve the code.
    Pending(EnrollPending),
}

/// What one poll of an enrolment learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    /// Approved: sign as this device from now on.
    Approved(EnrollPollResponse),
    /// Nobody has decided yet; wait the interval and ask again.
    Pending,
    /// Asked too soon: wait five seconds longer, from now on.
    SlowDown,
    /// Fifteen minutes passed with no approval.
    Expired,
    /// Denied, or approved and then revoked.
    Denied,
    /// The server knows no such enrolment.
    Unknown,
}

/// A Recall API client.
#[derive(Clone)]
pub struct Client {
    base_url: String,
    token: String,
    /// Set once this machine is an enrolled device: every request is then
    /// signed with its key, and the token is not sent.
    signer: Option<Signer>,
    http: reqwest::Client,
}

/// Never prints the token: a client ends up in a test failure or a log line
/// as easily as anything else, and the token is the one secret it holds.
/// The signer prints its device id and nothing of its key.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("token", &crate::config::redacted(&self.token))
            .field("signer", &self.signer)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Builds a client for one server. Trailing slashes on `base_url` are
    /// trimmed, so a URL pasted with one does not produce `//sync`.
    ///
    /// Every request says which protocol it speaks and which build sent it,
    /// so a server can refuse a protocol it does not speak by name and the
    /// operator can see which versions are still out there.
    ///
    /// A redirect is never followed, and no `Referer` is sent. Following
    /// one would send the request again to wherever `Location` points,
    /// another host included: a `307` or `308` repeats the body, which holds
    /// an authkey when enrolling and a memory file when pushing, and the
    /// signature headers go with it. reqwest drops `Authorization` on a
    /// redirect to another host, but nothing else. Recall talks to the one
    /// server it was given, so a redirect means that URL is wrong, and the
    /// person who set it is told so ([`Error::Redirect`]) rather than having
    /// it silently corrected by whoever answered.
    pub fn new(base_url: &str, token: &str) -> Result<Self, Error> {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(PROTOCOL_HEADER),
            HeaderValue::from(PROTOCOL),
        );
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            signer: None,
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(discovery::user_agent())
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .referer(false)
                .build()?,
        })
    }

    /// The same client, signing every request as an enrolled device
    /// instead of sending the token.
    ///
    /// Never both: a request carrying the right bearer token is the
    /// operator's whatever else it carries, so sending the token alongside
    /// would make every signed request the operator's and the device's name
    /// meaningless.
    pub fn with_signer(mut self, signer: Signer) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Whether requests are signed rather than carrying the token.
    pub fn signs(&self) -> bool {
        self.signer.is_some()
    }

    /// The normalized server URL this client will call.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends one memory file, or one delete.
    ///
    /// Validated client-side first, so a request the server would reject
    /// never leaves this machine and the user sees the real reason rather
    /// than a 400.
    pub async fn push(&self, req: &PushRequest) -> Result<PushResponse, Error> {
        req.validate()?;
        let body = serde_json::to_vec(req)?;
        let request = self
            .http
            .post(format!("{}/sync", self.base_url))
            .header("Content-Type", "application/json")
            .body(body);
        self.send(request).await
    }

    /// Fetches every file the server holds for a project, tombstones
    /// included — the caller needs to see those to remove local copies.
    pub async fn pull(&self, project_key: &str) -> Result<SyncResponse, Error> {
        let request = self
            .http
            .get(format!("{}/sync", self.base_url))
            .query(&[("project_key", project_key)]);
        self.send(request).await
    }

    /// Reads the health endpoint. It is unauthenticated, but the token is
    /// sent anyway — harmless, and it keeps one code path.
    pub async fn health(&self) -> Result<Health, Error> {
        let request = self.http.get(format!("{}/health", self.base_url));
        self.send(request).await
    }

    /// Reads the discovery document: what the server is and what it
    /// speaks. [`None`] from a server older than the document, which
    /// answers 404 and speaks protocol 1.
    pub async fn discover(&self) -> Result<Option<Discovery>, Error> {
        let request = self.http.get(format!("{}{DISCOVERY_PATH}", self.base_url));
        match self.send(request).await {
            Ok(doc) => Ok(Some(doc)),
            Err(Error::Status { code: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Asks whether the token is accepted, without reading or writing any
    /// memory.
    ///
    /// `GET /health` cannot answer this — it is unauthenticated, so a wrong
    /// token passes it — and a pull would need a project key and would
    /// fetch a project's notes to learn one bit. `/admin/stats` is
    /// authenticated and read-only, and the body is not kept.
    pub async fn check_token(&self) -> Result<(), Error> {
        let request = self.http.get(format!("{}/admin/stats", self.base_url));
        self.send::<serde_json::Value>(request).await.map(|_| ())
    }

    /// Starts enrolling this machine: `POST /v1/devices/enroll`.
    /// Unauthenticated; with an authkey the answer is an approved
    /// device, and without one a code for someone to approve.
    pub async fn enroll(&self, req: &EnrollRequest) -> Result<Enrolled, Error> {
        self.send(self.post_json(wire_devices::ENROLL_PATH, req)?)
            .await
    }

    /// Asks once whether an enrolment was approved. RFC 8628's answers
    /// arrive as `400`s, and are read into [`Poll`] rather than returned
    /// as errors, since all but one of them mean "keep going" or "stop".
    pub async fn poll(&self, enrollment_id: &str) -> Result<Poll, Error> {
        let req = EnrollPollRequest {
            enrollment_id: enrollment_id.to_string(),
        };
        match self
            .send::<EnrollPollResponse>(self.post_json(wire_devices::ENROLL_POLL_PATH, &req)?)
            .await
        {
            Ok(approved) => Ok(Poll::Approved(approved)),
            Err(e @ Error::Status { code: 400, .. }) => match e.reason().as_str() {
                wire_devices::AUTHORIZATION_PENDING => Ok(Poll::Pending),
                wire_devices::SLOW_DOWN => Ok(Poll::SlowDown),
                wire_devices::EXPIRED_TOKEN => Ok(Poll::Expired),
                wire_devices::ACCESS_DENIED => Ok(Poll::Denied),
                wire_devices::INVALID_GRANT => Ok(Poll::Unknown),
                _ => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    /// Which device signed this request: how a machine checks it is still
    /// enrolled.
    pub async fn me(&self) -> Result<DeviceIdentity, Error> {
        let request = self.http.get(format!(
            "{}{}",
            self.base_url,
            wire_devices::DEVICES_ME_PATH
        ));
        self.send(request).await
    }

    /// What approving `user_code` would approve. Admin.
    pub async fn pending(&self, user_code: &str) -> Result<PendingEnrollment, Error> {
        let request = self.http.get(format!(
            "{}{}",
            self.base_url,
            wire_devices::pending_path(user_code)
        ));
        self.send(request).await
    }

    /// Approves a code. Admin.
    pub async fn approve(&self, req: &ApproveRequest) -> Result<Device, Error> {
        self.send(self.post_json(wire_devices::APPROVE_PATH, req)?)
            .await
    }

    /// Every device, revoked ones included. Admin.
    pub async fn devices(&self) -> Result<DeviceList, Error> {
        let request = self
            .http
            .get(format!("{}{}", self.base_url, wire_devices::DEVICES_PATH));
        self.send(request).await
    }

    /// Revokes a device by id. Admin.
    pub async fn revoke_device(&self, id: &str) -> Result<Device, Error> {
        let path = wire_devices::revoke_device_path(id);
        self.send(self.post_json(&path, &serde_json::json!({}))?)
            .await
    }

    /// Makes an authkey. Admin; the key is in the answer and nowhere
    /// else.
    pub async fn create_authkey(&self, req: &AuthkeyRequest) -> Result<AuthkeyCreated, Error> {
        self.send(self.post_json(wire_devices::AUTHKEYS_PATH, req)?)
            .await
    }

    /// Every authkey, without the keys themselves. Admin.
    pub async fn authkeys(&self) -> Result<AuthkeyList, Error> {
        let request = self
            .http
            .get(format!("{}{}", self.base_url, wire_devices::AUTHKEYS_PATH));
        self.send(request).await
    }

    /// Stops an authkey enrolling anything more, and with
    /// `revoke_devices` revokes what it already enrolled. Admin.
    pub async fn revoke_authkey(&self, id: &str, revoke_devices: bool) -> Result<Authkey, Error> {
        let path = wire_devices::revoke_authkey_path(id);
        self.send(self.post_json(&path, &AuthkeyRevokeRequest { revoke_devices })?)
            .await
    }

    /// A JSON `POST` to `path`, the body serialized here so it is exactly
    /// the bytes that are signed.
    fn post_json<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<reqwest::RequestBuilder, Error> {
        Ok(self
            .http
            .post(format!("{}{path}", self.base_url))
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(body)?))
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, Error> {
        let Some(signer) = &self.signer else {
            // No token, no header: an empty `Bearer` says nothing a missing
            // one does not, and a client made to carry no credential should
            // not look as if it tried one.
            let request = match self.token.as_str() {
                "" => request,
                token => request.bearer_auth(token),
            };
            return read(self.http.execute(request.build()?).await?).await;
        };
        let mut next = request.build()?;
        let mut retries = 0;
        loop {
            // Kept unsigned for another attempt. Every body here is bytes,
            // which clone; one that did not would be sent once, unretried.
            let spare = next.try_clone();
            let mut request = next;
            sign(signer, &mut request)?;
            match read(self.http.execute(request).await?).await {
                Err(e) if e.signed_too_soon() && retries < RESTART_RETRIES => match spare {
                    Some(spare) => {
                        next = spare;
                        retries += 1;
                        tokio::time::sleep(RESTART_WAIT).await;
                    }
                    None => return Err(e),
                },
                other => return other,
            }
        }
    }
}

/// How often a request the server refused for being signed too soon after
/// it started is signed again, and how long apart. Together they cover the
/// server's whole refusal: it refuses signatures dated up to
/// [`signature::MAX_AHEAD_SECONDS`] after its start, and three waits of two
/// and a half seconds date the last attempt past that from any moment it
/// could have started, with two seconds to spare for this machine's clock
/// running behind. A deploy is the only time this happens, and a hook
/// waiting a few seconds then is better than one that drops a push.
const RESTART_RETRIES: usize = 3;
const RESTART_WAIT: Duration = Duration::from_millis(2500);

/// A response read into `T`, or into [`Error::Status`] when it is not a
/// success.
async fn read<T: DeserializeOwned>(response: reqwest::Response) -> Result<T, Error> {
    let status = response.status();
    if status.is_redirection() {
        return Err(Error::Redirect {
            code: status.as_u16(),
            location: response
                .headers()
                .get(reqwest::header::LOCATION)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .unwrap_or_else(|| "nowhere it said".to_string()),
        });
    }
    // Read as bytes rather than text: the body is only ever decoded as
    // JSON, and this keeps the client off reqwest's optional charset
    // feature.
    let body = response.bytes().await?;

    if !status.is_success() {
        return Err(Error::Status {
            code: status.as_u16(),
            body: String::from_utf8_lossy(&body).trim().to_string(),
        });
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Signs `request` as `signer`'s device, the way
/// [`recall_wire::signature::sign_request`] describes: the method, host,
/// path and query exactly as they will be sent, the protocol header, and
/// the digest of the exact body bytes.
///
/// Read off the built request rather than assembled alongside it, so what
/// is signed cannot drift from what is sent: reqwest encodes the query, and
/// the server checks the encoding it received.
fn sign(signer: &Signer, request: &mut reqwest::Request) -> Result<(), Error> {
    let protocol = PROTOCOL.to_string();
    let nonce = nonce()?;
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let signed = {
        let url = request.url();
        let host = url.host_str().unwrap_or_default();
        // `port()` is `None` for the scheme's default port, which is what
        // `Host` leaves out too, and the server drops `:80` and `:443`
        // either way.
        let authority = signature::normalize_authority(&match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        });
        let target = Target {
            method: request.method().as_str(),
            authority: &authority,
            path: url.path(),
            query: url.query(),
        };
        let body = request
            .body()
            .and_then(|b| b.as_bytes())
            .unwrap_or_default();
        signature::sign_request(
            &signer.key,
            &signer.device_id,
            &target,
            &protocol,
            body,
            created,
            &nonce,
        )?
    };
    let headers = request.headers_mut();
    // Set on the request itself rather than left to the client's defaults,
    // so the value signed is certainly the value sent.
    for (name, value) in [
        (PROTOCOL_HEADER, protocol),
        (signature::CONTENT_DIGEST_HEADER, signed.content_digest),
        (signature::SIGNATURE_INPUT_HEADER, signed.signature_input),
        (signature::SIGNATURE_HEADER, signed.signature),
    ] {
        let value = HeaderValue::from_str(&value)
            .map_err(|_| Error::Sign(SignatureError::Malformed("header value")))?;
        headers.insert(HeaderName::from_static(name), value);
    }
    Ok(())
}

/// A fresh nonce: 128 random bits, base64url. Never reused, because the
/// server refuses a nonce it has seen from this device inside the window.
fn nonce() -> Result<String, Error> {
    use base64::Engine;
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| Error::Nonce(e.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::FakeServer;

    /// Every request names the protocol and the build that sent it.
    #[tokio::test]
    async fn every_request_says_which_protocol_and_build_sent_it() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();
        client.pull("acme/app").await.unwrap();
        let (agent, protocol) = server.last_identity();
        assert_eq!(protocol.as_deref(), Some("1"));
        assert!(
            agent.as_deref().is_some_and(|a| a.starts_with("recall/")),
            "{agent:?}"
        );
    }

    /// A server from before the discovery document answers 404, which is
    /// an answer: it speaks protocol 1 and says nothing more.
    #[tokio::test]
    async fn a_server_without_discovery_is_not_an_error() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();
        assert_eq!(client.discover().await.unwrap(), None);
    }

    #[tokio::test]
    async fn pushes_and_pulls_against_a_real_server() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();

        let req = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            content: Some("hello".into()),
            source_env: "test".into(),
            deleted: false,
            base_sha256: None,
        };
        let resp = client.push(&req).await.unwrap();
        assert!(resp.ok);
        assert_eq!(server.pushes(), vec![req]);

        let resp = client.pull("acme/app").await.unwrap();
        assert_eq!(
            resp.project_key, "acme/app",
            "query parameter did not arrive"
        );
    }

    /// A project key with a slash has to survive query escaping.
    #[tokio::test]
    async fn escapes_the_project_key() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();
        let resp = client.pull("acme/app with spaces&more").await.unwrap();
        assert_eq!(resp.project_key, "acme/app with spaces&more");
    }

    #[tokio::test]
    async fn sends_the_bearer_token() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "s3cret").unwrap();
        client.health().await.unwrap();
        assert_eq!(
            server.last_authorization().as_deref(),
            Some("Bearer s3cret")
        );
    }

    /// A non-2xx must surface as a typed error carrying both halves — the
    /// hook's exit code comes off the status, the user's explanation off the
    /// body.
    #[tokio::test]
    async fn a_non_2xx_becomes_a_status_error() {
        let server = FakeServer::start().await;
        server.fail_with(403, r#"{"error":"invalid token"}"#);
        let client = Client::new(&server.url, "token").unwrap();

        let err = client.pull("acme/app").await.unwrap_err();
        match err {
            Error::Status { code, ref body } => {
                assert_eq!(code, 403);
                assert!(body.contains("invalid token"), "body lost: {body}");
            }
            other => panic!("expected a status error, got {other:?}"),
        }
        assert!(err.to_string().contains("403"));
    }

    /// Client-side validation stops a bad request before it is sent at all.
    #[tokio::test]
    async fn refuses_to_send_an_invalid_request() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();

        let err = client
            .push(&PushRequest {
                project_key: String::new(),
                file_path: "MEMORY.md".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
        assert!(
            server.pushes().is_empty(),
            "an invalid request was sent anyway"
        );
    }

    /// The distinction `recall connect` depends on: `/health` answers any
    /// token, so only an authenticated route can say the token is wrong.
    #[tokio::test]
    async fn check_token_tells_a_wrong_token_from_a_right_one() {
        let server = FakeServer::start().await;
        server.require_token("right");

        let wrong = Client::new(&server.url, "wrong").unwrap();
        assert!(wrong.health().await.is_ok(), "health does not check tokens");
        match wrong.check_token().await {
            Err(Error::Status { code: 401, .. }) => {}
            other => panic!("expected a 401, got {other:?}"),
        }

        let right = Client::new(&server.url, "right").unwrap();
        right.check_token().await.unwrap();
    }

    /// A redirect is reported with where it pointed, and nothing reaches
    /// that place: not a token, not a signature, and not a body, which when
    /// enrolling carries the authkey.
    #[tokio::test]
    async fn a_redirect_is_reported_and_never_followed() {
        let elsewhere = FakeServer::start().await;
        for code in [301, 302, 307, 308] {
            let server = FakeServer::start().await;
            let location = format!("{}/sync", elsewhere.url);
            server.redirect_to(code, &location);

            let bearer = Client::new(&server.url, "s3cret").unwrap();
            let req = PushRequest {
                project_key: "acme/app".into(),
                file_path: "MEMORY.md".into(),
                content: Some("private".into()),
                source_env: "test".into(),
                ..Default::default()
            };
            let err = bearer.push(&req).await.unwrap_err();
            match &err {
                Error::Redirect {
                    code: got,
                    location: to,
                } => {
                    assert_eq!((*got, to.as_str()), (code, location.as_str()));
                }
                other => panic!("expected a redirect, got {other:?}"),
            }
            let said = err.to_string();
            assert!(
                said.contains(&format!("the server redirected to {location}"))
                    && said.contains("update RECALL_URL"),
                "{said}"
            );

            let key = crate::device::DeviceKey::generate().unwrap();
            let signed = Client::new(&server.url, "")
                .unwrap()
                .with_signer(key.signer("dev_x"));
            assert!(matches!(
                signed.push(&req).await,
                Err(Error::Redirect { .. })
            ));
            let enrol = key.enroll_request("laptop", Some("recall-ak-secret"));
            assert!(matches!(
                Client::new(&server.url, "").unwrap().enroll(&enrol).await,
                Err(Error::Redirect { .. })
            ));
            assert_eq!(server.requests(), 3, "{code}: each was sent once");
        }
        assert_eq!(elsewhere.requests(), 0, "nothing followed a redirect");
    }

    /// Only the two refusals that name the device say it is gone, and only
    /// "unknown device" is one a cloud session may answer by enrolling
    /// again. Any other 401 says nothing about the device.
    #[test]
    fn only_the_server_naming_the_device_says_it_is_gone() {
        let refusal = |reason: &str| Error::Status {
            code: 401,
            body: serde_json::json!({ "error": reason }).to_string(),
        };
        let unknown = refusal(UNKNOWN_DEVICE);
        assert!(unknown.device_gone() && unknown.device_unknown() && !unknown.device_revoked());
        let revoked = refusal(REVOKED_DEVICE);
        assert!(revoked.device_gone() && revoked.device_revoked() && !revoked.device_unknown());
        for other in [
            "unauthorized: signature does not verify",
            "unauthorized: signature created too far in the past",
            "unauthorized",
        ] {
            let e = refusal(other);
            assert!(
                !e.device_gone() && !e.device_unknown() && !e.device_revoked(),
                "{other}"
            );
        }
        let forbidden = Error::Status {
            code: 403,
            body: serde_json::json!({ "error": UNKNOWN_DEVICE }).to_string(),
        };
        assert!(!forbidden.device_gone(), "only a 401 says it");
    }

    /// A signature refused for any reason but the server having only just
    /// started is not signed again: it would be refused again, and a hook
    /// would wait seconds for nothing.
    #[tokio::test]
    async fn a_refused_signature_is_not_signed_again() {
        let server = FakeServer::start().await;
        server.fail_with(
            401,
            r#"{"error":"unauthorized: signature does not verify"}"#,
        );
        let key = crate::device::DeviceKey::generate().unwrap();
        let client = Client::new(&server.url, "")
            .unwrap()
            .with_signer(key.signer("dev_x"));

        let started = std::time::Instant::now();
        let err = client.pull("acme/app").await.unwrap_err();
        assert!(!err.signed_too_soon(), "{err}");
        assert_eq!(server.requests(), 1, "sent once");
        assert!(started.elapsed() < RESTART_WAIT, "and not waited on");
    }

    /// The token never reaches a log line or a test failure.
    #[test]
    fn nothing_prints_the_token() {
        let client = Client::new("https://recall.example.com", "s3cret-token").unwrap();
        let printed = format!("{client:?}");
        assert!(!printed.contains("s3cret-token"), "{printed}");
        assert!(printed.contains("recall.example.com"), "{printed}");
    }

    #[tokio::test]
    async fn trailing_slashes_in_the_base_url_do_not_double_up() {
        let server = FakeServer::start().await;
        let client = Client::new(&format!("{}///", server.url), "token").unwrap();
        assert!(client.health().await.is_ok());
    }
}
