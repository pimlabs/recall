//! Passkey (WebAuthn) sign-in for `/admin`, with `webauthn-rs`.
//!
//! Three ceremonies, each started by one request and finished by the next:
//!
//! - **Bootstrap.** The operator's `RECALL_TOKEN`, with the one-time code
//!   the server prints where it runs (see [`crate::bootstrap`]), registers
//!   the first passkey, and only the first: once one exists, the route
//!   refuses whatever it is shown, so a token that leaks later cannot
//!   register a "first" passkey of its own.
//! - **Sign-in.** Anyone may start one; only a registered passkey finishes
//!   it. It is discoverable (usernameless): there is one owner, so the page
//!   asks for no name and the authenticator offers the passkey it holds.
//! - **Adding a passkey.** Only a signed-in owner, never the token, and
//!   only a session that signed in within the last five minutes.
//!
//! A ceremony's state is not kept on the server. It is sealed with
//! AES-256-GCM, under a key made when the process starts and never written
//! anywhere, together with when it expires and what may finish it, and the
//! sealed bytes are the `ceremony_id` the page sends back. The browser can
//! neither read nor change it, so the challenge stays the server's, which is
//! what `webauthn-rs` guards by refusing to serialise this state unless told
//! to. And starting a sign-in, which anyone may do, makes the server hold
//! nothing, so nobody can fill a table of ceremonies and keep the owner from
//! signing in. What the server does keep is the id of each ceremony whose
//! answer verified, until it would have expired anyway, so that none is
//! finished twice. Only a registered passkey, the token or a session can add
//! to that, so it stays small. A restart makes a new key, which ends every
//! ceremony in flight: the page starts again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use openssl::symm::{decrypt_aead, encrypt_aead, Cipher};
use recall_wire::devices::displayable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use webauthn_rs::prelude::{
    DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder, WebauthnError,
};

use super::admin::{
    cleared_cookie, csrf_token, no_store, require_recent_sign_in, session_cookie,
    session_token_sha256, OwnerSession, PasskeyStatus, SessionView, SESSION_IDLE, SESSION_LIMIT,
};
use super::auth::Caller;
use super::respond::{error, internal, json, Refusal};
use super::AppState;
use crate::format_timestamp;
use crate::store::{
    AddedCredential, AdminCredential, BootstrapCode, FirstPasskey, NewAdminCredential,
    RemovedCredential,
};

/// How long a ceremony may take, from its challenge to its answer. Also
/// the timeout the browser is given: long enough to find a phone, unlock
/// it and answer, per WebAuthn's own recommendation.
const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);

/// Bound into every sealed ceremony, so that nothing else this key might
/// ever seal opens as one.
const CEREMONY_AAD: &[u8] = b"recall admin ceremony v1";

/// AES-256-GCM's nonce and tag, either side of the sealed bytes.
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

/// The longest name a passkey may be given.
const MAX_NAME_CHARS: usize = 64;

/// What the authenticator shows the passkey as. There is one owner.
const USER_NAME: &str = "owner";
const USER_DISPLAY_NAME: &str = "Recall owner";

/// What happens next in a ceremony, and whose it is.
#[derive(Serialize, Deserialize)]
enum Pending {
    /// Registering the first passkey, with the token.
    Bootstrap {
        registration: PasskeyRegistration,
        user_handle: Uuid,
        /// The bootstrap code it was started with, which finishing it uses
        /// up: SHA-256, as the store knows it.
        code_sha256: String,
    },
    /// Registering another, from a session.
    Add {
        registration: PasskeyRegistration,
        user_handle: Uuid,
        /// SHA-256 of the cookie of the session that started it, which
        /// alone may finish it.
        session: String,
    },
    /// Signing in.
    SignIn(DiscoverableAuthentication),
}

/// What a `ceremony_id` holds, sealed.
#[derive(Serialize, Deserialize)]
struct Ceremony {
    /// Random: what the server remembers once it has been finished.
    id: String,
    /// When it expires, as a UNIX time by the clock [`AppState::now`] reads.
    expires: i64,
    pending: Pending,
}

/// The site passkeys are bound to.
struct Site {
    webauthn: Webauthn,
    origin: String,
}

/// Passkey sign-in: the relying party, when `RECALL_PUBLIC_URL` gives
/// one, the key ceremonies are sealed with, and the ceremonies finished.
pub(super) struct Passkeys {
    site: Result<Site, String>,
    key: [u8; 32],
    /// The id of every ceremony whose answer verified, and when it
    /// expires: after that it would be refused anyway, and is forgotten.
    finished: Mutex<HashMap<String, i64>>,
}

impl Passkeys {
    /// Passkey sign-in for the site `public_url` names, or off, with the
    /// reason, when it names none.
    pub(super) fn new(public_url: &str) -> Self {
        let mut key = [0u8; 32];
        let site = if public_url.is_empty() {
            Err(
                "RECALL_PUBLIC_URL is not set on the server, so passkey sign-in is off. \
                 Set it to the address this page is served from, such as \
                 https://recall.example.com, and restart the server."
                    .to_string(),
            )
        } else if let Err(e) = getrandom::fill(&mut key) {
            Err(format!(
                "the server has no randomness to seal ceremonies with ({e})"
            ))
        } else {
            site(public_url).map_err(|why| {
                let why = format!(
                    "RECALL_PUBLIC_URL is {public_url:?}, which cannot be used for passkeys: \
                     {why}. Passkey sign-in is off until it is fixed."
                );
                eprintln!("{why}");
                why
            })
        };
        if let (Ok(_), Some(warning)) = (&site, local_only(public_url)) {
            eprintln!("{warning}");
        }
        Self {
            site,
            key,
            finished: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn status(&self) -> PasskeyStatus {
        match &self.site {
            Ok(site) => PasskeyStatus {
                enabled: true,
                origin: Some(site.origin.clone()),
                reason: None,
            },
            Err(why) => PasskeyStatus {
                enabled: false,
                origin: None,
                reason: Some(why.clone()),
            },
        }
    }

    fn site(&self) -> Result<&Site, Refusal> {
        self.site.as_ref().map_err(|why| {
            Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("passkey sign-in is off: {why}"),
            )
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, i64>> {
        self.finished.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets every finished ceremony that has expired by `now`. Answers
    /// how many went.
    pub(super) fn prune(&self, now: i64) -> usize {
        let mut finished = self.lock();
        let before = finished.len();
        finished.retain(|_, expires| *expires > now);
        before - finished.len()
    }

    /// Seals a ceremony into the id the page finishes it with.
    fn begin(&self, pending: Pending, now: i64) -> Result<String, Refusal> {
        let sealing = |e: &dyn std::fmt::Display| {
            Refusal::internal(anyhow::anyhow!("sealing a ceremony: {e}"))
        };
        let ceremony = Ceremony {
            id: URL_SAFE_NO_PAD.encode(random::<16>()?),
            expires: now + CEREMONY_TTL.as_secs() as i64,
            pending,
        };
        let plain = serde_json::to_vec(&ceremony).map_err(|e| sealing(&e))?;
        let nonce = random::<NONCE_BYTES>()?;
        let mut tag = [0u8; TAG_BYTES];
        let sealed = encrypt_aead(
            Cipher::aes_256_gcm(),
            &self.key,
            Some(&nonce),
            CEREMONY_AAD,
            &plain,
            &mut tag,
        )
        .map_err(|e| sealing(&e))?;
        let mut out = Vec::with_capacity(NONCE_BYTES + sealed.len() + TAG_BYTES);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        out.extend_from_slice(&tag);
        Ok(format!("cer_{}", URL_SAFE_NO_PAD.encode(out)))
    }

    /// Opens a ceremony the page sent back: one this process sealed, not
    /// expired by `now`, and not finished already. Anything else is the
    /// same refusal, so a forged id learns nothing from it.
    fn open(&self, ceremony_id: &str, now: i64) -> Result<Ceremony, Refusal> {
        let raw = ceremony_id
            .strip_prefix("cer_")
            .and_then(|b64| URL_SAFE_NO_PAD.decode(b64).ok())
            .filter(|raw| raw.len() > NONCE_BYTES + TAG_BYTES)
            .ok_or_else(unusable)?;
        let (nonce, rest) = raw.split_at(NONCE_BYTES);
        let (sealed, tag) = rest.split_at(rest.len() - TAG_BYTES);
        let plain = decrypt_aead(
            Cipher::aes_256_gcm(),
            &self.key,
            Some(nonce),
            CEREMONY_AAD,
            sealed,
            tag,
        )
        .map_err(|_| unusable())?;
        let ceremony: Ceremony = serde_json::from_slice(&plain).map_err(|_| unusable())?;
        if ceremony.expires <= now || self.lock().contains_key(&ceremony.id) {
            return Err(unusable());
        }
        Ok(ceremony)
    }

    /// Marks a ceremony finished, once its answer has verified. Of two
    /// answers to one ceremony racing, only the first gets past this.
    fn finish(&self, ceremony: &Ceremony, now: i64) -> Result<(), Refusal> {
        let mut finished = self.lock();
        finished.retain(|_, expires| *expires > now);
        if finished
            .insert(ceremony.id.clone(), ceremony.expires)
            .is_some()
        {
            return Err(unusable());
        }
        Ok(())
    }
}

fn unusable() -> Refusal {
    Refusal::new(
        StatusCode::BAD_REQUEST,
        "this ceremony has expired or was already used; start again",
    )
}

/// Every ceremony's start and finish must say its body is JSON, or it is
/// `415`. A page on another site can send a POST here without the browser
/// asking this server first only as a "simple" request, whose
/// `Content-Type` cannot be `application/json`; so a request that cannot
/// have come from this site's page is refused before it does anything.
pub(super) async fn json_only(req: Request, next: Next) -> Response {
    if is_json(req.headers()) {
        next.run(req).await
    } else {
        error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "this needs Content-Type: application/json",
        )
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// The relying party for `public_url`: its host is the RP id, and the whole
/// origin the one passkeys may be used from.
fn site(public_url: &str) -> Result<Site, String> {
    let url = Url::parse(public_url).map_err(|e| format!("it is not a URL ({e})"))?;
    let host = url
        .host_str()
        .filter(|_| url.domain().is_some())
        .ok_or("it needs a domain name; passkeys cannot be bound to an IP address")?;
    // A browser does not take `recall.example.com.` for the same site as
    // `recall.example.com`, so a passkey registered at one would not work
    // at the other.
    if host.ends_with('.') {
        return Err("its host ends with a dot; leave the dot off".to_string());
    }
    if !host.contains('.') && host != "localhost" {
        return Err(format!(
            "{host:?} is not a full domain name; use the one people reach the server at, \
             such as recall.example.com"
        ));
    }
    // Browsers allow WebAuthn over plain HTTP only on localhost, which is
    // what trying the server on a laptop needs.
    match url.scheme() {
        "https" => {}
        "http" if host == "localhost" => {}
        _ => return Err("it must start with https://".to_string()),
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err("it must be an origin only, with no path, such as \
                    https://recall.example.com"
            .to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("it must not carry a user name or password".to_string());
    }
    // Neither `allow_subdomains` nor `allow_any_port`: an answer made at
    // any other origin than this exact one is refused.
    let webauthn = WebauthnBuilder::new(host, &url)
        .and_then(|b| b.rp_name("Recall").timeout(CEREMONY_TTL).build())
        .map_err(|e| format!("webauthn-rs refused it ({e})"))?;
    Ok(Site {
        webauthn,
        origin: url.origin().ascii_serialization(),
    })
}

/// What to warn about a `RECALL_PUBLIC_URL` that works, but only from a
/// browser on the server's own machine.
fn local_only(public_url: &str) -> Option<String> {
    let url = Url::parse(public_url).ok()?;
    (url.scheme() == "http").then(|| {
        format!(
            "RECALL_PUBLIC_URL is {public_url}: passkeys will work only from a browser on this \
             machine. For a server reached from anywhere else, set it to its https:// address."
        )
    })
}

fn random<const N: usize>() -> Result<[u8; N], Refusal> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("no randomness: {e}")))?;
    Ok(buf)
}

/// A passkey as the API shows it.
#[derive(Debug, Serialize)]
struct PasskeyView {
    id: String,
    name: String,
    created_at: String,
    last_used_at: Option<String>,
    /// Whether the session asking signed in with it.
    current: bool,
}

impl PasskeyView {
    fn of(c: AdminCredential, current: Option<&str>) -> Self {
        Self {
            current: current == Some(c.id.as_str()),
            id: c.id,
            name: c.name,
            created_at: c.created_at,
            last_used_at: c.last_used_at,
        }
    }
}

#[derive(Debug, Serialize)]
struct PasskeyList {
    passkeys: Vec<PasskeyView>,
}

/// A challenge, and the id to answer it under.
#[derive(Debug, Serialize)]
struct Started {
    ceremony_id: String,
    /// What `navigator.credentials.create()` or `.get()` is given, in
    /// WebAuthn's JSON form: binary values are base64url.
    options: Value,
}

#[derive(Debug, Deserialize)]
struct StartBootstrap {
    #[serde(default)]
    bootstrap_code: String,
}

#[derive(Debug, Deserialize)]
struct FinishRegistration {
    ceremony_id: String,
    #[serde(default)]
    name: String,
    credential: RegisterPublicKeyCredential,
}

#[derive(Debug, Deserialize)]
struct FinishSignIn {
    ceremony_id: String,
    credential: PublicKeyCredential,
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &Bytes) -> Result<T, Refusal> {
    serde_json::from_slice(bytes)
        .map_err(|e| Refusal::new(StatusCode::BAD_REQUEST, format!("invalid json body: {e}")))
}

fn started(ceremony_id: String, options: Value) -> Response {
    no_store(json(
        StatusCode::OK,
        &Started {
            ceremony_id,
            options,
        },
    ))
}

fn now_at(state: &AppState) -> (OffsetDateTime, String) {
    let at = state.clock();
    (at, format_timestamp(at))
}

/// The registration challenge, asking for a discoverable credential.
///
/// `webauthn-rs` asks for `residentKey: discouraged`, since a security key
/// has little room to keep credentials. Signing in here is usernameless,
/// though, which only a discoverable credential can do, so the browser is
/// asked for one. The server's check of the answer does not change: it
/// never depended on this.
fn registration_options(
    site: &Site,
    user_handle: Uuid,
    exclude: Vec<Passkey>,
) -> Result<(Value, PasskeyRegistration), Refusal> {
    let exclude =
        (!exclude.is_empty()).then(|| exclude.iter().map(|p| p.cred_id().clone()).collect());
    let (challenge, registration) = site
        .webauthn
        .start_passkey_registration(user_handle, USER_NAME, USER_DISPLAY_NAME, exclude)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("starting a registration: {e}")))?;
    let mut options =
        serde_json::to_value(challenge).map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
    if let Some(selection) = options.pointer_mut("/publicKey/authenticatorSelection") {
        if let Some(selection) = selection.as_object_mut() {
            selection.insert("residentKey".into(), Value::from("required"));
            selection.insert("requireResidentKey".into(), Value::from(true));
        }
    }
    Ok((options, registration))
}

fn stored_passkeys(state: &AppState) -> Result<Vec<(AdminCredential, Passkey)>, Refusal> {
    let credentials = state.store.admin_credentials().map_err(Refusal::internal)?;
    credentials
        .into_iter()
        .map(|c| {
            let passkey = serde_json::from_str(&c.passkey).map_err(|e| {
                Refusal::internal(anyhow::anyhow!("passkey {} does not parse: {e}", c.id))
            })?;
            Ok((c, passkey))
        })
        .collect()
}

fn credential_id(passkey: &Passkey) -> String {
    URL_SAFE_NO_PAD.encode(passkey.cred_id().as_ref() as &[u8])
}

/// The signature counter a passkey was registered with, which is where the
/// store's copy of it starts. `webauthn-rs` keeps it inside the passkey and
/// offers no accessor for it, so it is read from the JSON the store keeps.
fn registered_counter(passkey_json: &Value) -> Result<u32, Refusal> {
    passkey_json
        .pointer("/cred/counter")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            Refusal::internal(anyhow::anyhow!(
                "a registered passkey's JSON has no /cred/counter; has webauthn-rs changed its \
                 format?"
            ))
        })
}

/// The counter an assertion carries: bytes 33 to 36 of its authenticator
/// data, after the RP id hash and the flags. Only for saying what was
/// refused; `webauthn-rs` reads it for the check itself.
fn asserted_counter(credential: &PublicKeyCredential) -> Option<u32> {
    let data: &[u8] = credential.response.authenticator_data.as_ref();
    Some(u32::from_be_bytes(data.get(33..37)?.try_into().ok()?))
}

fn passkey_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    if !displayable(name, MAX_NAME_CHARS) {
        return Err(Refusal::new(
            StatusCode::BAD_REQUEST,
            "name must be at most 64 characters, with no control, format or invisible characters",
        ));
    }
    Ok(if name.is_empty() {
        "passkey".to_string()
    } else {
        name.to_string()
    })
}

fn not_registered(e: WebauthnError) -> Refusal {
    Refusal::new(
        StatusCode::BAD_REQUEST,
        format!("the passkey's answer did not verify: {e}"),
    )
}

/// Finishes a registration and stores the passkey. `first` is the
/// bootstrap code's hash, for the first passkey.
fn register(
    state: &AppState,
    ceremony: &Ceremony,
    registration: &PasskeyRegistration,
    user_handle: Uuid,
    finish: &FinishRegistration,
    first: Option<&str>,
) -> Result<AdminCredential, Refusal> {
    let site = state.passkeys.site()?;
    let name = passkey_name(&finish.name)?;
    // What the browser says, unsigned, so only ever a reason to refuse: an
    // authenticator that kept no discoverable credential could never sign
    // in here, where nobody types a user name.
    let kept = finish
        .credential
        .extensions
        .cred_props
        .as_ref()
        .and_then(|p| p.rk);
    if kept == Some(false) {
        return Err(Refusal::new(
            StatusCode::BAD_REQUEST,
            "this authenticator did not keep the passkey on itself (the browser says credProps.rk \
             is false), and signing in here needs one that does; use a phone, or a password \
             manager that saves passkeys",
        ));
    }
    let passkey = site
        .webauthn
        .finish_passkey_registration(&finish.credential, registration)
        .map_err(not_registered)?;
    state.passkeys.finish(ceremony, state.now())?;
    let passkey_json =
        serde_json::to_value(&passkey).map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
    let sign_count = registered_counter(&passkey_json)?;
    let id = credential_id(&passkey);
    let (_, now) = now_at(state);
    let added = state
        .store
        .add_admin_credential(
            &NewAdminCredential {
                id: &id,
                user_handle: &user_handle.to_string(),
                name: &name,
                passkey: &passkey_json.to_string(),
                sign_count,
                created_at: &now,
            },
            first.map(|code_sha256| FirstPasskey {
                code_sha256,
                now: &now,
            }),
        )
        .map_err(Refusal::internal)?;
    match added {
        AddedCredential::Added => {}
        AddedCredential::NotFirst => return Err(already_bootstrapped()),
        AddedCredential::Code(refused) => return Err(code_refused(refused)),
        AddedCredential::Duplicate => {
            return Err(Refusal::new(
                StatusCode::CONFLICT,
                "that passkey is registered already",
            ))
        }
    }
    state
        .store
        .admin_credential(&id)
        .map_err(Refusal::internal)?
        .ok_or_else(|| Refusal::internal(anyhow::anyhow!("passkey {id} vanished")))
}

fn already_bootstrapped() -> Refusal {
    Refusal::new(
        StatusCode::FORBIDDEN,
        "forbidden: a passkey is registered already, and RECALL_TOKEN cannot register \
         another; sign in with the passkey to add more",
    )
}

fn code_refused(why: BootstrapCode) -> Refusal {
    Refusal::new(
        StatusCode::FORBIDDEN,
        match why {
            BootstrapCode::Expired => {
                "forbidden: that bootstrap code has expired; restart the server, or run \
                 recall-server reset-passkeys where it runs, for a new one"
            }
            _ => {
                "forbidden: registering the first passkey needs the one-time bootstrap code the \
                 server printed where it runs, as well as RECALL_TOKEN"
            }
        },
    )
}

/// The bootstrap routes: the operator's token, and only while no passkey
/// exists. Checked before anything else, whatever else the request says.
fn bootstrap_allowed(state: &AppState, caller: &Caller) -> Result<(), Refusal> {
    if *caller != Caller::Operator {
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "forbidden: registering the first passkey needs RECALL_TOKEN",
        ));
    }
    match state.store.has_admin_credentials() {
        Ok(false) => Ok(()),
        Ok(true) => Err(already_bootstrapped()),
        Err(e) => Err(Refusal::internal(e)),
    }
}

/// `POST /admin/bootstrap/register`, with `{"bootstrap_code": "…"}`.
pub(super) async fn handle_bootstrap_start(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    bytes: Bytes,
) -> Response {
    let run = || -> Result<Response, Refusal> {
        bootstrap_allowed(&state, &caller)?;
        let site = state.passkeys.site()?;
        let typed = if bytes.is_empty() {
            String::new()
        } else {
            parse::<StartBootstrap>(&bytes)?.bootstrap_code
        };
        let code_sha256 =
            crate::bootstrap::sha256(&typed).ok_or_else(|| code_refused(BootstrapCode::Wrong))?;
        let (_, now) = now_at(&state);
        match state
            .store
            .check_bootstrap_code(&code_sha256, &now)
            .map_err(Refusal::internal)?
        {
            BootstrapCode::Valid => {}
            refused => return Err(code_refused(refused)),
        }
        let user_handle = Uuid::from_bytes(random::<16>()?);
        let (options, registration) = registration_options(site, user_handle, Vec::new())?;
        let id = state.passkeys.begin(
            Pending::Bootstrap {
                registration,
                user_handle,
                code_sha256,
            },
            state.now(),
        )?;
        Ok(started(id, options))
    };
    run().unwrap_or_else(IntoResponse::into_response)
}

/// `POST /admin/bootstrap/register/finish`.
pub(super) async fn handle_bootstrap_finish(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    bytes: Bytes,
) -> Response {
    let run = || -> Result<Response, Refusal> {
        bootstrap_allowed(&state, &caller)?;
        let finish: FinishRegistration = parse(&bytes)?;
        let ceremony = state.passkeys.open(&finish.ceremony_id, state.now())?;
        let Pending::Bootstrap {
            registration,
            user_handle,
            code_sha256,
        } = &ceremony.pending
        else {
            return Err(wrong_ceremony());
        };
        let credential = register(
            &state,
            &ceremony,
            registration,
            *user_handle,
            &finish,
            Some(code_sha256),
        )?;
        Ok(json(StatusCode::OK, &PasskeyView::of(credential, None)))
    };
    run().unwrap_or_else(IntoResponse::into_response)
}

fn wrong_ceremony() -> Refusal {
    Refusal::new(
        StatusCode::BAD_REQUEST,
        "that ceremony is not one this route finishes; start again",
    )
}

/// `POST /admin/login/start`.
pub(super) async fn handle_sign_in_start(State(state): State<Arc<AppState>>) -> Response {
    let run = || -> Result<Response, Refusal> {
        let site = state.passkeys.site()?;
        if !state
            .store
            .has_admin_credentials()
            .map_err(Refusal::internal)?
        {
            return Err(Refusal::new(
                StatusCode::CONFLICT,
                "no passkey is registered yet; register the first with RECALL_TOKEN",
            ));
        }
        let (mut challenge, authentication) = site
            .webauthn
            .start_discoverable_authentication()
            .map_err(|e| Refusal::internal(anyhow::anyhow!("starting a sign-in: {e}")))?;
        // webauthn-rs asks for conditional mediation, which offers the
        // passkey in a text field's autofill. The page signs in from a
        // button instead, which works the same on every phone.
        challenge.mediation = None;
        let options = serde_json::to_value(challenge)
            .map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
        let id = state
            .passkeys
            .begin(Pending::SignIn(authentication), state.now())?;
        Ok(started(id, options))
    };
    run().unwrap_or_else(IntoResponse::into_response)
}

fn refused_sign_in(why: &str) -> Refusal {
    Refusal::new(StatusCode::UNAUTHORIZED, format!("unauthorized: {why}"))
}

const COUNTER_WENT_BACK: &str = "this passkey's signature counter did not move forward, which \
     can mean a copy of it exists; sign-in refused";

/// Refuses a sign-in whose counter did not move forward, and says so in the
/// server's log: it is the one sign-in failure that can mean a passkey was
/// copied, which the owner would want to know about.
fn counter_went_back(stored: &AdminCredential, credential: &PublicKeyCredential) -> Refusal {
    let asserted = asserted_counter(credential)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unreadable".to_string());
    eprintln!(
        "admin sign-in refused: the passkey {:?} ({}) answered with signature counter {asserted}, \
         which is not past the {} already seen. A copy of it may exist.",
        stored.name, stored.id, stored.sign_count
    );
    refused_sign_in(COUNTER_WENT_BACK)
}

/// Verifies a sign-in, records it, and starts a session.
fn sign_in(state: &AppState, finish: &FinishSignIn) -> Result<(String, SessionView), Refusal> {
    let site = state.passkeys.site()?;
    let ceremony = state.passkeys.open(&finish.ceremony_id, state.now())?;
    let Pending::SignIn(authentication) = &ceremony.pending else {
        return Err(wrong_ceremony());
    };
    let (user_handle, raw_id) = site
        .webauthn
        .identify_discoverable_authentication(&finish.credential)
        .map_err(|_| refused_sign_in("the passkey did not say which account it is for"))?;
    let id = URL_SAFE_NO_PAD.encode(raw_id);
    let stored = state
        .store
        .admin_credential(&id)
        .map_err(Refusal::internal)?
        .ok_or_else(|| refused_sign_in("that passkey is not registered here"))?;
    if stored.user_handle != user_handle.to_string() {
        return Err(refused_sign_in("that passkey is not registered here"));
    }
    let mut passkey: Passkey = serde_json::from_str(&stored.passkey)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("passkey {id} does not parse: {e}")))?;
    let result = site
        .webauthn
        .finish_discoverable_authentication(
            &finish.credential,
            authentication.clone(),
            &[DiscoverableKey::from(&passkey)],
        )
        .map_err(|e| match e {
            // webauthn-rs compares the counter too, against the passkey it
            // was handed; the store's check below is the one that holds when
            // two sign-ins race.
            WebauthnError::CredentialPossibleCompromise => {
                counter_went_back(&stored, &finish.credential)
            }
            e => refused_sign_in(&format!("the passkey's answer did not verify: {e}")),
        })?;
    state.passkeys.finish(&ceremony, state.now())?;
    passkey.update_credential(&result);
    let passkey_json =
        serde_json::to_string(&passkey).map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
    let (at, now) = now_at(state);
    let recorded = state
        .store
        .record_admin_sign_in(&id, result.counter(), &passkey_json, &now)
        .map_err(Refusal::internal)?;
    if !recorded {
        return Err(counter_went_back(&stored, &finish.credential));
    }

    let token = URL_SAFE_NO_PAD.encode(random::<32>()?);
    let expires_at = format_timestamp(at + SESSION_LIMIT);
    let created = state
        .store
        .create_admin_session(&recall_wire::content_sha256(&token), &id, &now, &expires_at)
        .map_err(Refusal::internal)?;
    if !created {
        return Err(refused_sign_in("that passkey was removed as it signed in"));
    }
    Ok((
        token.clone(),
        SessionView {
            csrf_token: csrf_token(&token),
            expires_at,
            idle_expires_at: format_timestamp(at + SESSION_IDLE),
        },
    ))
}

/// `POST /admin/login/finish`: the session cookie, and the CSRF token the
/// page sends back with everything that changes state.
pub(super) async fn handle_sign_in_finish(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let finish: FinishSignIn = match parse(&bytes) {
        Ok(finish) => finish,
        Err(refused) => return refused.into_response(),
    };
    match sign_in(&state, &finish) {
        Ok((token, view)) => {
            // Signing in again from a browser that has a session, as the
            // page does before something that needs a recent sign-in,
            // replaces that session rather than leaving it to idle out.
            if let Some(previous) = session_token_sha256(&headers) {
                if let Err(e) = state.store.delete_admin_session(&previous) {
                    eprintln!("ending the session a sign-in replaced: {e:#}");
                }
            }
            let mut resp = no_store(json(StatusCode::OK, &view));
            resp.headers_mut()
                .insert(header::SET_COOKIE, session_cookie(&token, SESSION_LIMIT));
            resp
        }
        Err(refused) => refused.into_response(),
    }
}

/// `POST /admin/logout`.
pub(super) async fn handle_sign_out(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<OwnerSession>,
) -> Response {
    if let Err(e) = state.store.delete_admin_session(&session.token_sha256) {
        return internal(e);
    }
    let mut resp = json(StatusCode::OK, &serde_json::json!({ "signed_out": true }));
    resp.headers_mut()
        .insert(header::SET_COOKIE, cleared_cookie());
    resp
}

/// `POST /admin/logout/others`: every session but this one, such as one
/// signed in somewhere the owner no longer is.
pub(super) async fn handle_sign_out_others(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<OwnerSession>,
) -> Response {
    if let Err(refused) = require_recent_sign_in(&state, &session) {
        return refused.into_response();
    }
    match state
        .store
        .delete_other_admin_sessions(&session.token_sha256)
    {
        Ok(n) => json(
            StatusCode::OK,
            &serde_json::json!({ "other_sessions_ended": n }),
        ),
        Err(e) => internal(e),
    }
}

/// `GET /admin/passkeys`.
pub(super) async fn handle_list_passkeys(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<OwnerSession>,
) -> Response {
    match state.store.admin_credentials() {
        Ok(credentials) => json(
            StatusCode::OK,
            &PasskeyList {
                passkeys: credentials
                    .into_iter()
                    .map(|c| PasskeyView::of(c, Some(&session.credential_id)))
                    .collect(),
            },
        ),
        Err(e) => internal(e),
    }
}

/// `POST /admin/passkeys/register`: another passkey, for the same owner.
pub(super) async fn handle_add_start(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<OwnerSession>,
) -> Response {
    let run = || -> Result<Response, Refusal> {
        require_recent_sign_in(&state, &session)?;
        let site = state.passkeys.site()?;
        let existing = stored_passkeys(&state)?;
        // Every passkey has the owner's one user handle, so an
        // authenticator that already holds one is excluded rather than
        // asked to replace it.
        let user_handle = existing
            .first()
            .and_then(|(c, _)| Uuid::parse_str(&c.user_handle).ok())
            .ok_or_else(|| Refusal::internal(anyhow::anyhow!("no passkey to add to")))?;
        let exclude = existing.into_iter().map(|(_, p)| p).collect();
        let (options, registration) = registration_options(site, user_handle, exclude)?;
        let id = state.passkeys.begin(
            Pending::Add {
                registration,
                user_handle,
                session: session.token_sha256.clone(),
            },
            state.now(),
        )?;
        Ok(started(id, options))
    };
    run().unwrap_or_else(IntoResponse::into_response)
}

/// `POST /admin/passkeys/register/finish`.
pub(super) async fn handle_add_finish(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<OwnerSession>,
    bytes: Bytes,
) -> Response {
    let run = || -> Result<Response, Refusal> {
        let finish: FinishRegistration = parse(&bytes)?;
        let ceremony = state.passkeys.open(&finish.ceremony_id, state.now())?;
        let Pending::Add {
            registration,
            user_handle,
            session: started_by,
        } = &ceremony.pending
        else {
            return Err(wrong_ceremony());
        };
        if *started_by != session.token_sha256 {
            return Err(wrong_ceremony());
        }
        let credential = register(&state, &ceremony, registration, *user_handle, &finish, None)?;
        Ok(json(
            StatusCode::OK,
            &PasskeyView::of(credential, Some(&session.credential_id)),
        ))
    };
    run().unwrap_or_else(IntoResponse::into_response)
}

/// `POST /admin/passkeys/{id}/remove`: any but the last. The sessions it
/// signed in end with it, this one included if it is this one's.
pub(super) async fn handle_remove(
    State(state): State<Arc<AppState>>,
    Extension(session): Extension<OwnerSession>,
    Path(id): Path<String>,
) -> Response {
    if let Err(refused) = require_recent_sign_in(&state, &session) {
        return refused.into_response();
    }
    match state.store.remove_admin_credential(&id) {
        Ok(RemovedCredential::Removed(credential)) => {
            let own = credential.id == session.credential_id;
            let mut resp = json(
                StatusCode::OK,
                &PasskeyView::of(credential, Some(&session.credential_id)),
            );
            if own {
                resp.headers_mut()
                    .insert(header::SET_COOKIE, cleared_cookie());
            }
            resp
        }
        Ok(RemovedCredential::Last) => error(
            StatusCode::CONFLICT,
            "that is the only passkey; add another before removing it",
        ),
        Ok(RemovedCredential::NotFound) => error(StatusCode::NOT_FOUND, "no passkey has that id"),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_url_must_be_an_https_origin() {
        let ok = site("https://recall.example.com").ok().unwrap();
        assert_eq!(ok.origin, "https://recall.example.com");
        assert_eq!(
            site("https://recall.example.com:8443/")
                .ok()
                .unwrap()
                .origin,
            "https://recall.example.com:8443"
        );
        assert!(site("http://localhost:8787").is_ok(), "trying it locally");
        assert!(site("https://localhost").is_ok());
        for bad in [
            "recall.example.com",
            "http://recall.example.com",
            "https://recall.example.com/admin",
            "https://recall.example.com/?x=1",
            "https://203.0.113.9",
            "https://user:pw@recall.example.com",
            "ftp://recall.example.com",
            // Verification finding 6b: a trailing dot is another site to a
            // browser, and a single label is nobody's public address.
            "https://recall.example.com.",
            "http://localhost.:8787",
            "https://recall",
            "https://intranet:8443",
        ] {
            assert!(site(bad).is_err(), "{bad}");
        }
        assert!(site("https://recall.example.com.")
            .err()
            .unwrap()
            .contains("ends with a dot"));
        assert!(site("https://recall")
            .err()
            .unwrap()
            .contains("not a full domain name"));
    }

    #[test]
    fn plain_http_on_localhost_works_with_a_warning() {
        assert!(local_only("http://localhost:8787")
            .unwrap()
            .contains("only from a browser on this machine"));
        assert_eq!(local_only("https://recall.example.com"), None);
    }

    #[test]
    fn off_says_why() {
        let off = Passkeys::new("");
        let status = off.status();
        assert!(!status.enabled);
        assert!(status
            .reason
            .unwrap()
            .contains("RECALL_PUBLIC_URL is not set"));
        let bad = Passkeys::new("http://recall.example.com");
        assert!(bad.status().reason.unwrap().contains("https://"));
        assert!(Passkeys::new("https://recall.example.com").status().enabled);
    }

    /// A ceremony opens where it was sealed, until it expires, and until it
    /// is finished; a changed byte, another process's key or a finished
    /// ceremony is the one refusal.
    #[test]
    fn a_sealed_ceremony_opens_once_here_and_nowhere_else() {
        let p = Passkeys::new("https://recall.example.com");
        let site = p.site().ok().unwrap();
        let (_, auth) = site.webauthn.start_discoverable_authentication().unwrap();
        let now = 1_000_000;
        let id = p.begin(Pending::SignIn(auth), now).ok().unwrap();
        assert!(id.starts_with("cer_"));

        let ceremony = p.open(&id, now).ok().unwrap();
        assert!(matches!(ceremony.pending, Pending::SignIn(_)));
        assert!(p.open(&id, now + 299).is_ok(), "within five minutes");
        assert!(p.open(&id, now + 300).is_err(), "expired");

        // Tampered: any byte of the sealed part.
        let mut bytes = URL_SAFE_NO_PAD.decode(&id[4..]).unwrap();
        bytes[NONCE_BYTES + 3] ^= 1;
        let tampered = format!("cer_{}", URL_SAFE_NO_PAD.encode(&bytes));
        assert!(p.open(&tampered, now).is_err());
        // Another process, with its own key.
        let other = Passkeys::new("https://recall.example.com");
        assert!(other.open(&id, now).is_err());
        for junk in ["", "cer_", "cer_nope", "nope", &id[4..]] {
            assert!(p.open(junk, now).is_err(), "{junk:?}");
        }

        // Finished once, it opens no more, and cannot be finished again.
        assert!(p.finish(&ceremony, now).is_ok());
        assert!(p.finish(&ceremony, now).is_err());
        assert!(p.open(&id, now).is_err());
        assert_eq!(p.prune(now), 0, "not expired yet");
        assert_eq!(p.prune(now + 300), 1);
    }

    /// Starting a sign-in keeps nothing: a million strangers could start one
    /// each and the server would hold what it held before.
    #[test]
    fn starting_a_ceremony_holds_nothing() {
        let p = Passkeys::new("https://recall.example.com");
        let site = p.site().ok().unwrap();
        for _ in 0..100 {
            let (_, auth) = site.webauthn.start_discoverable_authentication().unwrap();
            p.begin(Pending::SignIn(auth), 0).ok().unwrap();
        }
        assert!(p.lock().is_empty());
    }

    #[test]
    fn only_json_is_json() {
        let with = |ct: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::CONTENT_TYPE, ct.parse().unwrap());
            is_json(&h)
        };
        assert!(with("application/json"));
        assert!(with("Application/JSON; charset=utf-8"));
        for no in [
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data",
            "application/jsonx",
            "text/plain; application/json",
        ] {
            assert!(!with(no), "{no}");
        }
        assert!(!is_json(&HeaderMap::new()));
    }

    #[test]
    fn the_counter_is_read_where_webauthn_rs_keeps_it() {
        let json = serde_json::json!({ "cred": { "counter": 7 } });
        assert_eq!(registered_counter(&json).ok(), Some(7));
        assert!(registered_counter(&serde_json::json!({})).is_err());
    }
}
