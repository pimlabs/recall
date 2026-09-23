//! Who sent a request: the operator, holding `RECALL_TOKEN`, or an
//! enrolled device, signing with its key.
//!
//! The bearer check lives in `middleware.rs`, where it always has. This is
//! the other way in: a request signed as `recall_wire::signature`
//! describes, checked against the device its `keyid` names, and remembered
//! so it cannot be sent twice.

// A refusal is the reply itself, which the middleware hands straight back
// to axum. Boxing it would allocate on the error path to save a copy
// nobody would notice.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use recall_wire::devices::SCOPE_ADMIN;
use recall_wire::signature::{
    self, SignatureInput, Target, LABEL, SIGNATURE_HEADER, SIGNATURE_INPUT_HEADER, WINDOW_SECONDS,
};

use super::respond::{error, internal};
use super::AppState;
use crate::{now, parse_timestamp};

/// Who a request came from, once it is known. The auth middleware puts one
/// in every authenticated request's extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Caller {
    /// Held `RECALL_TOKEN`: the operator, who may do anything.
    Operator,
    /// An enrolled device.
    Device {
        /// `dev_…`.
        id: String,
        /// `sync` or `admin`.
        scope: String,
    },
}

impl Caller {
    /// Whether this caller may manage devices and enrolment keys.
    pub(super) fn is_admin(&self) -> bool {
        match self {
            Caller::Operator => true,
            Caller::Device { scope, .. } => scope == SCOPE_ADMIN,
        }
    }
}

/// Whether a request carries a signature at all. One that does is judged
/// by it; one that does not falls back to the bearer check's answer.
pub(super) fn is_signed(headers: &HeaderMap) -> bool {
    headers.contains_key(SIGNATURE_INPUT_HEADER) || headers.contains_key(SIGNATURE_HEADER)
}

/// How many nonces are remembered at once. Only a request whose signature
/// verified gets one in, so this bounds what enrolled devices can use, not
/// what the internet can: at the default rate limit of 60 a minute per
/// address, filling it inside one window takes well over a hundred
/// addresses sending valid signatures. Each entry is under 200 bytes, so a
/// full cache is a few megabytes.
const REPLAY_CAPACITY: usize = 16_384;

/// `last_seen` is written at most this often per device: every request
/// would be a write, and "seen in the last minute" is all a person reading
/// the device list, or the ephemeral sweep, needs.
const LAST_SEEN_EVERY: Duration = Duration::from_secs(60);

/// Checks a signed request. `body` is the whole body, already read.
///
/// The order is cheapest first, and the nonce is recorded last: only a
/// request whose signature verified may take a place in the replay cache,
/// or anyone could fill it.
pub(super) fn verify(state: &AppState, parts: &Parts, body: &[u8]) -> Result<Caller, Response> {
    let rejected = |why: &dyn std::fmt::Display| {
        error(StatusCode::UNAUTHORIZED, &format!("unauthorized: {why}"))
    };
    let field = |name: &str| joined(&parts.headers, name);

    let (Some(input_field), Some(signature_field)) =
        (field(SIGNATURE_INPUT_HEADER), field(SIGNATURE_HEADER))
    else {
        return Err(rejected(
            &"a signed request needs both signature-input and signature",
        ));
    };
    let input = SignatureInput::parse(&input_field, LABEL).map_err(|e| rejected(&e))?;
    let sig = signature::parse_signature(&signature_field, LABEL).map_err(|e| rejected(&e))?;
    let Some(keyid) = input.keyid() else {
        return Err(rejected(&signature::SignatureError::MissingParameter(
            "keyid",
        )));
    };

    let device = match state.store.device(keyid) {
        Ok(Some(device)) => device,
        Ok(None) => return Err(rejected(&"unknown device")),
        Err(e) => return Err(internal(e)),
    };
    if device.revoked_at.is_some() {
        return Err(rejected(&"this device has been revoked"));
    }
    let key = signature::parse_public_key(&device.public_key)
        .map_err(|e| internal(anyhow::anyhow!("device {} has a bad key: {e}", device.id)))?;

    // Behind Traefik the request arrives as HTTP/1.1 with the client's
    // Host passed through; over HTTP/2 the authority is in the URI.
    let host = parts
        .headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| parts.uri.authority().map(|a| a.as_str()))
        .unwrap_or("");
    let authority = signature::normalize_authority(host);
    let target = Target {
        method: parts.method.as_str(),
        authority: &authority,
        path: parts.uri.path(),
        query: parts.uri.query(),
    };
    let unix_now = unix_now();
    signature::verify_request(
        &input,
        &sig,
        &target,
        &field,
        body,
        &key,
        unix_now,
        WINDOW_SECONDS,
    )
    .map_err(|e| rejected(&e))?;

    // check_profile has made sure both are there.
    let (nonce, created) = (input.nonce().unwrap_or(""), input.created().unwrap_or(0));
    match state.replay.first_use(keyid, nonce, created, unix_now) {
        Ok(true) => {}
        Ok(false) => return Err(rejected(&"this request was already received once")),
        Err(Full) => {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "too many signed requests at once, try again later",
            ))
        }
    }

    let stale = device
        .last_seen
        .as_deref()
        .and_then(parse_timestamp)
        .is_none_or(|seen| time::OffsetDateTime::now_utc() - seen >= LAST_SEEN_EVERY);
    if stale {
        // Best-effort: failing to record a sighting is no reason to refuse
        // a request that has proved who sent it.
        if let Err(e) = state.store.touch_device(&device.id, &now()) {
            eprintln!("recording last_seen for {}: {e:#}", device.id);
        }
    }
    Ok(Caller::Device {
        id: device.id,
        scope: device.scope,
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A header's value, with repeated instances joined by `", "` as RFC 9110
/// §5.3 combines them, and RFC 9421 §2.1 signs them. [`None`] when absent
/// or not ASCII.
fn joined(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut out: Option<String> = None;
    for value in headers.get_all(name) {
        let value = value.to_str().ok()?.trim();
        match &mut out {
            Some(s) => {
                s.push_str(", ");
                s.push_str(value);
            }
            None => out = Some(value.to_string()),
        }
    }
    out
}

/// Every nonce a verified signature carried, until its `created` falls out
/// of the window and the request could no longer be accepted anyway (RFC
/// 9421 §7.2.2).
pub(super) struct ReplayCache {
    capacity: usize,
    state: Mutex<ReplayState>,
}

struct ReplayState {
    /// `(keyid, nonce)` to the last UNIX second the request could still be
    /// accepted.
    seen: HashMap<(String, String), i64>,
    last_sweep: i64,
}

/// The cache is full of nonces still inside their window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Full;

impl ReplayCache {
    pub(super) fn new() -> Self {
        Self::with_capacity(REPLAY_CAPACITY)
    }

    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(ReplayState {
                seen: HashMap::new(),
                last_sweep: 0,
            }),
        }
    }

    /// Records a nonce, answering whether this is the first time it was
    /// seen inside its window.
    ///
    /// When the cache is full it refuses rather than forgetting an older
    /// nonce: forgetting one would let that request be replayed.
    pub(super) fn first_use(
        &self,
        keyid: &str,
        nonce: &str,
        created: i64,
        now: i64,
    ) -> Result<bool, Full> {
        let until = created.saturating_add(WINDOW_SECONDS as i64);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        // Swept on the way through, as the rate limiter is: no task of its
        // own, and no entry outlives its window by more than one window.
        if now - state.last_sweep >= WINDOW_SECONDS as i64 || state.seen.len() >= self.capacity {
            state.last_sweep = now;
            state.seen.retain(|_, until| *until >= now);
        }
        let key = (keyid.to_string(), nonce.to_string());
        if state.seen.get(&key).is_some_and(|u| *u >= now) {
            return Ok(false);
        }
        if state.seen.len() >= self.capacity {
            return Err(Full);
        }
        state.seen.insert(key, until);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_accepted_once_inside_its_window() {
        let cache = ReplayCache::new();
        assert_eq!(cache.first_use("dev_a", "n1", 1000, 1000), Ok(true));
        assert_eq!(cache.first_use("dev_a", "n1", 1000, 1030), Ok(false));
        // The same nonce from another device is another request.
        assert_eq!(cache.first_use("dev_b", "n1", 1000, 1030), Ok(true));
        assert_eq!(cache.first_use("dev_a", "n2", 1000, 1030), Ok(true));
    }

    /// Once `created` is out of the window the request is refused on its
    /// clock, so the nonce can be forgotten and the cache stays small.
    #[test]
    fn nonces_are_forgotten_once_their_window_has_passed() {
        let cache = ReplayCache::with_capacity(2);
        assert_eq!(cache.first_use("d", "a", 1000, 1000), Ok(true));
        assert_eq!(cache.first_use("d", "b", 1000, 1000), Ok(true));
        assert_eq!(
            cache.first_use("d", "c", 1000, 1000),
            Err(Full),
            "full of live nonces: refuse, never forget one"
        );
        assert_eq!(cache.first_use("d", "c", 1100, 1100), Ok(true));
        let n = cache
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .seen
            .len();
        assert_eq!(n, 1, "expired nonces should have been swept");
    }

    #[test]
    fn repeated_headers_are_joined_as_rfc9110_combines_them() {
        let mut h = HeaderMap::new();
        h.append("content-digest", "sha-256=:a:".parse().unwrap());
        h.append("content-digest", " sha-512=:b: ".parse().unwrap());
        assert_eq!(
            joined(&h, "content-digest").as_deref(),
            Some("sha-256=:a:, sha-512=:b:")
        );
        assert_eq!(joined(&h, "signature"), None);
    }

    #[test]
    fn only_the_operator_and_admin_devices_are_admins() {
        assert!(Caller::Operator.is_admin());
        assert!(Caller::Device {
            id: "dev_a".into(),
            scope: "admin".into()
        }
        .is_admin());
        assert!(!Caller::Device {
            id: "dev_a".into(),
            scope: "sync".into()
        }
        .is_admin());
    }
}
