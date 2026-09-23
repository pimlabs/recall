//! What every authenticated request passes through before it reaches a
//! handler: the rate limiter first, then the bearer check.
//!
//! The order is the point, and so is the fact that exactly one header may
//! decide a client's rate-limit bucket — both are asserted by the tests
//! below and by `scripts/trusted-ip-check.sh` against a real socket.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use super::respond::error;
use super::AppState;

/// Rate limiting runs *before* auth, so a flood of invalid tokens is
/// limited too rather than escaping the limiter by never reaching the auth
/// check.
pub(super) async fn guard(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if state
        .limiter
        .limited(&client_ip(&req, &state.cfg.trusted_ip_header))
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
        return resp;
    }
    if let Some(asked) = unsupported_protocol(req.headers()) {
        return error(
            StatusCode::BAD_REQUEST,
            &format!(
                "this server speaks Recall protocol {}, and the request asked for {asked}. \
                 Upgrade whichever side is older; GET {} says what this server supports",
                recall_wire::PROTOCOL,
                recall_wire::DISCOVERY_PATH
            ),
        );
    }
    if !authorized(&state.cfg.token, req.headers()) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
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
