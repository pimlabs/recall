//! Passkey (WebAuthn) sign-in for `/admin`, with `webauthn-rs`.
//!
//! Three ceremonies, each started by one request and finished by the next:
//!
//! - **Bootstrap.** The operator's `RECALL_TOKEN` registers the first
//!   passkey, and only the first: once one exists, the route refuses
//!   whatever token it is shown, so a token that leaks later cannot
//!   register a "first" passkey of its own.
//! - **Sign-in.** Anyone may start one; only a registered passkey finishes
//!   it. It is discoverable (usernameless): there is one owner, so the page
//!   asks for no name and the authenticator offers the passkey it holds.
//! - **Adding a passkey.** Only a signed-in owner, never the token.
//!
//! A ceremony's state stays in memory, keyed by a random id the page sends
//! back, for five minutes, and is used once. It is never sent to the
//! browser, which is what `webauthn-rs` asks of it: a challenge the client
//! could choose or replay would defeat the ceremony.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use recall_wire::devices::displayable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use webauthn_rs::prelude::{
    DiscoverableAuthentication, DiscoverableKey, Passkey, PasskeyRegistration,
    PublicKeyCredential, RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder,
    WebauthnError,
};

use super::admin::{
    cleared_cookie, csrf_token, no_store, session_cookie, OwnerSession, PasskeyStatus,
    SessionView, SESSION_IDLE, SESSION_LIMIT,
};
use super::auth::Caller;
use super::middleware::ClientIp;
use super::respond::{error, internal, json, Refusal};
use super::AppState;
use crate::format_timestamp;
use crate::store::{AddedCredential, AdminCredential, NewAdminCredential, RemovedCredential};

/// How long a ceremony may take, from its challenge to its answer. Also
/// the timeout the browser is given: long enough to find a phone, unlock
/// it and answer, per WebAuthn's own recommendation.
const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);

/// How many ceremonies may be in flight at once. Starting a sign-in needs
/// no credential, so this bounds what strangers can make the server hold:
/// each is a few hundred bytes.
const MAX_CEREMONIES: usize = 4096;

/// How many of those one address may have, as the rate limiter keys it,
/// so one address cannot take all of them and keep the owner from signing
/// in.
const MAX_CEREMONIES_PER_ADDRESS: usize = 8;

/// The longest name a passkey may be given.
const MAX_NAME_CHARS: usize = 64;

/// What the authenticator shows the passkey as. There is one owner.
const USER_NAME: &str = "owner";
const USER_DISPLAY_NAME: &str = "Recall owner";

/// What happens next in a ceremony, and whose it is.
enum Pending {
    /// Registering the first passkey, with the token.
    Bootstrap {
        registration: PasskeyRegistration,
        user_handle: Uuid,
    },
    /// Registering another, from a session.
    Add {
        registration: PasskeyRegistration,
        user_handle: Uuid,
        /// The session that started it, which alone may finish it.
        session: String,
    },
    /// Signing in.
    SignIn(DiscoverableAuthentication),
}

struct Ceremony {
    pending: Pending,
    client_ip: String,
    expires: Instant,
}

/// The site passkeys are bound to.
struct Site {
    webauthn: Webauthn,
    origin: String,
}

/// Passkey sign-in: the relying party, when `RECALL_PUBLIC_URL` gives
/// one, and the ceremonies in flight.
pub(super) struct Passkeys {
    site: Result<Site, String>,
    ceremonies: Mutex<HashMap<String, Ceremony>>,
}

impl Passkeys {
    /// Passkey sign-in for the site `public_url` names, or off, with the
    /// reason, when it names none.
    pub(super) fn new(public_url: &str) -> Self {
        let site = if public_url.is_empty() {
            Err("RECALL_PUBLIC_URL is not set on the server, so passkey sign-in is off. \
                 Set it to the address this page is served from, such as \
                 https://recall.example.com, and restart the server."
                .to_string())
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
        Self {
            site,
            ceremonies: Mutex::new(HashMap::new()),
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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Ceremony>> {
        self.ceremonies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets every ceremony whose time is up. Answers how many went.
    pub(super) fn prune(&self) -> usize {
        let now = Instant::now();
        let mut ceremonies = self.lock();
        let before = ceremonies.len();
        ceremonies.retain(|_, c| c.expires > now);
        before - ceremonies.len()
    }

    /// Remembers a ceremony, and answers the id the page finishes it with.
    fn begin(&self, pending: Pending, client_ip: &str) -> Result<String, Refusal> {
        let id = format!("cer_{}", URL_SAFE_NO_PAD.encode(random::<16>()?));
        let now = Instant::now();
        let mut ceremonies = self.lock();
        ceremonies.retain(|_, c| c.expires > now);
        if ceremonies.len() >= MAX_CEREMONIES {
            return Err(Refusal::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many sign-ins are in progress, try again in a few minutes",
            ));
        }
        let from_here = ceremonies
            .values()
            .filter(|c| c.client_ip == client_ip)
            .count();
        if from_here >= MAX_CEREMONIES_PER_ADDRESS {
            return Err(Refusal::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many sign-ins from this address are in progress, \
                 try again in a few minutes",
            ));
        }
        ceremonies.insert(
            id.clone(),
            Ceremony {
                pending,
                client_ip: client_ip.to_string(),
                expires: now + CEREMONY_TTL,
            },
        );
        Ok(id)
    }

    /// Takes a ceremony out, so it can be finished at most once, whether
    /// or not finishing it succeeds.
    fn take(&self, id: &str) -> Result<Pending, Refusal> {
        match self.lock().remove(id) {
            Some(c) if c.expires > Instant::now() => Ok(c.pending),
            _ => Err(Refusal::new(
                StatusCode::BAD_REQUEST,
                "this ceremony has expired or was already used; start again",
            )),
        }
    }
}

/// The relying party for `public_url`: its host is the RP id, and the whole
/// origin the one passkeys may be used from.
fn site(public_url: &str) -> Result<Site, String> {
    let url = Url::parse(public_url).map_err(|e| format!("it is not a URL ({e})"))?;
    let host = url
        .host_str()
        .filter(|_| url.domain().is_some())
        .ok_or("it needs a domain name; passkeys cannot be bound to an IP address")?;
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
    let webauthn = WebauthnBuilder::new(host, &url)
        .and_then(|b| b.rp_name("Recall").timeout(CEREMONY_TTL).build())
        .map_err(|e| format!("webauthn-rs refused it ({e})"))?;
    Ok(Site {
        webauthn,
        origin: url.origin().ascii_serialization(),
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
    serde_json::from_slice(bytes).map_err(|e| {
        Refusal::new(
            StatusCode::BAD_REQUEST,
            format!("invalid json body: {e}"),
        )
    })
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
    let exclude = (!exclude.is_empty()).then(|| exclude.iter().map(|p| p.cred_id().clone()).collect());
    let (challenge, registration) = site
        .webauthn
        .start_passkey_registration(user_handle, USER_NAME, USER_DISPLAY_NAME, exclude)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("starting a registration: {e}")))?;
    let mut options = serde_json::to_value(challenge)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
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

/// Finishes a registration and stores the passkey.
fn register(
    state: &AppState,
    registration: &PasskeyRegistration,
    user_handle: Uuid,
    finish: &FinishRegistration,
    first_only: bool,
) -> Result<AdminCredential, Refusal> {
    let site = state.passkeys.site()?;
    let name = passkey_name(&finish.name)?;
    let passkey = site
        .webauthn
        .finish_passkey_registration(&finish.credential, registration)
        .map_err(not_registered)?;
    let passkey_json = serde_json::to_string(&passkey)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
    let id = credential_id(&passkey);
    let (_, now) = now_at(state);
    let added = state
        .store
        .add_admin_credential(
            &NewAdminCredential {
                id: &id,
                user_handle: &user_handle.to_string(),
                name: &name,
                passkey: &passkey_json,
                sign_count: 0,
                created_at: &now,
            },
            first_only,
        )
        .map_err(Refusal::internal)?;
    match added {
        AddedCredential::Added => {}
        AddedCredential::NotFirst => return Err(already_bootstrapped()),
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

/// `POST /admin/bootstrap/register`.
pub(super) async fn handle_bootstrap_start(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
) -> Response {
    let run = || -> Result<Response, Refusal> {
        bootstrap_allowed(&state, &caller)?;
        let site = state.passkeys.site()?;
        let user_handle = Uuid::from_bytes(random::<16>()?);
        let (options, registration) = registration_options(site, user_handle, Vec::new())?;
        let id = state.passkeys.begin(
            Pending::Bootstrap {
                registration,
                user_handle,
            },
            &client_ip,
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
        let Pending::Bootstrap {
            registration,
            user_handle,
        } = state.passkeys.take(&finish.ceremony_id)?
        else {
            return Err(wrong_ceremony());
        };
        let credential = register(&state, &registration, user_handle, &finish, true)?;
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
pub(super) async fn handle_sign_in_start(
    State(state): State<Arc<AppState>>,
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
) -> Response {
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
            .begin(Pending::SignIn(authentication), &client_ip)?;
        Ok(started(id, options))
    };
    run().unwrap_or_else(IntoResponse::into_response)
}

fn refused_sign_in(why: &str) -> Refusal {
    Refusal::new(StatusCode::UNAUTHORIZED, format!("unauthorized: {why}"))
}

const COUNTER_WENT_BACK: &str = "this passkey's signature counter did not move forward, which \
     can mean a copy of it exists; sign-in refused";

/// Verifies a sign-in, records it, and starts a session.
fn sign_in(state: &AppState, finish: &FinishSignIn) -> Result<(String, SessionView), Refusal> {
    let site = state.passkeys.site()?;
    let Pending::SignIn(authentication) = state.passkeys.take(&finish.ceremony_id)? else {
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
            authentication,
            &[DiscoverableKey::from(&passkey)],
        )
        .map_err(|e| match e {
            // webauthn-rs compares the counter too, against the passkey it
            // was handed; the store's check below is the one that holds when
            // two sign-ins race.
            WebauthnError::CredentialPossibleCompromise => refused_sign_in(COUNTER_WENT_BACK),
            e => refused_sign_in(&format!("the passkey's answer did not verify: {e}")),
        })?;
    passkey.update_credential(&result);
    let passkey_json = serde_json::to_string(&passkey)
        .map_err(|e| Refusal::internal(anyhow::anyhow!("{e}")))?;
    let (at, now) = now_at(state);
    let recorded = state
        .store
        .record_admin_sign_in(&id, result.counter(), &passkey_json, &now)
        .map_err(Refusal::internal)?;
    if !recorded {
        return Err(refused_sign_in(COUNTER_WENT_BACK));
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
    bytes: Bytes,
) -> Response {
    let finish: FinishSignIn = match parse(&bytes) {
        Ok(finish) => finish,
        Err(refused) => return refused.into_response(),
    };
    match sign_in(&state, &finish) {
        Ok((token, view)) => {
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
    Extension(ClientIp(client_ip)): Extension<ClientIp>,
) -> Response {
    let run = || -> Result<Response, Refusal> {
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
            &client_ip,
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
        let Pending::Add {
            registration,
            user_handle,
            session: started_by,
        } = state.passkeys.take(&finish.ceremony_id)?
        else {
            return Err(wrong_ceremony());
        };
        if started_by != session.token_sha256 {
            return Err(wrong_ceremony());
        }
        let credential = register(&state, &registration, user_handle, &finish, false)?;
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
            site("https://recall.example.com:8443/").ok().unwrap().origin,
            "https://recall.example.com:8443"
        );
        assert!(site("http://localhost:8787").is_ok(), "trying it locally");
        for bad in [
            "recall.example.com",
            "http://recall.example.com",
            "https://recall.example.com/admin",
            "https://recall.example.com/?x=1",
            "https://203.0.113.9",
            "https://user:pw@recall.example.com",
            "ftp://recall.example.com",
        ] {
            assert!(site(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn off_says_why() {
        let off = Passkeys::new("");
        let status = off.status();
        assert!(!status.enabled);
        assert!(status.reason.unwrap().contains("RECALL_PUBLIC_URL is not set"));
        let bad = Passkeys::new("http://recall.example.com");
        assert!(bad.status().reason.unwrap().contains("https://"));
        assert!(Passkeys::new("https://recall.example.com").status().enabled);
    }

    #[test]
    fn a_ceremony_is_used_once_and_each_address_has_a_share() {
        let p = Passkeys::new("https://recall.example.com");
        let site = p.site().ok().unwrap();
        let (_, auth) = site.webauthn.start_discoverable_authentication().unwrap();
        let id = p.begin(Pending::SignIn(auth), "1.2.3.4").ok().unwrap();
        assert!(p.take(&id).is_ok());
        assert!(p.take(&id).is_err(), "used once");
        for _ in 0..MAX_CEREMONIES_PER_ADDRESS {
            let (_, auth) = site.webauthn.start_discoverable_authentication().unwrap();
            p.begin(Pending::SignIn(auth), "1.2.3.4").ok().unwrap();
        }
        let (_, auth) = site.webauthn.start_discoverable_authentication().unwrap();
        assert!(p.begin(Pending::SignIn(auth), "1.2.3.4").is_err());
        let (_, auth) = site.webauthn.start_discoverable_authentication().unwrap();
        assert!(p.begin(Pending::SignIn(auth), "5.6.7.8").is_ok());
        assert_eq!(p.prune(), 0, "none has expired");
    }
}
