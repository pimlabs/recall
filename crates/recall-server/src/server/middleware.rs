//! What every authenticated request passes through before it reaches a
//! handler: the rate limiter first, then the protocol check, then auth.
//!
//! The order is the point, and so is the fact that exactly one header may
//! decide a client's rate-limit bucket — both are asserted by the tests
//! below and by `scripts/trusted-ip-check.sh` against a real socket.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::auth::{self, Caller};
use super::respond::{error, Refusal};
use super::AppState;

/// Rate limiting runs *before* auth, so a flood of invalid tokens is
/// limited too rather than escaping the limiter by never reaching the auth
/// check.
///
/// Either credential is accepted: the operator's `RECALL_TOKEN`, exactly as
/// before devices existed, or a device's signature. Whichever it was is
/// left in the request's extensions as a [`Caller`], for the routes that
/// care.
pub(super) async fn guard(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(refused) = limit(&state, &req) {
        return refused;
    }
    match authenticate(&state, req).await {
        Ok(req) => next.run(req).await,
        Err(refused) => refused.into_response(),
    }
}

/// The address a request came from, as the rate limiter keys it, for the
/// routes that count per address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientIp(pub(super) String);

/// For the routes anyone may call, which is enrolling and polling: the
/// same rate limit and protocol check as everything else, and no auth,
/// since a machine enrolling has no credential yet. A body declared larger
/// than those routes take is refused before any of it is read.
pub(super) async fn limited(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(refused) = limit(&state, &req) {
        return refused;
    }
    if declared_length(req.headers()).is_some_and(|n| n > super::ENROLL_BODY_BYTES) {
        return too_large().into_response();
    }
    let ip = client_ip(&req, &state.cfg.trusted_ip_header);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// What `Content-Length` says the body will be, when it says.
fn declared_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(axum::http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub(super) fn too_large() -> Refusal {
    Refusal::new(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")
}

/// After [`guard`], on the routes that manage devices: the operator, or a
/// device approved with the admin scope. A `sync` device has proved who it
/// is, so this is a 403, not a 401.
pub(super) async fn admin_only(req: Request, next: Next) -> Response {
    match req.extensions().get::<Caller>() {
        Some(caller) if caller.is_admin() => next.run(req).await,
        _ => error(
            StatusCode::FORBIDDEN,
            "forbidden: this needs RECALL_TOKEN or a device with the admin scope",
        ),
    }
}

/// The rate limit, then the protocol check. [`None`] when the request may
/// go on.
fn limit(state: &AppState, req: &Request) -> Option<Response> {
    if state
        .limiter
        .limited(&client_ip(req, &state.cfg.trusted_ip_header))
    {
        let mut resp = error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limit exceeded, try again later",
        );
        if let Ok(v) = state
            .cfg
            .rate_limit_window
            .as_secs()
            .to_string()
            .parse::<axum::http::HeaderValue>()
        {
            resp.headers_mut().insert("retry-after", v);
        }
        return Some(resp);
    }
    if let Some(asked) = unsupported_protocol(req.headers()) {
        return Some(error(
            StatusCode::BAD_REQUEST,
            &format!(
                "this server speaks Recall protocol {}, and the request asked for {asked}. \
                 Upgrade whichever side is older; GET {} says what this server supports",
                recall_wire::PROTOCOL,
                recall_wire::DISCOVERY_PATH
            ),
        ));
    }
    None
}

/// The bearer token first, unchanged: a request carrying the right one is
/// the operator's, whatever else it carries. Then a signature, if there is
/// one. Anything else is the same bare 401 it always was.
///
/// A signed request's body has to be read here, since the signature's
/// digest covers it. It is read only once the headers alone have proved
/// the request is its device's (see `auth.rs`), and a body declared too
/// large is refused before then, so nobody without a device key can make
/// the server hold one.
async fn authenticate(state: &AppState, mut req: Request) -> Result<Request, Refusal> {
    if authorized(&state.cfg.token, req.headers()) {
        req.extensions_mut().insert(Caller::Operator);
        return Ok(req);
    }
    if !auth::is_signed(req.headers()) {
        return Err(Refusal::new(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let (parts, body) = req.into_parts();
    let checked = auth::check_headers(state, &parts)?;
    if declared_length(&parts.headers).is_some_and(|n| n > super::MAX_BODY_BYTES) {
        return Err(too_large());
    }
    let Ok(bytes) = axum::body::to_bytes(body, super::MAX_BODY_BYTES).await else {
        return Err(too_large());
    };
    let (caller, signed) = auth::finish(state, checked, &bytes)?;
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(caller);
    req.extensions_mut().insert(signed);
    Ok(req)
}

/// The protocol a request asked for, when it is one this server does not
/// speak. A request that names none is protocol 1: every client before the
/// header existed spoke it.
fn unsupported_protocol(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(recall_wire::PROTOCOL_HEADER)?;
    let text = value.to_str().unwrap_or("").trim();
    match text.parse::<u32>() {
        Ok(recall_wire::PROTOCOL) => None,
        _ => Some(text.to_string()),
    }
}

fn authorized(token: &str, headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    !value.is_empty() && constant_time_eq(value.as_bytes(), token.as_bytes())
}

/// Compared without an early exit so the time taken doesn't reveal how much
/// of a guessed token was right. Lengths are allowed to short-circuit —
/// they leak only the length, as `crypto/subtle` does.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// The address rate limiting keys off.
///
/// Reads exactly one header — the one `RECALL_TRUSTED_IP_HEADER` names — and
/// falls back to the socket's peer address. One header, not a list of
/// candidates: anything this server is willing to read from an untrusted
/// client is something that client can choose, and choosing your own rate
/// limit bucket defeats the rate limit.
///
/// This is safe only while nothing can reach the process except through the
/// ingress that sets that header. The compose files keep it that way by
/// using `expose` rather than `ports`, so the origin has no published port
/// to be addressed directly. If that ever changes, this setting is wrong and
/// the limiter is decorative.
fn client_ip(req: &Request, trusted_header: &str) -> String {
    if !trusted_header.is_empty() {
        if let Some(ip) = header_str(req.headers(), trusted_header) {
            return ip.to_string();
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_comparison_rejects_everything_but_the_exact_token() {
        let mut h = HeaderMap::new();
        assert!(!authorized("secret", &h), "no header");
        h.insert("authorization", "secret".parse().unwrap());
        assert!(!authorized("secret", &h), "missing Bearer scheme");
        h.insert("authorization", "Bearer ".parse().unwrap());
        assert!(!authorized("secret", &h), "empty token");
        h.insert("authorization", "Bearer secre".parse().unwrap());
        assert!(!authorized("secret", &h), "prefix of the token");
        h.insert("authorization", "Basic secret".parse().unwrap());
        assert!(!authorized("secret", &h), "wrong scheme");
        h.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(authorized("secret", &h));
    }

    fn request_with(headers: Vec<(&str, &str)>) -> Request {
        let mut req = Request::new(axum::body::Body::empty());
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        for (k, v) in headers {
            let name = axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap();
            req.headers_mut().insert(name, v.parse().unwrap());
        }
        req
    }

    #[test]
    fn client_ip_reads_the_configured_header_then_the_socket() {
        // Cloudflare Tunnel, the default.
        assert_eq!(
            client_ip(
                &request_with(vec![("cf-connecting-ip", "198.51.100.4")]),
                "cf-connecting-ip"
            ),
            "198.51.100.4"
        );
        // Traefik, nginx, Caddy.
        assert_eq!(
            client_ip(
                &request_with(vec![("x-real-ip", "198.51.100.7")]),
                "x-real-ip"
            ),
            "198.51.100.7"
        );
        // Header configured but absent: fall back rather than invent one.
        assert_eq!(
            client_ip(&request_with(vec![]), "cf-connecting-ip"),
            "127.0.0.1"
        );
        // Empty means trust nothing.
        assert_eq!(
            client_ip(
                &request_with(vec![("cf-connecting-ip", "198.51.100.4")]),
                ""
            ),
            "127.0.0.1"
        );
    }

    /// The reason this is configurable at all.
    ///
    /// Behind Traefik the ingress sets `x-real-ip`, but a client can still
    /// send whatever it likes under any other name. If more than one header
    /// were consulted, rotating the one the ingress does *not* set would
    /// hand out a fresh rate-limit bucket per request — and the limiter runs
    /// before auth, so that is unlimited attempts at guessing the token.
    #[test]
    fn a_header_the_ingress_does_not_set_is_ignored() {
        let attacker = request_with(vec![
            ("cf-connecting-ip", "1.1.1.1"),
            ("x-forwarded-for", "2.2.2.2"),
            ("true-client-ip", "3.3.3.3"),
            ("x-real-ip", "198.51.100.7"),
        ]);
        assert_eq!(
            client_ip(&attacker, "x-real-ip"),
            "198.51.100.7",
            "only the configured header may decide the bucket"
        );

        // And the same in the other direction: on Cloudflare, a spoofed
        // x-real-ip must not displace the tunnel's own header.
        assert_eq!(client_ip(&attacker, "cf-connecting-ip"), "1.1.1.1");
    }

    /// `x-forwarded-for` is deliberately not a sensible value for the
    /// setting: a proxy *appends* to it, so its first entry is whatever the
    /// client sent. This asserts the old first-entry behaviour is gone —
    /// reading the whole value is wrong too, but it is at least not silently
    /// attacker-chosen.
    #[test]
    fn forwarded_for_is_no_longer_split_and_trusted() {
        let req = request_with(vec![("x-forwarded-for", "203.0.113.9, 10.0.0.1")]);
        assert_ne!(
            client_ip(&req, "cf-connecting-ip"),
            "203.0.113.9",
            "x-forwarded-for must not be consulted when it is not the configured header"
        );
        assert_eq!(client_ip(&req, "cf-connecting-ip"), "127.0.0.1");
    }
}
