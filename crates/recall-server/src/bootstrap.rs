//! The one-time code that, beside `RECALL_TOKEN`, registers the first
//! passkey for `/admin`.
//!
//! The token alone once did, at first deploy and again after every
//! `recall-server reset-passkeys`. The token is long-lived and sits in more
//! places than the server (a password manager, `.env`, every machine not yet
//! enrolled as a device), so a leaked copy could plant a passkey that
//! survived rotating the token. The code closes that: it is printed only
//! where the server runs, in its log when it starts with no passkey and by
//! `reset-passkeys`, it works for an hour, and it works once. The server
//! keeps only its SHA-256.

use std::time::Duration;

use anyhow::{Context, Result};
use recall_wire::devices::USER_CODE_ALPHABET;
use time::OffsetDateTime;

use crate::{format_timestamp, Store};

/// How long a code works for. Restarting a server that has no passkey, or
/// running `reset-passkeys` again, prints a new one.
pub const TTL: Duration = Duration::from_secs(60 * 60);

/// How many characters a code has, not counting its dashes: 16 from a
/// 20-letter alphabet is 69 bits, and the token is needed as well.
const LENGTH: usize = 16;

/// A code just issued, for printing: [`BootstrapCode::instructions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapCode {
    /// As it is shown and typed: `BCDF-GHJK-LMNP-QRST`.
    pub code: String,
    /// When it stops working, in [`crate::now`]'s format.
    pub expires_at: String,
}

impl BootstrapCode {
    /// What the owner is told: where to use the code, and until when.
    /// `admin_url` is where the page is, when the server knows.
    pub fn instructions(&self, admin_url: Option<&str>) -> String {
        let page = admin_url.map_or_else(|| "/admin".to_string(), |u| format!("{u}/admin"));
        format!(
            "No passkey is registered for /admin. To register the first, open {page} and \
             enter RECALL_TOKEN and this one-time bootstrap code:\n\n    {}\n\n\
             It works once, until {} (an hour). Restarting the server while no passkey \
             exists, or running `recall-server reset-passkeys`, prints a new one.",
            self.code, self.expires_at
        )
    }
}

/// Makes a new code the only one, until [`TTL`] after `now`.
pub fn issue(store: &Store, now: OffsetDateTime) -> Result<BootstrapCode> {
    let (code, hash) = generate()?;
    let expires_at = format_timestamp(now + TTL);
    store
        .set_bootstrap_code(&hash, &format_timestamp(now), &expires_at)
        .context("storing the bootstrap code")?;
    Ok(BootstrapCode { code, expires_at })
}

/// Removes every passkey and admin session, and makes a new code the only
/// one: `recall-server reset-passkeys`. Answers how many passkeys went.
pub fn reset(store: &Store, now: OffsetDateTime) -> Result<(usize, BootstrapCode)> {
    let (code, hash) = generate()?;
    let expires_at = format_timestamp(now + TTL);
    let removed = store.reset_admin_credentials(&hash, &format_timestamp(now), &expires_at)?;
    Ok((removed, BootstrapCode { code, expires_at }))
}

/// The SHA-256 the store knows a typed code by: case, dashes and spaces do
/// not matter. [`None`] for anything that cannot be a code.
pub fn sha256(typed: &str) -> Option<String> {
    let plain: String = typed
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\t'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let ok = plain.len() == LENGTH && plain.bytes().all(|b| USER_CODE_ALPHABET.contains(&b));
    ok.then(|| recall_wire::content_sha256(&format!("recall bootstrap code\0{plain}")))
}

/// A new code, as shown, and its hash. Letters are drawn uniformly: a byte
/// is used only below the largest multiple of 20 that fits.
fn generate() -> Result<(String, String)> {
    let mut plain = String::with_capacity(LENGTH);
    while plain.len() < LENGTH {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        for b in buf {
            if plain.len() < LENGTH && b < 240 {
                plain.push(USER_CODE_ALPHABET[(b % 20) as usize] as char);
            }
        }
    }
    let shown = plain
        .as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("ASCII"))
        .collect::<Vec<_>>()
        .join("-");
    let hash = sha256(&shown).context("a generated bootstrap code did not read back")?;
    Ok((shown, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::BootstrapCode as Check;

    #[test]
    fn a_code_reads_back_however_it_is_typed() {
        let (shown, hash) = generate().unwrap();
        assert_eq!(shown.len(), LENGTH + 3);
        assert_eq!(shown.matches('-').count(), 3);
        assert_eq!(sha256(&shown).as_deref(), Some(hash.as_str()));
        assert_eq!(
            sha256(&shown.to_lowercase()).as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(
            sha256(&shown.replace('-', " ")).as_deref(),
            Some(hash.as_str())
        );
        assert_eq!(sha256(&shown[..shown.len() - 1]), None, "too short");
        assert_eq!(sha256("AAAA-AAAA-AAAA-AAAA"), None, "not the alphabet");
        assert_eq!(sha256(""), None);
        assert_ne!(generate().unwrap().0, shown);
    }

    #[test]
    fn an_issued_code_works_for_an_hour_and_the_next_replaces_it() {
        let st = Store::open_in_memory().unwrap();
        let t0 = OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
        let first = issue(&st, t0).unwrap();
        let hash = sha256(&first.code).unwrap();
        let at = |secs: i64| format_timestamp(t0 + time::Duration::seconds(secs));
        assert_eq!(
            st.check_bootstrap_code(&hash, &at(3599)).unwrap(),
            Check::Valid
        );
        assert_eq!(
            st.check_bootstrap_code(&hash, &at(3600)).unwrap(),
            Check::Expired
        );
        let second = issue(&st, t0).unwrap();
        assert_eq!(
            st.check_bootstrap_code(&hash, &at(0)).unwrap(),
            Check::Wrong
        );
        let text = second.instructions(Some("https://recall.example.com"));
        assert!(text.contains(&second.code), "{text}");
        assert!(text.contains("https://recall.example.com/admin"), "{text}");
        assert!(text.contains(&second.expires_at), "{text}");
    }
}
