//! The `/admin` page, and the session a passkey sign-in gives it.
//!
//! A session is a cookie holding 256 random bits, of which the server keeps
//! only the SHA-256. It ends after twelve hours unused, and after thirty
//! days however much it is used. It is a third credential on the routes
//! that manage devices, beside the bearer token and an admin device's
//! signature, and on nothing else: `/sync` never reads it.
//!
//! Cross-site requests are stopped twice. The cookie is `SameSite=Strict`,
//! so a browser does not send it with a request another site started; and
//! every state-changing request must also carry the session's CSRF token in
//! a header, which a page on another origin cannot read or set. The token
//! is derived from the cookie's value rather than stored, so nothing but
//! the cookie is needed to check it, and knowing the token tells nobody
//! the cookie.
//!
//! The passkey ceremonies that start a session live in `passkeys.rs`,
//! behind the `passkeys` feature; everything here builds without it.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::auth::Caller;
pub(super) use super::devices::no_store;
use super::middleware::{constant_time_eq, limit, ClientIp};
use super::respond::{internal, json, Refusal};
use super::AppState;
use crate::{format_timestamp, parse_timestamp};

/// The page is embedded so the binary stays self-contained: there is no
/// asset directory to forget to ship.
const ADMIN_HTML: &str = include_str!("../../assets/admin.html");

/// `__Host-` makes a browser refuse the cookie unless it is `Secure`,
/// `Path=/` and has no `Domain`, so no other host, not even a subdomain,
/// can set or shadow it.
pub(super) const SESSION_COOKIE: &str = "__Host-recall_admin";

/// The header a state-changing request carries the CSRF token in.
pub(super) const CSRF_HEADER: &str = "x-recall-csrf";

/// A session unused for this long has ended.
pub(super) const SESSION_IDLE: Duration = Duration::from_secs(12 * 60 * 60);

/// A session ends this long after it began, however much it is used.
pub(super) const SESSION_LIMIT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A session's `last_used_at` is written at most this often: each request
/// would be a write, and against twelve hours a minute is nothing.
const TOUCH_EVERY: Duration = Duration::from_secs(60);

/// The page's Content-Security-Policy.
///
/// The script and the stylesheet are inline, so the page is one response
/// with nothing external, and each is allowed by its SHA-256 rather than by
/// `'unsafe-inline'`: a script injected into the page some day would not
/// match, and would not run. A hash rather than a nonce because the page is
/// static, so the hash is computed once, and the page can be cached.
/// `connect-src 'self'` means even a script that did run would have nowhere
/// to send anything but this server.
static ADMIN_CSP: LazyLock<String> = LazyLock::new(|| csp_for(ADMIN_HTML));

fn csp_for(html: &str) -> String {
    let hashes = |tag: &str| {
        inline_blocks(html, tag)
            .into_iter()
            .map(|block| format!("'sha256-{}'", STANDARD.encode(Sha256::digest(block))))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!(
        "default-src 'none'; script-src {}; style-src {}; connect-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        hashes("script"),
        hashes("style")
    )
}

/// The text inside each `<tag>…</tag>`, exactly as a browser hashes it.
fn inline_blocks<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut blocks = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else {
            break;
        };
        blocks.push(&after[..end]);
        rest = &after[end + close.len()..];
    }
    blocks
}

/// `GET /admin`: static markup, no data. It signs in with a passkey, or
/// takes the token as it always has, and fetches everything else itself.
pub(super) async fn handle_admin_page() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, ADMIN_CSP.as_str()),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            // The CSP carries the page's hashes, so a cached page must not
            // outlive the header it came with across a deploy.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        ADMIN_HTML,
    )
        .into_response()
}

fn sha256_hex(text: &str) -> String {
    recall_wire::content_sha256(text)
}

/// The CSRF token of the session whose cookie holds `token`.
pub(super) fn csrf_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"recall admin csrf\0");
    h.update(token.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}

/// The session cookie's value, when the request has one. A value that
/// cannot be one of ours (43 base64url characters) counts as none.
fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value.trim())
        .filter(|value| {
            value.len() == 43
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Whether the request carries a session cookie at all.
pub(super) fn has_session_cookie(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|pair| pair.trim().starts_with(&format!("{SESSION_COOKIE}=")))
}

/// A live session, once its cookie has been checked.
#[derive(Debug, Clone)]
pub(super) struct OwnerSession {
    /// SHA-256 of the cookie's value.
    pub(super) token_sha256: String,
    /// The passkey that signed in.
    pub(super) credential_id: String,
    /// What the page sends back as [`CSRF_HEADER`].
    pub(super) csrf_token: String,
    /// When it ends, however much it is used.
    pub(super) expires_at: String,
    /// When it ends if it is not used before then.
    pub(super) idle_expires_at: String,
}

fn expired() -> Refusal {
    Refusal::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized: the admin session has ended; sign in again",
    )
}

/// The session a request's cookie names, if it is live, marked as used.
/// `Ok(None)` when there is no cookie; a cookie naming no live session is
/// refused.
pub(super) fn live_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<OwnerSession>, Refusal> {
    let Some(token) = session_token(headers) else {
        return if has_session_cookie(headers) {
            Err(expired())
        } else {
            Ok(None)
        };
    };
    let token_sha256 = sha256_hex(token);
    let session = match state.store.admin_session(&token_sha256) {
        Ok(Some(session)) => session,
        Ok(None) => return Err(expired()),
        Err(e) => return Err(Refusal::internal(e)),
    };
    let now = state.clock();
    let (Some(last_used), Some(limit)) = (
        parse_timestamp(&session.last_used_at),
        parse_timestamp(&session.expires_at),
    ) else {
        return Err(expired());
    };
    if now >= limit || now - last_used >= SESSION_IDLE {
        // Gone for good: nothing can bring it back, so it need not wait
        // for the sweep.
        let _ = state.store.delete_admin_session(&token_sha256);
        return Err(expired());
    }
    let mut used = last_used;
    if now - last_used >= TOUCH_EVERY {
        match state
            .store
            .touch_admin_session(&token_sha256, &format_timestamp(now))
        {
            Ok(()) => used = now,
            Err(e) => eprintln!("recording an admin session's use: {e:#}"),
        }
    }
    Ok(Some(OwnerSession {
        token_sha256,
        credential_id: session.credential_id,
        csrf_token: csrf_token(token),
        expires_at: session.expires_at,
        idle_expires_at: format_timestamp((used + SESSION_IDLE).min(limit)),
    }))
}

/// A state-changing request must carry the session's CSRF token.
pub(super) fn check_csrf(
    session: &OwnerSession,
    method: &Method,
    headers: &HeaderMap,
) -> Result<(), Refusal> {
    if matches!(*method, Method::GET | Method::HEAD) {
        return Ok(());
    }
    let sent = headers
        .get(CSRF_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if sent.is_empty() || !constant_time_eq(sent, session.csrf_token.as_bytes()) {
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            "forbidden: this needs the admin session's X-Recall-CSRF header",
        ));
    }
    Ok(())
}

/// For the device routes, after the bearer token and a signature have been
/// ruled out: the admin session, with its CSRF token on anything that
/// changes state. The caller is the owner.
pub(super) fn authenticate_session(state: &AppState, req: &mut Request) -> Result<(), Refusal> {
    let Some(session) = live_session(state, req.headers())? else {
        return Err(Refusal::new(StatusCode::UNAUTHORIZED, "unauthorized"));
    };
    check_csrf(&session, req.method(), req.headers())?;
    req.extensions_mut().insert(Caller::Owner {
        credential_id: session.credential_id.clone(),
    });
    req.extensions_mut().insert(session);
    Ok(())
}

/// For the routes only a signed-in owner may use, managing passkeys and
/// signing out: the rate limit and protocol check, then the session and
/// its CSRF token. Not the bearer token, and not a device: a leaked token
/// must not be able to add a passkey, which would outlast rotating it.
pub(super) async fn owner_only(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(refused) = limit(&state, &req) {
        return refused;
    }
    let session = match live_session(&state, req.headers()) {
        Ok(Some(session)) => session,
        Ok(None) => {
            return Refusal::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized: this needs an admin session; sign in with a passkey",
            )
            .into_response()
        }
        Err(refused) => return refused.into_response(),
    };
    if let Err(refused) = check_csrf(&session, req.method(), req.headers()) {
        return refused.into_response();
    }
    let ip = super::middleware::client_ip(&req, &state.cfg.trusted_ip_header);
    req.extensions_mut().insert(ClientIp(ip));
    req.extensions_mut().insert(session);
    next.run(req).await
}

/// The `Set-Cookie` that starts a session.
pub(super) fn session_cookie(token: &str, max_age: Duration) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}",
        max_age.as_secs()
    ))
    .expect("a base64url token makes a valid header")
}

/// The `Set-Cookie` that ends one.
pub(super) fn cleared_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-recall_admin=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0",
    )
}

/// What the page is told about a session it holds.
#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionView {
    /// Sent back as `X-Recall-CSRF` on every state-changing request.
    pub(super) csrf_token: String,
    /// When the session ends, however much it is used.
    pub(super) expires_at: String,
    /// When it ends if it is not used before then.
    pub(super) idle_expires_at: String,
}

impl From<&OwnerSession> for SessionView {
    fn from(s: &OwnerSession) -> Self {
        Self {
            csrf_token: s.csrf_token.clone(),
            expires_at: s.expires_at.clone(),
            idle_expires_at: s.idle_expires_at.clone(),
        }
    }
}

/// Whether passkey sign-in is available, and if not, why.
#[derive(Debug, Clone, Serialize)]
pub(super) struct PasskeyStatus {
    /// Whether the page can sign in with a passkey.
    pub(super) enabled: bool,
    /// The origin passkeys are bound to, from `RECALL_PUBLIC_URL`.
    pub(super) origin: Option<String>,
    /// Why sign-in is off, for the page to show.
    pub(super) reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionStatus {
    passkeys: PasskeyStatus,
    /// Whether a passkey has been registered.
    bootstrapped: bool,
    /// The session this request's cookie names, if it is live.
    session: Option<SessionView>,
}

/// `GET /admin/session`: what the page needs to decide what to show.
/// Unauthenticated: it tells a stranger only whether sign-in is set up,
/// and the CSRF token only to the holder of the cookie.
pub(super) async fn handle_session_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let bootstrapped = match state.store.has_admin_credentials() {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let (session, clear) = match live_session(&state, &headers) {
        Ok(session) => (session, false),
        Err(_) => (None, true),
    };
    let mut resp = no_store(json(
        StatusCode::OK,
        &SessionStatus {
            passkeys: state.passkeys.status(),
            bootstrapped,
            session: session.as_ref().map(SessionView::from),
        },
    ));
    if clear {
        resp.headers_mut()
            .insert(header::SET_COOKIE, cleared_cookie());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_csp_allows_exactly_the_pages_own_script_and_style() {
        let scripts = inline_blocks(ADMIN_HTML, "script");
        let styles = inline_blocks(ADMIN_HTML, "style");
        assert_eq!(scripts.len(), 1, "one inline script, hashed");
        assert_eq!(styles.len(), 1, "one inline stylesheet, hashed");
        let csp = csp_for(ADMIN_HTML);
        assert!(!csp.contains("unsafe"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        let want = STANDARD.encode(Sha256::digest(scripts[0]));
        assert!(
            csp.contains(&format!("script-src 'sha256-{want}'")),
            "{csp}"
        );
        // Anything the hashes do not cover would be blocked, and the page
        // would break in a way no Rust test sees: no script from elsewhere,
        // no inline handlers, no style attributes.
        assert!(!ADMIN_HTML.contains("<script src"));
        assert!(!ADMIN_HTML.contains(" style=\""));
        for handler in [
            " onclick=",
            " onload=",
            " onsubmit=",
            " oninput=",
            " onchange=",
        ] {
            assert!(!ADMIN_HTML.contains(handler), "{handler}");
        }
    }

    #[test]
    fn the_session_cookie_is_found_among_others_and_nothing_else_is() {
        let token = "a".repeat(43);
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            format!("theme=dark; {SESSION_COOKIE}={token}; x=y")
                .parse()
                .unwrap(),
        );
        assert_eq!(session_token(&h), Some(token.as_str()));
        assert!(has_session_cookie(&h));

        h.insert(
            header::COOKIE,
            format!("{SESSION_COOKIE}=short").parse().unwrap(),
        );
        assert_eq!(session_token(&h), None, "not one of ours");
        assert!(has_session_cookie(&h), "but a cookie all the same");

        h.insert(
            header::COOKIE,
            format!("recall_admin={token}").parse().unwrap(),
        );
        assert_eq!(session_token(&h), None, "the prefix is part of the name");
        assert!(!has_session_cookie(&h));
    }

    #[test]
    fn the_csrf_token_is_bound_to_the_cookie_and_is_not_it() {
        let a = "a".repeat(43);
        let b = "b".repeat(43);
        assert_ne!(csrf_token(&a), csrf_token(&b));
        assert_ne!(csrf_token(&a), a);
        assert_eq!(csrf_token(&a), csrf_token(&a));
    }

    #[test]
    fn the_cookie_has_every_attribute_it_needs() {
        let c = session_cookie(&"a".repeat(43), SESSION_LIMIT);
        let c = c.to_str().unwrap();
        for attr in [
            "HttpOnly",
            "Secure",
            "SameSite=Strict",
            "Path=/",
            "Max-Age=2592000",
        ] {
            assert!(c.contains(attr), "{c}");
        }
        assert!(c.starts_with("__Host-recall_admin="));
        assert!(!c.to_ascii_lowercase().contains("domain="));
    }
}
