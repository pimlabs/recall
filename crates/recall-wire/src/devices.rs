//! Devices: how a machine enrols, and how the owner approves, lists and
//! revokes machines and the keys cloud sessions enrol with.
//!
//! Enrolment is OAuth's device flow (RFC 8628) with a key pair in place of
//! a token. A machine generates an Ed25519 key, sends the public half to
//! [`ENROLL_PATH`], and gets a short code. The owner approves the code
//! somewhere already trusted, while the machine polls [`ENROLL_POLL_PATH`];
//! once approved, the poll answers with the machine's device id, and from
//! then on the machine signs its requests (see [`crate::signature`]).
//!
//! A cloud session cannot wait for anyone, so it enrols with an
//! [`EnrollRequest::enroll_key`] instead, the way a Tailscale auth key
//! works: approved at once, `sync` scope only, and ephemeral if the key
//! says so.
//!
//! Everything under `/v1/devices` except enrolling, polling and
//! [`DEVICES_ME_PATH`], and everything under `/v1/enroll-keys`, needs the
//! operator's `RECALL_TOKEN` or a device with [`SCOPE_ADMIN`].

use serde::{Deserialize, Serialize};

use crate::signature::{self, SignatureError, VerifyingKey};

/// `POST`: start enrolling a device. Unauthenticated.
pub const ENROLL_PATH: &str = "/v1/devices/enroll";

/// `POST`: ask whether an enrolment was approved. Unauthenticated; the
/// enrolment id is the secret.
pub const ENROLL_POLL_PATH: &str = "/v1/devices/enroll/poll";

/// `GET`: every device.
pub const DEVICES_PATH: &str = "/v1/devices";

/// `GET`: the device that signed the request, as the server knows it.
pub const DEVICES_ME_PATH: &str = "/v1/devices/me";

/// `POST`: approve a pending enrolment by its user code.
pub const APPROVE_PATH: &str = "/v1/devices/approve";

/// `POST`: refuse a pending enrolment by its user code.
pub const DENY_PATH: &str = "/v1/devices/deny";

/// `GET` lists enrolment keys, `POST` creates one.
pub const ENROLL_KEYS_PATH: &str = "/v1/enroll-keys";

/// `POST`: revoke the device `id`.
pub fn revoke_device_path(id: &str) -> String {
    format!("{DEVICES_PATH}/{id}/revoke")
}

/// `POST`: revoke the enrolment key `id`.
pub fn revoke_enroll_key_path(id: &str) -> String {
    format!("{ENROLL_KEYS_PATH}/{id}/revoke")
}

/// `GET`: what the enrolment waiting with `user_code` asked for, so the
/// approver can compare it with what the machine shows before approving.
pub fn pending_path(user_code: &str) -> String {
    format!("{DEVICES_PATH}/pending/{user_code}")
}

/// A device that may push and pull.
pub const SCOPE_SYNC: &str = "sync";

/// A device that may also approve, list and revoke devices and enrolment
/// keys. It includes [`SCOPE_SYNC`].
pub const SCOPE_ADMIN: &str = "admin";

/// A device that may claim merge jobs and post their results, and nothing
/// else: not push, not pull, not manage devices. It is how `recall-worker`
/// is enrolled; see [`crate::jobs`]. An enrolment key never makes one.
pub const SCOPE_WORKER: &str = "worker";

/// How long a user code stays valid, in seconds: fifteen minutes, as
/// GitHub's device flow gives.
pub const CODE_TTL_SECONDS: u64 = 900;

/// How often a machine may poll, in seconds (RFC 8628 §3.2's default).
pub const POLL_INTERVAL_SECONDS: u64 = 5;

/// What every enrolment key starts with, so one found in a log or a
/// secret scanner's report says what it is.
pub const ENROLL_KEY_PREFIX: &str = "recall-ek-";

/// The characters a user code is made of: RFC 8628 §6.1's base-20 set,
/// consonants only, so no code spells a word and none needs a shift key.
pub const USER_CODE_ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";

/// The longest device name accepted, in characters.
pub const MAX_NAME_CHARS: usize = 64;

/// The longest `agent` accepted, in characters.
pub const MAX_AGENT_CHARS: usize = 256;

/// The longest enrolment key tag accepted, in characters. A device an
/// enrolment key enrols is named after the tag, so it is kept short.
pub const MAX_TAG_CHARS: usize = 32;

/// The longest an enrolment key may live, in days.
pub const MAX_ENROLL_KEY_DAYS: u32 = 365;

/// RFC 8628 §3.5: not approved yet; poll again after the interval.
pub const AUTHORIZATION_PENDING: &str = "authorization_pending";
/// RFC 8628 §3.5: not approved yet, and polled too soon; add five seconds
/// to the interval, for this and every later poll.
pub const SLOW_DOWN: &str = "slow_down";
/// RFC 8628 §3.5: the code expired before anyone approved it.
pub const EXPIRED_TOKEN: &str = "expired_token";
/// RFC 8628 §3.5: the owner refused it, or revoked the device it made.
pub const ACCESS_DENIED: &str = "access_denied";
/// RFC 6749 §5.2: no enrolment has that id.
pub const INVALID_GRANT: &str = "invalid_grant";

/// Formats eight characters of [`USER_CODE_ALPHABET`] the way they are
/// shown: `WDJB-MJHT`.
///
/// Reads what a person typed, too: case, the hyphen, spaces and any other
/// character outside the alphabet are ignored, as RFC 8628 §6.1 suggests,
/// so `wdjb mjht` is the same code. [`None`] unless exactly eight remain.
pub fn normalize_user_code(input: &str) -> Option<String> {
    let chars: Vec<char> = input
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii() && USER_CODE_ALPHABET.contains(&(*c as u8)))
        .collect();
    if chars.len() != 8 {
        return None;
    }
    let (a, b) = chars.split_at(4);
    Some(format!(
        "{}-{}",
        a.iter().collect::<String>(),
        b.iter().collect::<String>()
    ))
}

/// Body of `POST /v1/devices/enroll`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnrollRequest {
    /// What the owner will see this machine as, such as `laptop`.
    pub name: String,
    /// The device's Ed25519 public key: 32 bytes, base64url, no padding.
    pub public_key: String,
    /// The client's `User-Agent`, recorded so the device list can show
    /// which version each machine runs.
    #[serde(default)]
    pub agent: String,
    /// An enrolment key, for a machine that cannot wait for approval. It
    /// is approved at once, with `sync` scope, and named by the server
    /// rather than by `name`: nobody approved it, so it may not choose a
    /// name that passes for another machine's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enroll_key: Option<String>,
}

/// Why an enrolment request was refused before anything was stored. The
/// wording is what the server answers with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnrollError {
    /// No name, or no key.
    #[error("name and public_key are required")]
    Missing,
    /// A name too long, or with a character that could hide what it says.
    #[error("name must be at most 64 characters, with no control, format or invisible characters")]
    Name,
    /// An agent too long, or with a character that could hide what it says.
    #[error(
        "agent must be at most 256 characters, with no control, format or invisible characters"
    )]
    Agent,
    /// Not an acceptable Ed25519 public key.
    #[error("{0}")]
    PublicKey(SignatureError),
}

/// Characters a name must not contain, beyond `char::is_control`'s: the
/// ones that change how the rest of a string displays, or display as
/// nothing at all, so `laptop` followed by a zero-width space, or a name
/// reversed by U+202E, cannot pass for another.
///
/// General categories Cf (format), Zl and Zp (line and paragraph
/// separators), as Python's `unicodedata` lists them for Unicode 14, with
/// the Egyptian format-control block widened to what later versions
/// added; then the invisible characters Unicode marks default-ignorable
/// that are not Cf: the combining grapheme joiner, the Hangul fillers,
/// the Khmer inherent vowels, the Mongolian variation selectors and the
/// variation selectors; and the braille blank, which draws nothing.
const HIDDEN: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0x2800, 0x2800),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
    (0xE0100, 0xE01EF),
];

/// Whether `c` is a control character, or one of the format and invisible
/// characters a name may not contain: Unicode categories Cf, Zl and Zp,
/// and the default-ignorable invisibles outside them.
pub fn is_hidden(c: char) -> bool {
    let cp = u32::from(c);
    c.is_control() || HIDDEN.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp))
}

/// Whether `text` is at most `max` characters, none of them
/// [`is_hidden`]. Device names, agents and enrolment key tags are held to
/// this, since each is shown to a person deciding what to trust.
pub fn displayable(text: &str, max: usize) -> bool {
    text.chars().count() <= max && !text.chars().any(is_hidden)
}

impl EnrollRequest {
    /// Checks the request as both sides see it, and returns the key it
    /// names.
    pub fn validate(&self) -> Result<VerifyingKey, EnrollError> {
        if self.name.trim().is_empty() || self.public_key.is_empty() {
            return Err(EnrollError::Missing);
        }
        if !displayable(&self.name, MAX_NAME_CHARS) {
            return Err(EnrollError::Name);
        }
        if !displayable(&self.agent, MAX_AGENT_CHARS) {
            return Err(EnrollError::Agent);
        }
        signature::parse_public_key(&self.public_key).map_err(EnrollError::PublicKey)
    }
}

/// `POST /v1/devices/enroll` without an enrolment key: RFC 8628 §3.2's
/// device authorization response, with the enrolment id in the place of
/// its `device_code`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollPending {
    /// What the machine polls with. A secret: whoever holds it learns the
    /// device id once approved.
    pub enrollment_id: String,
    /// What the owner approves, formatted as [`normalize_user_code`] does.
    pub user_code: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

/// `POST /v1/devices/enroll` with a valid enrolment key: approved at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollApproved {
    /// The device id, which the machine signs with as `keyid`.
    pub device_id: String,
    /// The name the server gave it: the key's tag, a hyphen, and the start
    /// of the device id.
    pub name: String,
    /// Always [`SCOPE_SYNC`].
    pub scope: String,
    /// Whether the server removes this device after it has been idle for
    /// a while.
    pub ephemeral: bool,
}

/// Body of `POST /v1/devices/enroll/poll`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollPollRequest {
    /// From [`EnrollPending`].
    pub enrollment_id: String,
}

/// `POST /v1/devices/enroll/poll` once approved. Before that, the answer
/// is a 400 whose `error` is one of RFC 8628's codes, such as
/// [`AUTHORIZATION_PENDING`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollPollResponse {
    /// The device id, which the machine signs with as `keyid`.
    pub device_id: String,
    /// [`SCOPE_SYNC`] or [`SCOPE_ADMIN`], as the owner approved it.
    pub scope: String,
}

fn default_scope() -> String {
    SCOPE_SYNC.to_string()
}

/// Body of `POST /v1/devices/approve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveRequest {
    /// The code the machine shows, in any case, with or without its
    /// hyphen.
    pub user_code: String,
    /// [`SCOPE_SYNC`] unless given; [`SCOPE_ADMIN`] to let the device
    /// manage others; [`SCOPE_WORKER`] for `recall-worker`.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// The key fingerprint the approver was shown, by the machine or by
    /// `GET /v1/devices/pending/{user_code}`. When given, the approval is
    /// refused unless the code's key has exactly this fingerprint, which
    /// binds the approval to what the approver actually saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// Body of `POST /v1/devices/deny`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyRequest {
    /// The code the machine shows.
    pub user_code: String,
}

/// `GET /v1/devices/pending/{user_code}`: an enrolment still waiting, as
/// the approver sees it before deciding. Showing enough here that a
/// phished approval gets noticed is what RFC 8628 §5.4 asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingEnrollment {
    /// The code, normalized.
    pub user_code: String,
    /// The name the machine asked to be known by.
    pub name: String,
    /// The `agent` it enrolled with.
    pub agent: String,
    /// Its key's fingerprint (see [`signature::fingerprint`]): the machine
    /// shows the same one, and the two should match.
    pub fingerprint: String,
    /// Seconds until the code can no longer be approved.
    pub expires_in: u64,
}

/// `POST /v1/devices/deny`: what was refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenyResponse {
    /// The code, normalized.
    pub user_code: String,
    /// The name the machine asked to be known by.
    pub name: String,
    /// Always `true`.
    pub denied: bool,
}

/// One device, as `GET /v1/devices` lists it and as approving or revoking
/// one answers.
///
/// Absent timestamps and ids are `null`, never omitted, so every key is
/// always there to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// `dev_` and 26 characters: the `keyid` it signs with.
    pub id: String,
    /// What the owner sees it as.
    pub name: String,
    /// [`SCOPE_SYNC`] or [`SCOPE_ADMIN`].
    pub scope: String,
    /// Whether it is removed after being idle for a while.
    pub ephemeral: bool,
    /// The `agent` it enrolled with.
    pub agent: String,
    /// See [`signature::fingerprint`]: what the machine showed when it
    /// enrolled, to compare against.
    pub fingerprint: String,
    /// Its Ed25519 public key, base64url without padding.
    pub public_key: String,
    /// The enrolment key it enrolled with, or `null` when a person
    /// approved it.
    pub enroll_key_id: Option<String>,
    /// When it was approved.
    pub created_at: String,
    /// When it last made a signed request, to within a minute; `null`
    /// before its first.
    pub last_seen: Option<String>,
    /// When it was revoked; `null` while it is not.
    pub revoked_at: Option<String>,
}

/// `GET /v1/devices/me`: the device that signed the request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// Its id, the `keyid` it signed with.
    pub device_id: String,
    /// Its name.
    pub name: String,
    /// [`SCOPE_SYNC`] or [`SCOPE_ADMIN`].
    pub scope: String,
    /// Whether it is removed after being idle for a while.
    pub ephemeral: bool,
}

/// `GET /v1/devices`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    /// Every device, newest first, revoked ones included.
    pub devices: Vec<Device>,
}

/// Body of `POST /v1/enroll-keys`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollKeyRequest {
    /// A label, such as `cloud`.
    #[serde(default)]
    pub tag: String,
    /// How many days the key can enrol devices for, 1 to 365. Required:
    /// there is no key that never expires.
    pub expires_in_days: u32,
    /// Whether devices enrolled with it are ephemeral. `true` unless given:
    /// a key is for machines that come and go.
    #[serde(default = "default_true")]
    pub ephemeral: bool,
    /// The most devices it may have enrolled and unrevoked at once, 1 or
    /// more; no limit when left out. An ephemeral device swept for being
    /// idle frees its place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_devices: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// Body of `POST /v1/enroll-keys/{id}/revoke`, which may also be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollKeyRevokeRequest {
    /// Also revoke every device the key enrolled. `false` unless given:
    /// revoking a key on its own only stops new enrolments.
    #[serde(default)]
    pub revoke_devices: bool,
}

/// One enrolment key, without the key itself, which is shown only once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollKey {
    /// `ek_` and 16 characters. Not a secret.
    pub id: String,
    /// Its label.
    pub tag: String,
    /// Whether devices enrolled with it are ephemeral.
    pub ephemeral: bool,
    /// The most unrevoked devices it may have enrolled at once; `null` for
    /// no limit.
    pub max_devices: Option<u32>,
    /// When it was made.
    pub created_at: String,
    /// When it stops enrolling devices.
    pub expires_at: String,
    /// When it was revoked; `null` while it is not.
    pub revoked_at: Option<String>,
}

/// `POST /v1/enroll-keys`: the new key, the one time it is ever shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollKeyCreated {
    /// As in [`EnrollKey`].
    pub id: String,
    /// The key: [`ENROLL_KEY_PREFIX`] and 52 characters. The server keeps
    /// only its SHA-256.
    pub key: String,
    /// As in [`EnrollKey`].
    pub tag: String,
    /// As in [`EnrollKey`].
    pub ephemeral: bool,
    /// As in [`EnrollKey`].
    pub max_devices: Option<u32>,
    /// As in [`EnrollKey`].
    pub created_at: String,
    /// As in [`EnrollKey`].
    pub expires_at: String,
}

/// `GET /v1/enroll-keys`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollKeyList {
    /// Every enrolment key, newest first, revoked and expired ones
    /// included.
    pub enroll_keys: Vec<EnrollKey>,
}

/// The `devices` capability in the discovery document: present when the
/// server enrols devices and accepts their signatures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicesCapability {
    /// Where to start enrolling: [`ENROLL_PATH`].
    pub enroll_path: String,
    /// How long a user code lives: [`CODE_TTL_SECONDS`].
    pub code_ttl_seconds: u64,
    /// How often to poll: [`POLL_INTERVAL_SECONDS`].
    pub poll_interval_seconds: u64,
    /// How far a signature's `created` may be from the server's clock:
    /// [`signature::WINDOW_SECONDS`].
    pub signature_window_seconds: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_codes_are_read_the_way_rfc8628_suggests() {
        assert_eq!(
            normalize_user_code("WDJB-MJHT").as_deref(),
            Some("WDJB-MJHT")
        );
        assert_eq!(
            normalize_user_code("wdjbmjht").as_deref(),
            Some("WDJB-MJHT")
        );
        assert_eq!(
            normalize_user_code(" wdjb mjht ").as_deref(),
            Some("WDJB-MJHT")
        );
        // Vowels and digits are not in the alphabet, so they are dropped,
        // and what is left is too short.
        assert_eq!(normalize_user_code("WDJB-MJHA"), None);
        assert_eq!(normalize_user_code("WDJB-MJH"), None);
        assert_eq!(normalize_user_code("WDJB-MJHTB"), None);
        assert_eq!(normalize_user_code(""), None);
    }

    #[test]
    fn enrolment_requests_are_checked_the_same_way_on_both_sides() {
        let key = "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs";
        let ok = EnrollRequest {
            name: "laptop".into(),
            public_key: key.into(),
            agent: "recall/0.4.1 (macos-aarch64)".into(),
            enroll_key: None,
        };
        assert!(ok.validate().is_ok());
        for (req, want) in [
            (
                EnrollRequest {
                    name: "  ".into(),
                    ..ok.clone()
                },
                EnrollError::Missing,
            ),
            (
                EnrollRequest {
                    public_key: String::new(),
                    ..ok.clone()
                },
                EnrollError::Missing,
            ),
            (
                EnrollRequest {
                    name: "x".repeat(65),
                    ..ok.clone()
                },
                EnrollError::Name,
            ),
            (
                EnrollRequest {
                    name: "lap\ntop".into(),
                    ..ok.clone()
                },
                EnrollError::Name,
            ),
            (
                EnrollRequest {
                    agent: "a".repeat(257),
                    ..ok.clone()
                },
                EnrollError::Agent,
            ),
            (
                EnrollRequest {
                    public_key: "short".into(),
                    ..ok.clone()
                },
                EnrollError::PublicKey(SignatureError::PublicKey),
            ),
        ] {
            assert_eq!(req.validate(), Err(want), "{req:?}");
        }
        // Sixty-four characters is the limit, not sixty-four bytes.
        let wide = EnrollRequest {
            name: "é".repeat(64),
            ..ok
        };
        assert!(wide.validate().is_ok());
    }

    #[test]
    fn approve_defaults_to_the_narrow_scope() {
        let req: ApproveRequest = serde_json::from_str(r#"{"user_code":"WDJB-MJHT"}"#).unwrap();
        assert_eq!(req.scope, SCOPE_SYNC);
    }

    #[test]
    fn an_enrolment_key_request_needs_an_expiry() {
        assert!(serde_json::from_str::<EnrollKeyRequest>(r#"{"tag":"cloud"}"#).is_err());
        let req: EnrollKeyRequest = serde_json::from_str(r#"{"expires_in_days":90}"#).unwrap();
        assert_eq!(req.tag, "");
        assert!(req.ephemeral, "a key's devices are ephemeral unless asked");
        assert_eq!(req.max_devices, None);
        let req: EnrollKeyRevokeRequest = serde_json::from_str("{}").unwrap();
        assert!(!req.revoke_devices);
    }

    /// Each character that could make one name pass for another: a
    /// zero-width space, a right-to-left override, an isolate, a byte order
    /// mark, a Hangul filler, a soft hyphen, a variation selector, a tag
    /// character, a line separator, a bell.
    #[test]
    fn a_name_cannot_hide_characters() {
        for hidden in [
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{FEFF}',
            '\u{3164}',
            '\u{00AD}',
            '\u{FE0F}',
            '\u{E0041}',
            '\u{2028}',
            '\u{0007}',
        ] {
            let name = format!("lap{hidden}top");
            assert!(
                !displayable(&name, MAX_NAME_CHARS),
                "{:04X}",
                u32::from(hidden)
            );
        }
        for fine in [
            "laptop",
            "Pim's MacBook Air",
            "büro-rechner",
            "ノートPC",
            "laptop 2",
        ] {
            assert!(displayable(fine, MAX_NAME_CHARS), "{fine}");
        }
    }

    #[test]
    fn device_nulls_are_sent_not_omitted() {
        let text = serde_json::to_string(&Device::default()).unwrap();
        for key in ["enroll_key_id", "last_seen", "revoked_at"] {
            assert!(text.contains(&format!("\"{key}\":null")), "{text}");
        }
    }
}
