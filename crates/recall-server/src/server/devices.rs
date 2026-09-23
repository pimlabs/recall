//! The device routes: enrolling and polling, which anyone may call, and
//! approving, listing and revoking devices and authkeys, which need
//! the operator's token or an admin device.
//!
//! Enrolment is RFC 8628's device flow. The user code is typed by a person
//! on another screen, so it is short; what keeps it from being guessed is
//! that approving one is itself authenticated (§5.1), and it expires in
//! fifteen minutes. The enrolment id the machine polls with is the long
//! secret, as the device code is in the RFC.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use recall_wire::devices::{
    self, displayable, normalize_user_code, ACCESS_DENIED, AUTHKEY_PREFIX, AUTHORIZATION_PENDING,
    CODE_TTL_SECONDS, DEFAULT_MAX_DEVICES, EXPIRED_TOKEN, INVALID_GRANT, MAX_AUTHKEY_DAYS,
    MAX_TAG_CHARS, POLL_INTERVAL_SECONDS, SCOPE_ADMIN, SCOPE_SYNC, SLOW_DOWN, USER_CODE_ALPHABET,
};
use recall_wire::signature::encode_public_key;
use recall_wire::{
    ApproveRequest, AuthkeyCreated, AuthkeyList, AuthkeyRequest, AuthkeyRevokeRequest, DenyRequest,
    DenyResponse, DeviceIdentity, DeviceList, EnrollApproved, EnrollPending, EnrollPollRequest,
    EnrollPollResponse, EnrollRequest, PendingEnrollment,
};
use serde::de::DeserializeOwned;
use time::OffsetDateTime;

use super::audit::{actor_for, signed_request_for};
use super::auth::{Caller, SignedRequestInfo};
use super::middleware::{too_large, ClientIp};
use super::respond::{error, internal, json, Refusal};
use super::AppState;
use crate::audit::leaf;
use crate::store::{
    plain_name, Created, Decision, Inserted, NewAuthkey, NewDevice, NewEnrollment, Poll,
};
use crate::{format_timestamp, now, parse_timestamp};

/// How many enrolments may wait for approval at once. An owner has a
/// handful at most; the cap is what bounds the table when someone who is
/// not the owner calls the unauthenticated enrol route in a loop, each
/// row living fifteen minutes.
const MAX_PENDING_ENROLLMENTS: usize = 1000;

/// How many of those may come from one address, as the rate limiter keys
/// it, so one address cannot take all of them and lock the owner's own
/// machines out for fifteen minutes.
const MAX_PENDING_PER_ADDRESS: usize = 5;

/// How long an expired enrolment is kept, so a machine polling late hears
/// `expired_token` rather than `invalid_grant`.
pub(super) const EXPIRED_ENROLLMENT_KEPT: Duration = Duration::from_secs(60 * 60);

/// What a device an authkey enrols is called when the key has no
/// tag.
const UNTAGGED: &str = "device";

/// Reads a JSON body, answering with the wording `POST /sync` uses for one
/// that does not parse.
fn body<T: DeserializeOwned>(bytes: &Bytes) -> Result<T, Refusal> {
    serde_json::from_slice(bytes)
        .map_err(|_| Refusal::new(StatusCode::BAD_REQUEST, "invalid json body"))
}

/// A body on the routes with a small limit: one over it is refused in the
/// usual JSON shape rather than axum's plain text.
fn small_body(bytes: Result<Bytes, BytesRejection>) -> Result<Bytes, Refusal> {
    bytes.map_err(|rejection| match rejection.status() {
        StatusCode::PAYLOAD_TOO_LARGE => too_large(),
        status => Refusal::new(status, "could not read the request body"),
    })
}

/// A reply that carries a secret, which RFC 6749 §5.1 says no cache may
/// keep.
fn no_store(mut resp: Response) -> Response {
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn random(bytes: usize) -> anyhow::Result<Vec<u8>> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(buf)
}

/// RFC 4648 base32, lowercase, unpadded: ids and keys a person may have to
/// read out or type, with no case to get wrong and no `0`/`O` or `1`/`l`
/// to confuse.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// `prefix` and `bytes` random bytes in [`base32`].
fn new_id(prefix: &str, bytes: usize) -> anyhow::Result<String> {
    Ok(format!("{prefix}{}", base32(&random(bytes)?)))
}

/// Eight characters of the RFC 8628 §6.1 alphabet, uniformly: a byte is
/// used only below the largest multiple of 20 that fits, so no letter is
/// likelier than another.
fn new_user_code() -> anyhow::Result<String> {
    let mut code = String::with_capacity(9);
    while code.len() < 8 {
        for b in random(16)? {
            if code.len() == 8 {
                break;
            }
            if b < 240 {
                code.push(USER_CODE_ALPHABET[(b % 20) as usize] as char);
            }
        }
    }
    normalize_user_code(&code).context("a generated user code did not normalize")
}

fn later(by: Duration) -> String {
    format_timestamp(OffsetDateTime::now_utc() + by)
}

fn name_taken(name: &str) -> Refusal {
    Refusal::new(
        StatusCode::CONFLICT,
        format!(
            "a device named {name} already exists; revoke it first, or enrol with another name"
        ),
    )
}

/// `POST /v1/devices/enroll`.
pub(super) async fn handle_enroll(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let req: EnrollRequest = match small_body(bytes).and_then(|b| body(&b)) {
        Ok(req) => req,
        Err(refused) => return refused.into_response(),
    };
    let key = match req.validate() {
        Ok(key) => key,
        Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    // Stored as the encoder writes it, whatever whitespace it came with.
    let public_key = encode_public_key(&key);

    if let Some(authkey) = req.authkey.as_deref() {
        return enroll_with_authkey(&state, authkey, &public_key, &req.agent);
    }

    // Whether the name is taken is not said here, only when the code is
    // approved. This route needs no credential, so an answer that said it
    // would tell anyone which names the owner's devices have, a question
    // at a time; asked this way, each question waits for approval, and
    // counts against its address's few waiting places.
    let name = &plain_name(&req.name);
    let now = now();
    let expires_at = later(Duration::from_secs(CODE_TTL_SECONDS));
    let enrollment_id = match new_id("enr_", 16) {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    // A clash with a code still waiting is one in 25 billion per waiting
    // enrolment; a few tries make it vanish.
    for _ in 0..8 {
        let user_code = match new_user_code() {
            Ok(code) => code,
            Err(e) => return internal(e),
        };
        let created = state.store.create_enrollment(
            &NewEnrollment {
                enrollment_id: &enrollment_id,
                user_code: &user_code,
                name,
                public_key: &public_key,
                agent: &req.agent,
                created_at: &now,
                expires_at: &expires_at,
                client_ip: &client_ip,
            },
            MAX_PENDING_ENROLLMENTS,
            MAX_PENDING_PER_ADDRESS,
        );
        match created {
            Ok(Created::Created) => {
                return no_store(json(
                    StatusCode::OK,
                    &EnrollPending {
                        enrollment_id,
                        user_code,
                        expires_in: CODE_TTL_SECONDS,
                        interval: POLL_INTERVAL_SECONDS,
                    },
                ))
            }
            Ok(Created::CodeTaken) => continue,
            Ok(Created::Full) => {
                return error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too many enrolments are waiting for approval, try again later",
                )
            }
            Ok(Created::AddressFull) => {
                return error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many enrolments from this address are waiting for approval; \
                     approve or deny them, or let them expire",
                )
            }
            Err(e) => return internal(e),
        }
    }
    internal(anyhow::anyhow!("could not find a free user code"))
}

/// An authkey approves at once, `sync` scope only, which is the
/// whole of what a leaked one can give away.
///
/// Nobody looks at a device enrolled this way before it exists, so it does
/// not choose its name: it is the key's tag and the start of its id, such
/// as `cloud-k3jz9w2q`, and cannot pass for the owner's laptop.
fn enroll_with_authkey(state: &AppState, authkey: &str, public_key: &str, agent: &str) -> Response {
    let refused = |why: &str| {
        error(
            StatusCode::UNAUTHORIZED,
            &format!("unauthorized: this authkey {why}"),
        )
    };
    let key = match state
        .store
        .authkey_by_hash(&recall_wire::content_sha256(authkey.trim()))
    {
        Ok(Some(key)) => key,
        Ok(None) => return refused("is not one this server issued"),
        Err(e) => return internal(e),
    };
    let now = now();
    if key.revoked_at.is_some() {
        return refused("has been revoked");
    }
    if key.expires_at <= now {
        return refused("has expired");
    }
    let device_id = match new_id("dev_", 16) {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    let tag = if key.tag.is_empty() {
        UNTAGGED
    } else {
        &key.tag
    };
    let random = &device_id["dev_".len()..];
    let max_devices = key.max_devices.unwrap_or(DEFAULT_MAX_DEVICES);
    // The short name first; the whole id only in the one-in-a-trillion
    // case that the short one is taken.
    for name in [format!("{tag}-{}", &random[..8]), format!("{tag}-{random}")] {
        let inserted = state.store.insert_device(
            &NewDevice {
                id: &device_id,
                name: &name,
                public_key,
                scope: SCOPE_SYNC,
                agent,
                ephemeral: key.ephemeral,
                authkey_id: Some(&key.id),
                created_at: &now,
            },
            Some(max_devices),
        );
        match inserted {
            Ok(Inserted::Done(device)) => {
                return no_store(json(
                    StatusCode::OK,
                    &EnrollApproved {
                        device_id: device.id,
                        name: device.name,
                        scope: device.scope,
                        ephemeral: device.ephemeral,
                    },
                ))
            }
            Ok(Inserted::NameTaken) => continue,
            Ok(Inserted::KeyFull) => {
                return error(
                    StatusCode::FORBIDDEN,
                    &format!(
                        "forbidden: this authkey already has its {} devices; \
                         revoke one, or make another authkey",
                        max_devices
                    ),
                )
            }
            Err(e) => return internal(e),
        }
    }
    internal(anyhow::anyhow!(
        "could not find a free name for {device_id}"
    ))
}

/// `POST /v1/devices/enroll/poll`: RFC 8628 §3.4 and §3.5, with each
/// error code as the body's `error`, which is the shape every Recall error
/// already has.
pub(super) async fn handle_poll(
    State(state): State<Arc<AppState>>,
    bytes: Result<Bytes, BytesRejection>,
) -> Response {
    let req: EnrollPollRequest = match small_body(bytes).and_then(|b| body(&b)) {
        Ok(req) => req,
        Err(refused) => return refused.into_response(),
    };
    if req.enrollment_id.is_empty() {
        return error(StatusCode::BAD_REQUEST, "enrollment_id is required");
    }
    let poll = state.store.poll_enrollment(
        &req.enrollment_id,
        OffsetDateTime::now_utc(),
        Duration::from_secs(POLL_INTERVAL_SECONDS),
    );
    let code = match poll {
        Ok(Poll::Approved { device_id, scope }) => {
            return no_store(json(
                StatusCode::OK,
                &EnrollPollResponse { device_id, scope },
            ))
        }
        Ok(Poll::Pending) => AUTHORIZATION_PENDING,
        Ok(Poll::SlowDown) => SLOW_DOWN,
        Ok(Poll::Expired) => EXPIRED_TOKEN,
        Ok(Poll::Denied) => ACCESS_DENIED,
        Ok(Poll::Unknown) => INVALID_GRANT,
        Err(e) => return internal(e),
    };
    no_store(error(StatusCode::BAD_REQUEST, code))
}

/// The code a person typed, normalized, or the 400 that says what a code
/// looks like.
fn user_code(input: &str) -> Result<String, Refusal> {
    normalize_user_code(input).ok_or_else(|| {
        Refusal::new(
            StatusCode::BAD_REQUEST,
            "user_code must be the 8 letters the device shows, such as WDJB-MJHT",
        )
    })
}

fn undecided<T>(decision: Decision<T>) -> Result<T, Refusal> {
    match decision {
        Decision::Done(v) => Ok(v),
        Decision::NotFound => Err(Refusal::new(
            StatusCode::NOT_FOUND,
            "no enrolment is waiting with that code",
        )),
        Decision::Expired => Err(Refusal::new(
            StatusCode::GONE,
            "that code has expired; start the enrolment again",
        )),
        Decision::AlreadyDecided => Err(Refusal::new(
            StatusCode::CONFLICT,
            "that code was already approved or denied",
        )),
        Decision::KeyMismatch => Err(Refusal::new(
            StatusCode::CONFLICT,
            "that code's key does not have the fingerprint given; nothing was approved",
        )),
        Decision::NameTaken(name) => Err(name_taken(&name)),
    }
}

/// `POST /v1/devices/approve`.
pub(super) async fn handle_approve(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    bytes: Bytes,
) -> Response {
    let req: ApproveRequest = match body(&bytes) {
        Ok(req) => req,
        Err(refused) => return refused.into_response(),
    };
    if req.scope != SCOPE_SYNC && req.scope != SCOPE_ADMIN {
        return error(StatusCode::BAD_REQUEST, "scope must be sync or admin");
    }
    let code = match user_code(&req.user_code) {
        Ok(code) => code,
        Err(refused) => return refused.into_response(),
    };
    let device_id = match new_id("dev_", 16) {
        Ok(id) => id,
        Err(e) => return internal(e),
    };
    let actor = actor_for(caller.as_ref().map(|Extension(c)| c));
    let request = signed_request_for(signed.as_ref().map(|Extension(s)| s));
    let at = now();
    match state
        .store
        .approve_enrollment_audited(
            &code,
            &device_id,
            &req.scope,
            &at,
            req.fingerprint.as_deref(),
            |seq, device| {
                leaf::encode(
                    seq,
                    &at,
                    leaf::action::APPROVE,
                    &actor,
                    leaf::subject_device(
                        &device.id,
                        &device.name,
                        &device.scope,
                        &device.public_key,
                        &device.fingerprint,
                    ),
                    request.as_ref(),
                )
            },
        )
        .map(undecided)
    {
        Ok(Ok(device)) => json(StatusCode::OK, &device),
        Ok(Err(refused)) => refused.into_response(),
        Err(e) => internal(e),
    }
}

/// `POST /v1/devices/deny`.
pub(super) async fn handle_deny(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    bytes: Bytes,
) -> Response {
    let req: DenyRequest = match body(&bytes) {
        Ok(req) => req,
        Err(refused) => return refused.into_response(),
    };
    let code = match user_code(&req.user_code) {
        Ok(code) => code,
        Err(refused) => return refused.into_response(),
    };
    let actor = actor_for(caller.as_ref().map(|Extension(c)| c));
    let request = signed_request_for(signed.as_ref().map(|Extension(s)| s));
    let at = now();
    let leaf_code = code.clone();
    match state
        .store
        .deny_enrollment_audited(&code, &at, |seq, name| {
            leaf::encode(
                seq,
                &at,
                leaf::action::DENY,
                &actor,
                leaf::subject_denied(&leaf_code, name),
                request.as_ref(),
            )
        })
        .map(undecided)
    {
        Ok(Ok(name)) => json(
            StatusCode::OK,
            &DenyResponse {
                user_code: code,
                name,
                denied: true,
            },
        ),
        Ok(Err(refused)) => refused.into_response(),
        Err(e) => internal(e),
    }
}

/// `GET /v1/devices/pending/{user_code}`: what approving the code would
/// approve, so the approver can check the name and fingerprint against the
/// machine's screen first (RFC 8628 §5.4). It is judged exactly as
/// approving it would be, so the two never disagree, and like every answer
/// about an enrolment it is kept out of caches.
pub(super) async fn handle_pending(
    State(state): State<Arc<AppState>>,
    Path(input): Path<String>,
) -> Response {
    let code = match user_code(&input) {
        Ok(code) => code,
        Err(refused) => return refused.into_response(),
    };
    let now = OffsetDateTime::now_utc();
    match state
        .store
        .pending_enrollment(&code, &format_timestamp(now))
        .map(undecided)
    {
        Ok(Ok(waiting)) => {
            // Only a key that parsed was ever stored.
            let fingerprint = recall_wire::signature::parse_public_key(&waiting.public_key)
                .map(|k| recall_wire::signature::fingerprint(&k))
                .unwrap_or_default();
            let expires_in = parse_timestamp(&waiting.expires_at)
                .map(|at| (at - now).whole_seconds().max(0) as u64)
                .unwrap_or(0);
            no_store(json(
                StatusCode::OK,
                &PendingEnrollment {
                    user_code: code,
                    name: waiting.name,
                    agent: waiting.agent,
                    fingerprint,
                    expires_in,
                },
            ))
        }
        Ok(Err(refused)) => no_store(refused.into_response()),
        Err(e) => internal(e),
    }
}

/// `GET /v1/devices/me`: the device that signed the request, so a client
/// can check the server knows it, with no need of the admin scope.
pub(super) async fn handle_me(Extension(caller): Extension<Caller>) -> Response {
    match caller {
        Caller::Device {
            id,
            name,
            scope,
            ephemeral,
            ..
        } => json(
            StatusCode::OK,
            &DeviceIdentity {
                device_id: id,
                name,
                scope,
                ephemeral,
            },
        ),
        Caller::Operator => error(
            StatusCode::NOT_FOUND,
            "not a device: this request was authenticated with RECALL_TOKEN",
        ),
    }
}

/// `GET /v1/devices`.
pub(super) async fn handle_list_devices(State(state): State<Arc<AppState>>) -> Response {
    match state.store.devices() {
        Ok(devices) => json(StatusCode::OK, &DeviceList { devices }),
        Err(e) => internal(e),
    }
}

/// `POST /v1/devices/{id}/revoke`. Revoking twice is not an error; the
/// first time stands.
pub(super) async fn handle_revoke_device(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    Path(id): Path<String>,
) -> Response {
    let actor = actor_for(caller.as_ref().map(|Extension(c)| c));
    let request = signed_request_for(signed.as_ref().map(|Extension(s)| s));
    let at = now();
    match state.store.revoke_device_audited(&id, &at, |seq, device| {
        leaf::encode(
            seq,
            &at,
            leaf::action::REVOKE,
            &actor,
            leaf::subject_device_id(&device.id, &device.name),
            request.as_ref(),
        )
    }) {
        Ok(Some(device)) => json(StatusCode::OK, &device),
        Ok(None) => error(StatusCode::NOT_FOUND, "no device has that id"),
        Err(e) => internal(e),
    }
}

/// `POST /v1/authkeys`.
pub(super) async fn handle_create_authkey(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    bytes: Bytes,
) -> Response {
    let req: AuthkeyRequest = match body(&bytes) {
        Ok(req) => req,
        Err(refused) => return refused.into_response(),
    };
    if !(1..=MAX_AUTHKEY_DAYS).contains(&req.expires_in_days) {
        return error(StatusCode::BAD_REQUEST, "expires_in_days must be 1 to 365");
    }
    // The tag becomes part of the name of every device the key enrols, so
    // it is held to the same rules as a name.
    if !displayable(&req.tag, MAX_TAG_CHARS) {
        return error(
            StatusCode::BAD_REQUEST,
            "tag must be at most 32 characters, with no control, format or invisible characters",
        );
    }
    if req.max_devices == Some(0) {
        return error(StatusCode::BAD_REQUEST, "max_devices must be at least 1");
    }
    // Stored, rather than applied when the key is used, so the list shows
    // the limit every key has.
    let max_devices = req.max_devices.unwrap_or(DEFAULT_MAX_DEVICES);
    let (id, secret) = match (new_id("ak_", 10), new_id(AUTHKEY_PREFIX, 32)) {
        (Ok(id), Ok(secret)) => (id, secret),
        (Err(e), _) | (_, Err(e)) => return internal(e),
    };
    let expires_at = later(Duration::from_secs(
        u64::from(req.expires_in_days) * 24 * 60 * 60,
    ));
    let actor = actor_for(caller.as_ref().map(|Extension(c)| c));
    let request = signed_request_for(signed.as_ref().map(|Extension(s)| s));
    let at = now();
    let stored = state.store.insert_authkey_audited(
        &NewAuthkey {
            id: &id,
            key_sha256: &recall_wire::content_sha256(&secret),
            tag: &plain_name(&req.tag),
            ephemeral: req.ephemeral,
            max_devices: Some(max_devices),
            created_at: &at,
            expires_at: &expires_at,
        },
        |seq, key| {
            leaf::encode(
                seq,
                &at,
                leaf::action::AUTHKEY_CREATE,
                &actor,
                leaf::subject_authkey(&key.id, &key.tag, key.ephemeral, key.max_devices),
                request.as_ref(),
            )
        },
    );
    match stored {
        Ok(key) => no_store(json(
            StatusCode::OK,
            &AuthkeyCreated {
                id: key.id,
                key: secret,
                tag: key.tag,
                ephemeral: key.ephemeral,
                max_devices: key.max_devices,
                created_at: key.created_at,
                expires_at: key.expires_at,
            },
        )),
        Err(e) => internal(e),
    }
}

/// `GET /v1/authkeys`.
pub(super) async fn handle_list_authkeys(State(state): State<Arc<AppState>>) -> Response {
    match state.store.authkeys() {
        Ok(authkeys) => json(StatusCode::OK, &AuthkeyList { authkeys }),
        Err(e) => internal(e),
    }
}

/// `POST /v1/authkeys/{id}/revoke`: no new device enrols with it. The
/// devices it enrolled keep working unless the body asks for them to be
/// revoked too; the body may be empty.
pub(super) async fn handle_revoke_authkey(
    State(state): State<Arc<AppState>>,
    caller: Option<Extension<Caller>>,
    signed: Option<Extension<SignedRequestInfo>>,
    Path(id): Path<String>,
    bytes: Bytes,
) -> Response {
    let req: AuthkeyRevokeRequest = if bytes.iter().all(u8::is_ascii_whitespace) {
        AuthkeyRevokeRequest::default()
    } else {
        match body(&bytes) {
            Ok(req) => req,
            Err(refused) => return refused.into_response(),
        }
    };
    let actor = actor_for(caller.as_ref().map(|Extension(c)| c));
    let request = signed_request_for(signed.as_ref().map(|Extension(s)| s));
    let at = now();
    let revoke_devices = req.revoke_devices;
    match state
        .store
        .revoke_authkey_audited(&id, &at, revoke_devices, |seq, key| {
            leaf::encode(
                seq,
                &at,
                leaf::action::AUTHKEY_REVOKE,
                &actor,
                leaf::subject_authkey_revoke(&key.id, revoke_devices),
                request.as_ref(),
            )
        }) {
        Ok(Some(key)) => json(StatusCode::OK, &key),
        Ok(None) => error(StatusCode::NOT_FOUND, "no authkey has that id"),
        Err(e) => internal(e),
    }
}

/// What the discovery document says about devices.
pub(super) fn capability() -> recall_wire::DevicesCapability {
    recall_wire::DevicesCapability {
        enroll_path: devices::ENROLL_PATH.to_string(),
        code_ttl_seconds: CODE_TTL_SECONDS,
        poll_interval_seconds: POLL_INTERVAL_SECONDS,
        signature_window_seconds: super::auth::WINDOW,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10's test vectors, lowercased and unpadded.
    #[test]
    fn base32_matches_rfc4648() {
        for (input, want) in [
            ("", ""),
            ("f", "my"),
            ("fo", "mzxq"),
            ("foo", "mzxw6"),
            ("foob", "mzxw6yq"),
            ("fooba", "mzxw6ytb"),
            ("foobar", "mzxw6ytboi"),
        ] {
            assert_eq!(base32(input.as_bytes()), want, "{input:?}");
        }
    }

    #[test]
    fn ids_have_their_prefix_and_128_bits() {
        let id = new_id("dev_", 16).unwrap();
        assert!(id.starts_with("dev_") && id.len() == 4 + 26, "{id}");
        assert_ne!(id, new_id("dev_", 16).unwrap());
        let key = new_id(AUTHKEY_PREFIX, 32).unwrap();
        assert_eq!(key.len(), AUTHKEY_PREFIX.len() + 52, "{key}");
    }

    #[test]
    fn user_codes_use_only_the_alphabet() {
        for _ in 0..100 {
            let code = new_user_code().unwrap();
            assert_eq!(code.len(), 9, "{code}");
            assert_eq!(&code[4..5], "-");
            assert!(code
                .chars()
                .filter(|c| *c != '-')
                .all(|c| USER_CODE_ALPHABET.contains(&(c as u8))));
        }
    }
}
