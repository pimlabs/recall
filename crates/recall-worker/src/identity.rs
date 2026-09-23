//! Who the worker is: its Ed25519 key, and the device id the server gave
//! it once the owner approved it.
//!
//! Kept in one file, `worker-identity.json` in the data directory, readable
//! only by its owner. Whoever reads it can act as the worker, which may
//! claim jobs and so read the two versions of every conflicting file, so
//! it lives on the worker's own volume and is never mounted into the API
//! container.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use recall_wire::signature::{encode_public_key, fingerprint, SigningKey};
use serde::{Deserialize, Serialize};

/// The file's name inside the data directory.
pub const FILE_NAME: &str = "worker-identity.json";

/// The worker's key, and how far its enrolment has got.
#[derive(Clone)]
pub struct Identity {
    key: SigningKey,
    /// `dev_…`, once approved.
    pub device_id: Option<String>,
    /// The enrolment waiting for approval, so a restart keeps polling the
    /// same one rather than asking the owner to approve a second code.
    pub enrollment_id: Option<String>,
    /// That enrolment's code, as the owner types it.
    pub user_code: Option<String>,
}

impl std::fmt::Debug for Identity {
    // Never the private key, not even in a debug print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint())
            .field("device_id", &self.device_id)
            .field("enrollment_id", &self.enrollment_id.as_ref().map(|_| "…"))
            .field("user_code", &self.user_code)
            .finish()
    }
}

/// The file's shape.
#[derive(Serialize, Deserialize)]
struct Stored {
    /// The Ed25519 seed, 32 bytes, base64url without padding.
    private_key: String,
    #[serde(default)]
    device_id: Option<String>,
    #[serde(default)]
    enrollment_id: Option<String>,
    #[serde(default)]
    user_code: Option<String>,
}

impl Identity {
    /// A fresh key from the operating system's randomness.
    pub fn generate() -> std::io::Result<Self> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self::from_seed(seed))
    }

    /// A key from a known seed; tests use this to be repeatable.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
            device_id: None,
            enrollment_id: None,
            user_code: None,
        }
    }

    /// Where the identity lives inside `dir`.
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(FILE_NAME)
    }

    /// Reads the identity in `dir`, or makes and saves a new one when
    /// there is none. A file that exists but does not read is an error,
    /// never silently replaced: replacing it would enrol a second worker
    /// and orphan the first.
    pub fn load_or_create(dir: &Path) -> std::io::Result<Self> {
        let path = Self::path(dir);
        match fs::read(&path) {
            Ok(bytes) => Self::parse(&bytes).map_err(|why| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} does not read: {why}", path.display()),
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                id.save(dir)?;
                Ok(id)
            }
            Err(e) => Err(e),
        }
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let stored: Stored = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let seed = URL_SAFE_NO_PAD
            .decode(stored.private_key.trim())
            .map_err(|e| e.to_string())?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| "private_key is not 32 bytes".to_string())?;
        Ok(Self {
            device_id: stored.device_id,
            enrollment_id: stored.enrollment_id,
            user_code: stored.user_code,
            ..Self::from_seed(seed)
        })
    }

    /// Writes the identity to `dir`, readable by its owner only, through a
    /// temporary file renamed over the old one, so a crash mid-write
    /// leaves the previous identity rather than half of one.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let stored = Stored {
            private_key: URL_SAFE_NO_PAD.encode(self.key.to_bytes()),
            device_id: self.device_id.clone(),
            enrollment_id: self.enrollment_id.clone(),
            user_code: self.user_code.clone(),
        };
        let body = serde_json::to_vec_pretty(&stored).map_err(std::io::Error::other)?;
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        let _ = fs::remove_file(&tmp);
        {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, Self::path(dir))
    }

    /// The signing key.
    pub fn key(&self) -> &SigningKey {
        &self.key
    }

    /// The public key, as the server stores it.
    pub fn public_key(&self) -> String {
        encode_public_key(&self.key.verifying_key())
    }

    /// `SHA256:…`, which the owner compares before approving.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.key.verifying_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_identity_is_saved_and_read_back_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let first = Identity::load_or_create(dir.path()).unwrap();
        let mut again = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.public_key(), again.public_key());
        assert_eq!(again.device_id, None);

        again.device_id = Some("dev_a".into());
        again.save(dir.path()).unwrap();
        let third = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(third.device_id.as_deref(), Some("dev_a"));
        assert_eq!(third.public_key(), first.public_key());
    }

    #[cfg(unix)]
    #[test]
    fn only_its_owner_can_read_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Identity::load_or_create(dir.path()).unwrap();
        let mode = fs::metadata(Identity::path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A damaged file stops the worker rather than enrolling a new one.
    #[test]
    fn a_file_that_does_not_read_is_an_error_not_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(Identity::path(dir.path()), "{").unwrap();
        assert!(Identity::load_or_create(dir.path()).is_err());
    }

    #[test]
    fn the_debug_print_holds_no_key() {
        let id = Identity::from_seed([7; 32]);
        let printed = format!("{id:?}");
        assert!(!printed.contains(&URL_SAFE_NO_PAD.encode([7u8; 32])));
        assert!(printed.contains("SHA256:"));
    }
}
