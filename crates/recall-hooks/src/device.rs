//! This machine as an enrolled device: its key, and how it signs requests
//! with it.
//!
//! A machine enrols once per server (`recall connect`, or a cloud session's
//! first `recall pull`) and from then on signs every request with an Ed25519
//! key it generated, instead of sending the shared `RECALL_TOKEN`. The wire
//! half of that, what is signed and how, is [`recall_wire::signature`], one
//! implementation shared with the server. What lives here is the client's
//! half: making a key, keeping it, and turning it into a [`Signer`].
//!
//! The key is kept in `~/.recall/device.key`; [`crate::home`] says why a
//! file rather than the OS keychain.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use recall_wire::devices::{EnrollRequest, SCOPE_SYNC};
use recall_wire::signature::{self, SigningKey};

use crate::client::{self, Client, Enrolled};
use crate::home::{DeviceEntry, DevicesLock};

/// The variable a cloud environment holds an authkey in. With it, a
/// session that has no device key enrols itself at its first pull, and is
/// approved at once.
pub const AUTHKEY_VAR: &str = "RECALL_AUTHKEY";

/// Why a device key could not be made or used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The OS would not supply randomness.
    #[error("no randomness to make a key with: {0}")]
    Random(String),
    /// The saved key is not one.
    #[error("the device key saved for this server is damaged; run recall connect to enrol again")]
    Damaged,
    /// The server refused, or could not be reached.
    #[error(transparent)]
    Client(#[from] client::Error),
    /// The server answered an authkey with something other than an
    /// approved device.
    #[error("the server did not approve the authkey at once, which it always should")]
    NotApproved,
    /// Saving the key failed.
    #[error(transparent)]
    Home(#[from] crate::home::Error),
}

/// A device key pair, private half included.
///
/// Never printed: `Debug` names the fingerprint and nothing else.
pub struct DeviceKey(SigningKey);

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.fingerprint())
    }
}

impl DeviceKey {
    /// A new key pair, from the OS's randomness.
    pub fn generate() -> Result<Self, Error> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| Error::Random(e.to_string()))?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// The key a [`DeviceEntry`] holds.
    pub fn from_saved(private_key: &str) -> Result<Self, Error> {
        let bytes = URL_SAFE_NO_PAD
            .decode(private_key.trim())
            .map_err(|_| Error::Damaged)?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| Error::Damaged)?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// The private key as [`DeviceEntry::private_key`] keeps it.
    pub fn to_saved(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0.to_bytes())
    }

    /// The public key as enrolment sends it.
    pub fn public_key(&self) -> String {
        signature::encode_public_key(&self.0.verifying_key())
    }

    /// What a person compares, `SHA256:…`, the way the server shows it.
    pub fn fingerprint(&self) -> String {
        signature::fingerprint(&self.0.verifying_key())
    }

    /// What signs requests as `device_id`.
    pub fn signer(&self, device_id: &str) -> Signer {
        Signer {
            key: self.0.clone(),
            device_id: device_id.to_string(),
        }
    }

    /// The enrolment request for this key.
    pub fn enroll_request(&self, name: &str, authkey: Option<&str>) -> EnrollRequest {
        EnrollRequest {
            name: name.to_string(),
            public_key: self.public_key(),
            agent: recall_wire::discovery::user_agent(),
            authkey: authkey.map(str::to_string),
        }
    }

    /// The entry to save once the server has approved this key as
    /// `device_id`.
    pub fn entry(&self, device_id: &str, name: &str, scope: &str, ephemeral: bool) -> DeviceEntry {
        DeviceEntry {
            device_id: device_id.to_string(),
            name: name.to_string(),
            scope: scope.to_string(),
            ephemeral,
            private_key: self.to_saved(),
        }
    }
}

/// What a [`Client`] signs with: the key, and the device id it is known by.
#[derive(Clone)]
pub struct Signer {
    pub(crate) key: SigningKey,
    pub(crate) device_id: String,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// The signer a saved entry describes.
    pub fn from_entry(entry: &DeviceEntry) -> Result<Self, Error> {
        Ok(DeviceKey::from_saved(&entry.private_key)?.signer(&entry.device_id))
    }

    /// The device id every signature names.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

/// The name a machine asks to enrol as, when nobody chose one: what it
/// labels its pushes with, when the server would accept that, else a plain
/// one. The server names a device enrolled with an authkey itself and
/// ignores this, but still checks it.
pub fn enroll_name(label: &str) -> String {
    let label = label.trim();
    if !label.is_empty()
        && recall_wire::devices::displayable(label, recall_wire::devices::MAX_NAME_CHARS)
    {
        label.to_string()
    } else {
        "recall".to_string()
    }
}

/// Enrols this machine at `url` with an authkey, and saves the key
/// the server approved.
///
/// A cloud session's way in: no code to show and nobody to approve it, so
/// the server approves it at once (Tailscale's `preauthorized`). A fresh key
/// every time, never an old one, so a revoked or swept device is never
/// brought back.
///
/// Takes the held lock rather than the home, so that whoever calls it has
/// looked for a key saved by another process under the same lock first:
/// see [`crate::home::Home::lock_devices`].
pub async fn enrol_with_authkey(
    held: &DevicesLock<'_>,
    url: &str,
    authkey: &str,
    label: &str,
) -> Result<DeviceEntry, Error> {
    let key = DeviceKey::generate()?;
    let client = Client::new(url, "")?;
    let req = key.enroll_request(&enroll_name(label), Some(authkey.trim()));
    let approved = match client.enroll(&req).await? {
        Enrolled::Approved(approved) => approved,
        Enrolled::Pending(_) => return Err(Error::NotApproved),
    };
    let scope = if approved.scope.is_empty() {
        SCOPE_SYNC.to_string()
    } else {
        approved.scope
    };
    let entry = key.entry(
        &approved.device_id,
        &approved.name,
        &scope,
        approved.ephemeral,
    );
    held.save_device(url, entry.clone())?;
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_survives_being_saved_and_read_back() {
        let key = DeviceKey::generate().unwrap();
        let again = DeviceKey::from_saved(&key.to_saved()).unwrap();
        assert_eq!(key.public_key(), again.public_key());
        assert_eq!(key.fingerprint(), again.fingerprint());
        assert!(key.fingerprint().starts_with("SHA256:"));
    }

    #[test]
    fn two_keys_are_never_the_same() {
        let a = DeviceKey::generate().unwrap();
        let b = DeviceKey::generate().unwrap();
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn a_damaged_key_is_an_error_not_a_different_key() {
        assert!(DeviceKey::from_saved("not base64!").is_err());
        assert!(DeviceKey::from_saved("c2hvcnQ").is_err());
    }

    /// The private key must never reach a log line or a test failure.
    #[test]
    fn nothing_prints_the_private_key() {
        let key = DeviceKey::generate().unwrap();
        let saved = key.to_saved();
        let entry = key.entry("dev_x", "laptop", "sync", false);
        for printed in [
            format!("{key:?}"),
            format!("{entry:?}"),
            format!("{:?}", key.signer("dev_x")),
        ] {
            assert!(!printed.contains(&saved), "{printed}");
        }
    }

    #[test]
    fn the_enrolment_name_is_one_the_server_accepts() {
        assert_eq!(enroll_name("laptop"), "laptop");
        assert_eq!(enroll_name("  "), "recall");
        assert_eq!(enroll_name("a\u{202e}b"), "recall");
        assert_eq!(enroll_name(&"x".repeat(65)), "recall");
    }
}
