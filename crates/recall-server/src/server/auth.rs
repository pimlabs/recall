//! Who sent a request: the operator, holding `RECALL_TOKEN`, or an
//! enrolled device, signing with its key.
//!
//! The bearer check lives in `middleware.rs`, where it always has. This is
//! the other way in: a request signed as `recall_wire::signature`
//! describes, checked against the device its `keyid` names, and remembered
//! so it cannot be sent twice.
//!
//! It happens in two steps, and the body is read only between them. The
//! signature covers the `Content-Digest` header rather than the body
//! itself, so [`check_headers`] can verify it, with everything else a
//! request's headers say, before a byte of the body has arrived: only a
//! request the device's own key signed makes the server read and hold its
//! body. [`finish`] then checks the body against the digest the signature
//! vouched for, and only then records the nonce.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use recall_wire::devices::{SCOPE_ADMIN, SCOPE_SYNC, SCOPE_WORKER};
use recall_wire::signature::{
    self, Received, SignatureInput, Target, LABEL, MAX_AHEAD_SECONDS, SIGNATURE_HEADER,
    SIGNATURE_INPUT_HEADER, WINDOW_SECONDS,
};
use recall_wire::Device;

use super::respond::Refusal;
use super::AppState;
use crate::{now, parse_timestamp};

/// How far a signature's `created` may be behind the server's clock, and
/// so also how long after its `created` a nonce is remembered. One
/// constant for both: a verifier that accepted a wider window than the
/// cache remembers would accept a request again once its nonce had been
/// forgotten.
pub(super) const WINDOW: u64 = WINDOW_SECONDS;

/// The longest a nonce stays live: `created` may be a few seconds ahead of
/// the clock, and is then remembered for the window after it.
pub(super) const NONCE_LIFETIME: u64 = WINDOW + MAX_AHEAD_SECONDS;

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
        /// The name it enrolled as. A push it signs is recorded under this,
        /// whatever the push says: the name belongs to the key.
        name: String,
        /// `sync`, `admin` or `worker`.
        scope: String,
        /// Whether it is removed once idle.
        ephemeral: bool,
    },
}

impl Caller {
    /// Whether this caller may manage devices and authkeys, and read
    /// the stats.
    pub(super) fn is_admin(&self) -> bool {
        match self {
            Caller::Operator => true,
            Caller::Device { scope, .. } => scope == SCOPE_ADMIN,
        }
    }

    /// Whether this caller is a worker device: it may claim jobs and post
    /// their results, and nothing else. Not the operator, who has no
    /// business holding a lease.
    pub(super) fn is_worker(&self) -> bool {
        matches!(self, Caller::Device { scope, .. } if scope == SCOPE_WORKER)
    }
}

/// Whether a request carries a signature at all. One that does is judged
/// by it; one that does not falls back to the bearer check's answer.
pub(super) fn is_signed(headers: &HeaderMap) -> bool {
    headers.contains_key(SIGNATURE_INPUT_HEADER) || headers.contains_key(SIGNATURE_HEADER)
}

/// How many nonces are remembered at once, across every device. Only a
/// request whose signature verified gets one in, and each device has its
/// own smaller cap as well (see [`ReplayCache::new`]), so this bounds the
/// memory of many devices together. Each entry is under 200 bytes, so a
/// full cache is a few megabytes.
const REPLAY_CAPACITY: usize = 16_384;

/// The most live nonces one device may have, however high the rate limit
/// is set: a sixty-fourth of the cache. Every device would have to be at
/// its cap together to fill it, and an authkey mints at most
/// [`recall_wire::devices::DEFAULT_MAX_DEVICES`] unless its maker asked
/// for more, so a leaked one cannot mint enough to lock everyone else out.
pub(super) const MAX_NONCES_PER_DEVICE: usize = REPLAY_CAPACITY / 64;

/// `last_seen` is written at most this often per device: every request
/// would be a write, and "seen in the last minute" is all a person reading
/// the device list, or the ephemeral sweep, needs.
const LAST_SEEN_EVERY: Duration = Duration::from_secs(60);

/// A signed request whose headers have passed, waiting for its body.
pub(super) struct Checked {
    device: Device,
    digest: String,
    nonce: String,
    created: i64,
}

/// Whether this server knows what a device of `scope` may do.
///
/// The rest of the server decides by `admin` and `worker` and treats any
/// other device as a `sync` one, so a scope a later version adds would
/// otherwise be read as `sync`: a device a newer server kept away from
/// memory would read and write every project after a rollback to this
/// version. So a scope this server does not know is refused rather than
/// guessed at. (Servers from 0.4.1 refuse a worker this way; the one
/// before them does not, which is why a rollback past the worker revokes
/// it first.)
fn known_scope(scope: &str) -> bool {
    scope == SCOPE_SYNC || scope == SCOPE_ADMIN || scope == SCOPE_WORKER
}

fn rejected(why: &dyn std::fmt::Display) -> Refusal {
    Refusal::new(StatusCode::UNAUTHORIZED, format!("unauthorized: {why}"))
}

/// Everything about a signed request its headers can settle, cheapest
/// first: they parse; the device is known and not revoked; the signature
/// meets Recall's profile, `created` inside the window; it was not made so
/// early that the process before this one could have accepted it; the
/// signature verifies; and its nonce has not been seen. Nothing here reads
/// the body or records anything.
pub(super) fn check_headers(state: &AppState, parts: &Parts) -> Result<Checked, Refusal> {
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
        Err(e) => return Err(Refusal::internal(e)),
    };
    if device.revoked_at.is_some() {
        return Err(rejected(&"this device has been revoked"));
    }
    if !known_scope(&device.scope) {
        return Err(Refusal::new(
            StatusCode::FORBIDDEN,
            format!(
                "forbidden: this server does not know the scope {:?}; revoke the device \
                 or run the server version that enrolled it",
                device.scope
            ),
        ));
    }
    let key = signature::parse_public_key(&device.public_key).map_err(|e| {
        Refusal::internal(anyhow::anyhow!("device {} has a bad key: {e}", device.id))
    })?;

    // Behind Traefik the request arrives as HTTP/1.1 with the client's
    // Host passed through; over HTTP/2 the authority is in the URI.
    let host = parts
        .headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| parts.uri.authority().map(|a| a.as_str()))
        .unwrap_or("");
    let authority = signature::normalize_authority(host);
    let received = Received {
        input: &input,
        signature: &sig,
        target: Target {
            method: parts.method.as_str(),
            authority: &authority,
            path: parts.uri.path(),
            query: parts.uri.query(),
        },
        field: &field,
    };
    let now = state.now();
    signature::verify_headers(&received, &key, now, WINDOW).map_err(|e| rejected(&e))?;

    // check_profile has made sure both are there.
    let (nonce, created) = (input.nonce().unwrap_or(""), input.created().unwrap_or(0));
    // Nonces live in memory, so a restart forgets them, and a signature
    // the process before this one accepted must be refused here instead:
    // without this, every deploy would open a window in which the last
    // minute's requests could be sent again. That process judged by the
    // same clock, in whole seconds, and stopped before this one started,
    // so it accepted nothing dated more than MAX_AHEAD_SECONDS past this
    // one's start. Everything dated up to then is refused, including a
    // signature made in the very second this process started, which the
    // one before may have accepted in that same second. The cost is that
    // for those few seconds after a start every signature is refused, and
    // its client signs again.
    let settled = state.started().saturating_add(MAX_AHEAD_SECONDS as i64);
    if created <= settled {
        return Err(rejected(
            &"signature created before this server started, or too soon after; \
              sign the request again in a few seconds",
        ));
    }
    if state.replay.seen(keyid, nonce, now) {
        return Err(rejected(&"this request was already received once"));
    }
    Ok(Checked {
        digest: field(signature::CONTENT_DIGEST_HEADER).unwrap_or_default(),
        nonce: nonce.to_string(),
        created,
        device,
    })
}

/// The rest, once the body has been read: it matches the digest the
/// signature covered, and only then is the nonce recorded, so a forged or
/// altered request cannot use up a nonce the real device has yet to send.
pub(super) fn finish(state: &AppState, checked: Checked, body: &[u8]) -> Result<Caller, Refusal> {
    let Checked {
        device,
        digest,
        nonce,
        created,
    } = checked;
    signature::check_content_digest(&digest, body).map_err(|e| rejected(&e))?;

    match state
        .replay
        .first_use(&device.id, &nonce, created, &|| state.now())
    {
        Recorded::Fresh => {}
        Recorded::Replayed => return Err(rejected(&"this request was already received once")),
        Recorded::Stale => {
            return Err(rejected(
                &"the signature's window closed while the request was read; sign it again",
            ))
        }
        Recorded::DeviceFull => {
            return Err(Refusal::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too many signed requests from this device, try again later",
            ))
        }
        Recorded::Full => {
            return Err(Refusal::new(
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
        name: device.name,
        scope: device.scope,
        ephemeral: device.ephemeral,
    })
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
    window: i64,
    capacity: usize,
    per_device: usize,
    state: Mutex<ReplayState>,
}

struct ReplayState {
    /// `(keyid, nonce)` to the last UNIX second the request could still be
    /// accepted.
    seen: HashMap<(String, String), i64>,
    /// How many live entries each device has.
    per_device: HashMap<String, usize>,
    last_sweep: i64,
    /// How many sweeps there have been, for the tests that count them.
    #[cfg(test)]
    sweeps: usize,
}

/// What recording a nonce came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Recorded {
    /// Recorded: the first time inside its window.
    Fresh,
    /// Seen already, inside its window.
    Replayed,
    /// Its window closed before it could be recorded.
    Stale,
    /// Its device has as many live nonces as one device may.
    DeviceFull,
    /// The cache holds as many live nonces as it may.
    Full,
}

impl ReplayCache {
    /// A cache for signatures checked against `window`, holding at most
    /// `per_device` live nonces for any one device, and never more than
    /// [`MAX_NONCES_PER_DEVICE`].
    ///
    /// A nonce is live for up to [`NONCE_LIFETIME`], since `created` may be
    /// a few seconds ahead of the clock. The server sizes `per_device` from the
    /// rate limit: as many requests as one address may send in that time,
    /// which a device keeping to the limit never reaches. So one device
    /// that does, from many addresses, is refused on its own, and cannot
    /// fill the cache and lock every other device out. The ceiling keeps
    /// that true when the rate limit is set high: without it, a few dozen
    /// devices at their share would fill the cache.
    pub(super) fn new(window: u64, per_device: usize) -> Self {
        Self::with_capacity(
            window,
            REPLAY_CAPACITY,
            per_device.min(MAX_NONCES_PER_DEVICE),
        )
    }

    fn with_capacity(window: u64, capacity: usize, per_device: usize) -> Self {
        Self {
            window: window as i64,
            capacity,
            per_device: per_device.max(1),
            state: Mutex::new(ReplayState {
                seen: HashMap::new(),
                per_device: HashMap::new(),
                last_sweep: 0,
                #[cfg(test)]
                sweeps: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReplayState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a nonce is live already: the cheap refusal before a body is
    /// read. [`ReplayCache::first_use`] still decides.
    pub(super) fn seen(&self, keyid: &str, nonce: &str, now: i64) -> bool {
        self.lock()
            .seen
            .get(&(keyid.to_string(), nonce.to_string()))
            .is_some_and(|until| *until >= now)
    }

    /// Records a nonce, reading the clock under the lock.
    ///
    /// Reading it under the lock is what makes this safe against a request
    /// and its replay arriving together. Entries are swept by the clock,
    /// and the time this judges by never goes back past the last sweep,
    /// even when the system clock is stepped backwards: it is the later of
    /// the clock and the last sweep. So a nonce swept away had a window
    /// that closed before now, and its replay is refused as
    /// [`Recorded::Stale`] here rather than recorded afresh.
    ///
    /// When the cache is full it refuses rather than forgetting a live
    /// nonce: forgetting one would let that request be replayed.
    pub(super) fn first_use(
        &self,
        keyid: &str,
        nonce: &str,
        created: i64,
        clock: &dyn Fn() -> i64,
    ) -> Recorded {
        let until = created.saturating_add(self.window);
        let mut state = self.lock();
        let now = clock().max(state.last_sweep);
        let device_count = state.per_device.get(keyid).copied().unwrap_or(0);
        let crowded = state.seen.len() >= self.capacity || device_count >= self.per_device;
        // Swept on the way through, as the rate limiter is: no task of its
        // own, and no entry outlives its window by more than one window.
        // A full cache, or a device at its share, asks for a sweep sooner,
        // but at most once a second: a sweep walks every entry under the
        // lock, and a second sweep in the same second finds nothing the
        // first did not, so a device sending over its share would only be
        // making every other request wait.
        if now - state.last_sweep >= self.window || (crowded && now > state.last_sweep) {
            sweep(&mut state, now);
        }
        if until < now {
            return Recorded::Stale;
        }
        let key = (keyid.to_string(), nonce.to_string());
        if state.seen.get(&key).is_some_and(|u| *u >= now) {
            return Recorded::Replayed;
        }
        if state.per_device.get(keyid).copied().unwrap_or(0) >= self.per_device {
            return Recorded::DeviceFull;
        }
        if state.seen.len() >= self.capacity {
            return Recorded::Full;
        }
        state.seen.insert(key, until);
        *state.per_device.entry(keyid.to_string()).or_insert(0) += 1;
        Recorded::Fresh
    }
}

fn sweep(state: &mut ReplayState, now: i64) {
    #[cfg(test)]
    {
        state.sweeps += 1;
    }
    state.last_sweep = now;
    state.seen.retain(|_, until| *until >= now);
    state.per_device.clear();
    for (keyid, _) in state.seen.keys() {
        *state.per_device.entry(keyid.clone()).or_insert(0) += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t: i64) -> impl Fn() -> i64 {
        move || t
    }

    #[test]
    fn a_nonce_is_accepted_once_inside_its_window() {
        let cache = ReplayCache::new(60, 100);
        assert_eq!(
            cache.first_use("dev_a", "n1", 1000, &at(1000)),
            Recorded::Fresh
        );
        assert!(cache.seen("dev_a", "n1", 1030));
        assert_eq!(
            cache.first_use("dev_a", "n1", 1000, &at(1030)),
            Recorded::Replayed
        );
        // The same nonce from another device is another request.
        assert_eq!(
            cache.first_use("dev_b", "n1", 1000, &at(1030)),
            Recorded::Fresh
        );
        assert_eq!(
            cache.first_use("dev_a", "n2", 1000, &at(1030)),
            Recorded::Fresh
        );
    }

    /// Once `created` is out of the window the request is refused on its
    /// clock, so the nonce can be forgotten and the cache stays small.
    #[test]
    fn nonces_are_forgotten_once_their_window_has_passed() {
        let cache = ReplayCache::with_capacity(60, 2, 100);
        assert_eq!(cache.first_use("d", "a", 1000, &at(1000)), Recorded::Fresh);
        assert_eq!(cache.first_use("e", "b", 1000, &at(1000)), Recorded::Fresh);
        assert_eq!(
            cache.first_use("f", "c", 1000, &at(1000)),
            Recorded::Full,
            "full of live nonces: refuse, never forget one"
        );
        assert_eq!(cache.first_use("f", "c", 1100, &at(1100)), Recorded::Fresh);
        assert_eq!(cache.lock().seen.len(), 1, "expired nonces were swept");
    }

    /// The review's finding: one device could fill the whole cache and
    /// lock every other device out. Now it fills only its own share.
    #[test]
    fn one_device_cannot_fill_the_cache_for_the_others() {
        let cache = ReplayCache::with_capacity(60, 100, 3);
        for n in ["a", "b", "c"] {
            assert_eq!(
                cache.first_use("greedy", n, 1000, &at(1000)),
                Recorded::Fresh
            );
        }
        assert_eq!(
            cache.first_use("greedy", "d", 1000, &at(1000)),
            Recorded::DeviceFull
        );
        assert_eq!(
            cache.first_use("other", "a", 1000, &at(1000)),
            Recorded::Fresh
        );
        // Its share comes back as its nonces age out.
        assert_eq!(
            cache.first_use("greedy", "d", 1100, &at(1100)),
            Recorded::Fresh
        );
    }

    /// Verification finding N3: with the rate limit set high, a device's
    /// share was high too, and ~137 devices at it filled the cache. It is
    /// now a sixty-fourth of the cache at most, whatever the limit.
    #[test]
    fn a_devices_share_is_capped_however_high_the_rate_limit() {
        assert_eq!(ReplayCache::new(60, usize::MAX).per_device, 256);
        assert_eq!(ReplayCache::new(60, 120).per_device, 120);
        let cache = ReplayCache::new(60, usize::MAX);
        for n in 0..256 {
            assert_eq!(
                cache.first_use("greedy", &n.to_string(), 1000, &at(1000)),
                Recorded::Fresh
            );
        }
        assert_eq!(
            cache.first_use("greedy", "one more", 1000, &at(1000)),
            Recorded::DeviceFull
        );
    }

    /// The race the review found: a sweep by one request removing the
    /// nonce of another that passed its clock check a moment earlier. The
    /// second is refused, because its window is judged by the clock read
    /// under the lock, not by the one it was checked with.
    #[test]
    fn a_nonce_swept_away_is_not_recorded_afresh() {
        let cache = ReplayCache::new(60, 100);
        // Accepted at the last second of its window.
        assert_eq!(cache.first_use("d", "n", 40, &at(100)), Recorded::Fresh);
        // Later, another request sweeps it away.
        assert_eq!(cache.first_use("e", "m", 101, &at(161)), Recorded::Fresh);
        assert!(!cache.seen("d", "n", 101));
        // The replay, checked against a clock that still said 100, reaches
        // the cache after the sweep.
        assert_eq!(cache.first_use("d", "n", 40, &at(101)), Recorded::Stale);
    }

    /// Verification finding N6: the system clock can be stepped
    /// backwards. A nonce swept away must not be recorded afresh when the
    /// clock, stepped back, says its window is open again.
    #[test]
    fn a_clock_stepped_back_does_not_bring_a_swept_nonce_back() {
        let cache = ReplayCache::new(60, 100);
        assert_eq!(cache.first_use("d", "n", 40, &at(100)), Recorded::Fresh);
        // A request at 161 sweeps it away.
        assert_eq!(cache.first_use("e", "m", 101, &at(161)), Recorded::Fresh);
        // Then the clock is stepped back to 90, inside the first one's
        // window, and its replay arrives.
        assert_eq!(cache.first_use("d", "n", 40, &at(90)), Recorded::Stale);
    }

    /// Verification finding N5: a device over its share made every one of
    /// its requests sweep the whole cache under the lock. It sweeps at most
    /// once a second now.
    #[test]
    fn a_crowded_cache_is_swept_at_most_once_a_second() {
        let cache = ReplayCache::new(60, 2);
        for n in ["a", "b"] {
            assert_eq!(cache.first_use("d", n, 1000, &at(1000)), Recorded::Fresh);
        }
        let swept = cache.lock().sweeps;
        for n in 0..100 {
            assert_eq!(
                cache.first_use("d", &n.to_string(), 1000, &at(1000)),
                Recorded::DeviceFull
            );
        }
        assert_eq!(cache.lock().sweeps, swept, "no sweep in the same second");
        assert_eq!(
            cache.first_use("d", "c", 1001, &at(1001)),
            Recorded::DeviceFull
        );
        assert_eq!(cache.lock().sweeps, swept + 1, "one in the next");
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
        let device = |scope: &str| Caller::Device {
            id: "dev_a".into(),
            name: "laptop".into(),
            scope: scope.into(),
            ephemeral: false,
        };
        assert!(Caller::Operator.is_admin());
        assert!(device("admin").is_admin());
        assert!(!device("sync").is_admin());
        assert!(!device("worker").is_admin());
    }

    #[test]
    fn only_a_worker_device_is_a_worker() {
        let device = |scope: &str| Caller::Device {
            id: "dev_a".into(),
            name: "worker".into(),
            scope: scope.into(),
            ephemeral: false,
        };
        assert!(device("worker").is_worker());
        assert!(!device("admin").is_worker());
        assert!(!device("sync").is_worker());
        assert!(!Caller::Operator.is_worker());
    }

    #[test]
    fn a_scope_this_server_does_not_know_is_not_read_as_sync() {
        assert!(known_scope("sync"));
        assert!(known_scope("admin"));
        assert!(known_scope("worker"));
        for later in ["evaluator", "Worker", "Sync", "", "sync "] {
            assert!(!known_scope(later), "{later:?}");
        }
    }
}
