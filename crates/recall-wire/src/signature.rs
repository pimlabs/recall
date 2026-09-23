//! Signing a request, and checking one: RFC 9421 HTTP Message Signatures
//! with Ed25519, over an RFC 9530 `Content-Digest` of the body.
//!
//! A device holds an Ed25519 key pair it generated; the private key never
//! leaves it. Every request it sends carries three headers:
//!
//! ```text
//! Content-Digest: sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:
//! Signature-Input: sig1=("@method" "@authority" "@path" "@query" "content-digest" "recall-protocol");created=1790000000;keyid="dev_…";nonce="…";alg="ed25519"
//! Signature: sig1=:…:
//! ```
//!
//! No secret travels with the request, so one copied out of a proxy log is
//! worth nothing once its `created` window has passed, and the nonce stops
//! it being replayed inside the window.
//!
//! # Why these components
//!
//! The design sketch covered `@target-uri`. It is the right idea and the
//! wrong component for this deployment: Traefik terminates TLS, so the
//! server sees plain HTTP and cannot reconstruct the scheme the client
//! signed. It does pass `Host` through, so the signature covers the same
//! URI in the pieces the server can see: `@authority`, `@path` and
//! `@query`. `content-digest` binds the body, and is always sent and
//! always covered, a GET carrying the digest of an empty body, so the
//! server never has to decide whether a request "has" a body.
//! `recall-protocol` is covered so the protocol a request claims cannot be
//! changed in transit.
//!
//! # What is implemented
//!
//! Only the subset Recall sends: one signature per request, labelled
//! [`LABEL`]; covered components that are plain strings, with no component
//! parameters; the four derived components above; and header fields by
//! name. Structured-field parsing follows RFC 8941 for the types those
//! headers use. Anything else fails to parse, and the server refuses a
//! request whose signature headers do not parse with a 401 that names the
//! header, rather than falling back to treating it as unsigned.
//!
//! Ed25519 is `ed25519-dalek`, which is pure Rust: the server is built
//! static against musl and links no C crypto library.

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use ed25519_dalek::{Signature, Signer};
use sha2::{Digest, Sha256};

pub use ed25519_dalek::{SigningKey, VerifyingKey};

/// The header carrying the body's digest (RFC 9530).
pub const CONTENT_DIGEST_HEADER: &str = "content-digest";

/// The header describing what a signature covers (RFC 9421 §4.1).
pub const SIGNATURE_INPUT_HEADER: &str = "signature-input";

/// The header carrying the signature itself (RFC 9421 §4.2).
pub const SIGNATURE_HEADER: &str = "signature";

/// The label Recall signs under, and the only one the server reads.
pub const LABEL: &str = "sig1";

/// The one algorithm Recall signs with, by its RFC 9421 registry name.
pub const ALGORITHM: &str = "ed25519";

/// What every Recall signature covers, in the order the client signs them.
/// The server requires all of them and accepts them in any order.
pub const COVERED_COMPONENTS: [&str; 6] = [
    "@method",
    "@authority",
    "@path",
    "@query",
    CONTENT_DIGEST_HEADER,
    crate::PROTOCOL_HEADER,
];

/// How far `created` may be from the server's clock, either way, in
/// seconds. The server also remembers every nonce for this long.
pub const WINDOW_SECONDS: u64 = 60;

/// The longest nonce the server accepts. It keeps each one in memory for
/// [`WINDOW_SECONDS`], so it is bounded.
pub const MAX_NONCE_LEN: usize = 128;

/// Why a signature, or something it depends on, was not accepted. The
/// messages are safe to show a user and name what to fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// A header was present but did not parse.
    #[error("malformed {0} header")]
    Malformed(&'static str),
    /// `Signature-Input` or `Signature` has no member labelled [`LABEL`].
    #[error("no signature labelled sig1")]
    NoLabel,
    /// A parameter the server requires is absent.
    #[error("the signature has no {0} parameter")]
    MissingParameter(&'static str),
    /// A component Recall requires is not covered.
    #[error(
        "the signature must cover @method, @authority, @path, @query, content-digest and recall-protocol"
    )]
    NotCovered,
    /// A component is listed twice (RFC 9421 §2.5, step 2.1).
    #[error("the signature covers {0} twice")]
    Duplicate(String),
    /// A covered component has no value in this request.
    #[error("the signature covers {0}, which this request does not have")]
    MissingComponent(String),
    /// `alg` names something other than [`ALGORITHM`].
    #[error("unsupported signature algorithm {0:?}")]
    Algorithm(String),
    /// The nonce is empty or longer than [`MAX_NONCE_LEN`].
    #[error("the nonce must be 1 to 128 characters")]
    Nonce,
    /// `created` is outside the window.
    #[error(
        "signature created {skew} seconds from the server's clock, more than the {window} allowed; check this machine's clock"
    )]
    Clock {
        /// How far off it was, in seconds.
        skew: u64,
        /// How far off it may be.
        window: u64,
    },
    /// `expires` has passed.
    #[error("the signature has expired")]
    Expired,
    /// `Content-Digest` has no `sha-256` member.
    #[error("content-digest has no sha-256 value")]
    NoDigest,
    /// `Content-Digest` does not match the body received.
    #[error("content-digest does not match the body")]
    DigestMismatch,
    /// The signature base holds something that is not ASCII (§2.5, step 4).
    #[error("the signature base is not ASCII")]
    NotAscii,
    /// The signature does not verify with the device's key.
    #[error("the signature does not verify")]
    BadSignature,
    /// A public key is not an acceptable Ed25519 key.
    #[error("public_key must be an Ed25519 public key: 32 bytes, base64url without padding")]
    PublicKey,
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

/// A public key as it travels in JSON: the raw 32 bytes, base64url, no
/// padding.
pub fn encode_public_key(key: &VerifyingKey) -> String {
    URL_SAFE_NO_PAD.encode(key.as_bytes())
}

/// Reads a public key sent as [`encode_public_key`] writes it.
///
/// A key of small order is refused: every signature "verifies" under one,
/// so accepting it would let whoever enrolled it sign as anyone holding it.
pub fn parse_public_key(text: &str) -> Result<VerifyingKey, SignatureError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| SignatureError::PublicKey)?;
    let raw: [u8; 32] = bytes.try_into().map_err(|_| SignatureError::PublicKey)?;
    let key = VerifyingKey::from_bytes(&raw).map_err(|_| SignatureError::PublicKey)?;
    if key.is_weak() {
        return Err(SignatureError::PublicKey);
    }
    Ok(key)
}

/// What a person compares to confirm two screens show the same key:
/// `SHA256:` and the unpadded base64 of the key's SHA-256, the shape
/// `ssh-keygen -l` prints.
pub fn fingerprint(key: &VerifyingKey) -> String {
    format!(
        "SHA256:{}",
        STANDARD_NO_PAD.encode(Sha256::digest(key.as_bytes()))
    )
}

// ---------------------------------------------------------------------------
// Content-Digest (RFC 9530)
// ---------------------------------------------------------------------------

/// The `Content-Digest` value for `body`: its SHA-256 as a structured-field
/// byte sequence. An empty body has a digest too, and a GET sends it.
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(body)))
}

/// Checks a received `Content-Digest` against the body that arrived.
///
/// Only `sha-256` is read. Other algorithms may be present and are ignored,
/// as RFC 9530 §2 allows a recipient to.
pub fn check_content_digest(field: &str, body: &[u8]) -> Result<(), SignatureError> {
    if sha256_of(field)?.as_slice() == Sha256::digest(body).as_slice() {
        Ok(())
    } else {
        Err(SignatureError::DigestMismatch)
    }
}

/// The `sha-256` value a `Content-Digest` carries.
fn sha256_of(field: &str) -> Result<Vec<u8>, SignatureError> {
    let dict = sf::dictionary(field).ok_or(SignatureError::Malformed("content-digest"))?;
    dict.into_iter()
        .find(|(k, _)| k == "sha-256")
        .and_then(|(_, m)| match m {
            sf::Member::Item(sf::Item::Bytes(b), _) => Some(b),
            _ => None,
        })
        .ok_or(SignatureError::NoDigest)
}

// ---------------------------------------------------------------------------
// the request a signature is about
// ---------------------------------------------------------------------------

/// The parts of a request the derived components come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target<'a> {
    /// The method, as sent: `GET`, `POST`.
    pub method: &'a str,
    /// Host and port, already through [`normalize_authority`].
    pub authority: &'a str,
    /// The path, percent-encoding untouched, without the query.
    pub path: &'a str,
    /// The query string without its `?`, as sent; [`None`] when there is
    /// none.
    pub query: Option<&'a str>,
}

impl Target<'_> {
    /// A derived component's value, or [`None`] for one Recall does not
    /// implement.
    fn derived(&self, name: &str) -> Option<String> {
        match name {
            "@method" => Some(self.method.to_string()),
            "@authority" => Some(self.authority.to_string()),
            // RFC 9421 §2.2.6: an empty path is a single slash.
            "@path" => Some(if self.path.is_empty() {
                "/".to_string()
            } else {
                self.path.to_string()
            }),
            // §2.2.7: the leading "?" is part of the value, and a request
            // with no query covers "?" alone.
            "@query" => Some(format!("?{}", self.query.unwrap_or(""))),
            _ => None,
        }
    }
}

/// The `@authority` value both sides compute: host and port, lowercased,
/// without a default port (RFC 9421 §2.2.3).
///
/// The server cannot tell which port was the default, because behind a
/// TLS-terminating proxy it never learns the scheme. It drops both `:80`
/// and `:443`, and so does the client, so the two agree whatever the
/// scheme; a server reachable on 443 and 80 at one host is one server.
pub fn normalize_authority(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    for default in [":443", ":80"] {
        if let Some(stripped) = host.strip_suffix(default) {
            return stripped.to_string();
        }
    }
    host
}

// ---------------------------------------------------------------------------
// Signature-Input
// ---------------------------------------------------------------------------

/// A value in a signature's parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Param {
    /// An integer, such as `created`.
    Integer(i64),
    /// A string, such as `keyid`.
    String(String),
    /// A token.
    Token(String),
    /// A boolean; a parameter with no value is `true`.
    Boolean(bool),
    /// A byte sequence.
    Bytes(Vec<u8>),
}

/// One signature's covered components and parameters: a
/// `Signature-Input` member, and the `@signature-params` line of the
/// signature base.
///
/// The order of both lists is kept exactly as received, because the
/// signature base re-serializes them and any other order is a different
/// base (RFC 9421 §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInput {
    /// Component names, in order.
    pub components: Vec<String>,
    /// Parameters, in order.
    pub params: Vec<(String, Param)>,
}

impl SignatureInput {
    /// What the Recall client signs: [`COVERED_COMPONENTS`], then
    /// `created`, `keyid`, `nonce` and `alg`, in that order.
    pub fn recall(created: i64, keyid: &str, nonce: &str) -> Self {
        Self {
            components: COVERED_COMPONENTS.iter().map(|c| c.to_string()).collect(),
            params: vec![
                ("created".to_string(), Param::Integer(created)),
                ("keyid".to_string(), Param::String(keyid.to_string())),
                ("nonce".to_string(), Param::String(nonce.to_string())),
                ("alg".to_string(), Param::String(ALGORITHM.to_string())),
            ],
        }
    }

    /// Reads the member labelled `label` from a `Signature-Input` value.
    pub fn parse(field: &str, label: &str) -> Result<Self, SignatureError> {
        const WHAT: &str = "signature-input";
        let dict = sf::dictionary(field).ok_or(SignatureError::Malformed(WHAT))?;
        let (items, params) = match dict.into_iter().find(|(k, _)| k == label) {
            Some((_, sf::Member::InnerList(items, params))) => (items, params),
            Some(_) => return Err(SignatureError::Malformed(WHAT)),
            None => return Err(SignatureError::NoLabel),
        };
        let mut components = Vec::with_capacity(items.len());
        for (item, item_params) in items {
            // Component parameters (`;req`, `;key=`, `;sf`...) are not
            // part of what Recall signs.
            match item {
                sf::Item::String(name) if item_params.is_empty() => components.push(name),
                _ => return Err(SignatureError::Malformed(WHAT)),
            }
        }
        Ok(Self {
            components,
            params: params.into_iter().map(|(k, v)| (k, v.into())).collect(),
        })
    }

    /// The inner list, with parameters, exactly as it appears after
    /// `sig1=` and after `"@signature-params": `.
    pub fn serialize(&self) -> String {
        let mut out = String::from("(");
        for (i, c) in self.components.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            sf::write_string(&mut out, c);
        }
        out.push(')');
        for (key, value) in &self.params {
            out.push(';');
            out.push_str(key);
            match value {
                Param::Boolean(true) => {}
                Param::Boolean(false) => out.push_str("=?0"),
                Param::Integer(n) => {
                    out.push('=');
                    out.push_str(&n.to_string());
                }
                Param::String(s) => {
                    out.push('=');
                    sf::write_string(&mut out, s);
                }
                Param::Token(t) => {
                    out.push('=');
                    out.push_str(t);
                }
                Param::Bytes(b) => {
                    out.push_str("=:");
                    out.push_str(&STANDARD.encode(b));
                    out.push(':');
                }
            }
        }
        out
    }

    fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    fn string_param(&self, name: &str) -> Option<&str> {
        match self.param(name)? {
            Param::String(s) => Some(s),
            _ => None,
        }
    }

    fn integer_param(&self, name: &str) -> Option<i64> {
        match self.param(name)? {
            Param::Integer(n) => Some(*n),
            _ => None,
        }
    }

    /// `created`, as a UNIX time.
    pub fn created(&self) -> Option<i64> {
        self.integer_param("created")
    }

    /// `keyid`: for Recall, the device id.
    pub fn keyid(&self) -> Option<&str> {
        self.string_param("keyid")
    }

    /// `nonce`.
    pub fn nonce(&self) -> Option<&str> {
        self.string_param("nonce")
    }

    /// `alg`, when present.
    pub fn alg(&self) -> Option<&str> {
        self.string_param("alg")
    }

    /// Builds the signature base (RFC 9421 §2.5), taking each covered
    /// component's value from `value`.
    pub fn signature_base(
        &self,
        value: impl Fn(&str) -> Option<String>,
    ) -> Result<String, SignatureError> {
        let mut out = String::new();
        let mut seen: Vec<&str> = Vec::with_capacity(self.components.len());
        for name in &self.components {
            if seen.contains(&name.as_str()) {
                return Err(SignatureError::Duplicate(name.clone()));
            }
            seen.push(name);
            let v = value(name).ok_or_else(|| SignatureError::MissingComponent(name.clone()))?;
            // A value spanning lines would forge a line of its own.
            if v.contains('\n') || v.contains('\r') {
                return Err(SignatureError::MissingComponent(name.clone()));
            }
            sf::write_string(&mut out, name);
            out.push_str(": ");
            out.push_str(&v);
            out.push('\n');
        }
        out.push_str("\"@signature-params\": ");
        out.push_str(&self.serialize());
        if !out.is_ascii() {
            return Err(SignatureError::NotAscii);
        }
        Ok(out)
    }

    /// Checks the parameters and coverage against what Recall requires of
    /// every signature, given the verifier's clock as a UNIX time.
    pub fn check_profile(&self, now: i64, window: u64) -> Result<(), SignatureError> {
        // RFC 9421 §3.2 step 6.5: an algorithm named in the signature has
        // to agree with the key's. The key is Ed25519, so only that name
        // may appear; leaving it out is allowed, the key decides.
        if let Some(param) = self.param("alg") {
            match param {
                Param::String(a) if a == ALGORITHM => {}
                Param::String(a) => return Err(SignatureError::Algorithm(a.clone())),
                _ => return Err(SignatureError::Algorithm(String::new())),
            }
        }
        if self.keyid().is_none() {
            return Err(SignatureError::MissingParameter("keyid"));
        }
        let nonce = self
            .nonce()
            .ok_or(SignatureError::MissingParameter("nonce"))?;
        if nonce.is_empty() || nonce.len() > MAX_NONCE_LEN {
            return Err(SignatureError::Nonce);
        }
        let created = self
            .created()
            .ok_or(SignatureError::MissingParameter("created"))?;
        let skew = now.abs_diff(created);
        if skew > window {
            return Err(SignatureError::Clock { skew, window });
        }
        if let Some(expires) = self.integer_param("expires") {
            if expires < now {
                return Err(SignatureError::Expired);
            }
        }
        if !COVERED_COMPONENTS
            .iter()
            .all(|c| self.components.iter().any(|have| have == c))
        {
            return Err(SignatureError::NotCovered);
        }
        Ok(())
    }
}

impl From<sf::Item> for Param {
    fn from(item: sf::Item) -> Self {
        match item {
            sf::Item::Integer(n) => Param::Integer(n),
            sf::Item::String(s) => Param::String(s),
            sf::Item::Token(t) => Param::Token(t),
            sf::Item::Boolean(b) => Param::Boolean(b),
            sf::Item::Bytes(b) => Param::Bytes(b),
        }
    }
}

/// Reads the signature labelled `label` from a `Signature` value.
pub fn parse_signature(field: &str, label: &str) -> Result<Vec<u8>, SignatureError> {
    let dict = sf::dictionary(field).ok_or(SignatureError::Malformed("signature"))?;
    match dict.into_iter().find(|(k, _)| k == label) {
        Some((_, sf::Member::Item(sf::Item::Bytes(b), _))) => Ok(b),
        Some(_) => Err(SignatureError::Malformed("signature")),
        None => Err(SignatureError::NoLabel),
    }
}

// ---------------------------------------------------------------------------
// signing and verifying
// ---------------------------------------------------------------------------

/// Signs a signature base with Ed25519 (RFC 9421 §3.3.6).
pub fn sign(key: &SigningKey, base: &str) -> Vec<u8> {
    key.sign(base.as_bytes()).to_bytes().to_vec()
}

/// Verifies an Ed25519 signature over a signature base.
///
/// Strict verification: it also refuses the non-canonical encodings plain
/// RFC 8032 verification lets through, so one request cannot be given two
/// valid signatures.
pub fn verify(key: &VerifyingKey, base: &str, signature: &[u8]) -> Result<(), SignatureError> {
    let bytes: [u8; 64] = signature
        .try_into()
        .map_err(|_| SignatureError::BadSignature)?;
    key.verify_strict(base.as_bytes(), &Signature::from_bytes(&bytes))
        .map_err(|_| SignatureError::BadSignature)
}

/// A covered component's value: derived ones from `target`, anything else
/// from the header of that name through `field`.
fn component_value(
    target: &Target<'_>,
    field: &dyn Fn(&str) -> Option<String>,
    name: &str,
) -> Option<String> {
    if name.starts_with('@') {
        target.derived(name)
    } else {
        field(name).map(|v| v.trim().to_string())
    }
}

/// The three headers a signed request carries, as [`sign_request`] makes
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedHeaders {
    /// For [`CONTENT_DIGEST_HEADER`].
    pub content_digest: String,
    /// For [`SIGNATURE_INPUT_HEADER`].
    pub signature_input: String,
    /// For [`SIGNATURE_HEADER`].
    pub signature: String,
}

/// Signs a request the way the Recall client does: [`COVERED_COMPONENTS`]
/// over `target`, with `protocol` as the `Recall-Protocol` header the
/// request also sends, and the digest of `body`.
///
/// `created` is the current UNIX time and `nonce` a fresh random string
/// that is never reused; both are the caller's so this stays
/// deterministic.
pub fn sign_request(
    key: &SigningKey,
    keyid: &str,
    target: &Target<'_>,
    protocol: &str,
    body: &[u8],
    created: i64,
    nonce: &str,
) -> Result<SignedHeaders, SignatureError> {
    let digest = content_digest(body);
    let input = SignatureInput::recall(created, keyid, nonce);
    let field = |name: &str| match name {
        CONTENT_DIGEST_HEADER => Some(digest.clone()),
        crate::PROTOCOL_HEADER => Some(protocol.to_string()),
        _ => None,
    };
    let base = input.signature_base(|name| component_value(target, &field, name))?;
    Ok(SignedHeaders {
        signature_input: format!("{LABEL}={}", input.serialize()),
        signature: format!("{LABEL}=:{}:", STANDARD.encode(sign(key, &base))),
        content_digest: digest,
    })
}

/// A signed request's headers as they arrived: everything
/// [`verify_headers`] reads.
pub struct Received<'a> {
    /// The `Signature-Input` member labelled [`LABEL`].
    pub input: &'a SignatureInput,
    /// The `Signature` member labelled [`LABEL`].
    pub signature: &'a [u8],
    /// Where the derived components come from.
    pub target: Target<'a>,
    /// A header's value by lowercase name, with repeated headers joined by
    /// `", "` (RFC 9110 §5.3).
    pub field: &'a dyn Fn(&str) -> Option<String>,
}

/// Everything about a signed request that can be checked from its headers
/// alone, given the verifier's clock as a UNIX time: the profile
/// ([`SignatureInput::check_profile`]), a `Content-Digest` with a sha-256
/// value, and the signature against `key`.
///
/// The signature covers the `Content-Digest` header, not the body, so it
/// can be verified before a byte of the body is read: a server need only
/// read a body for a request its device really signed. The body must then
/// be checked against the digest with [`check_content_digest`], or the
/// signature proves nothing about it; [`verify_request`] does both.
///
/// What needs the server's state, which key a `keyid` names and whether a
/// nonce was already used, is the caller's.
pub fn verify_headers(
    req: &Received<'_>,
    key: &VerifyingKey,
    now: i64,
    window: u64,
) -> Result<(), SignatureError> {
    req.input.check_profile(now, window)?;
    let digest = (req.field)(CONTENT_DIGEST_HEADER)
        .ok_or_else(|| SignatureError::MissingComponent(CONTENT_DIGEST_HEADER.to_string()))?;
    sha256_of(&digest)?;
    let base = req
        .input
        .signature_base(|name| component_value(&req.target, req.field, name))?;
    verify(key, &base, req.signature)
}

/// [`verify_headers`], then the body against `Content-Digest`: the whole
/// of what can be checked without state.
pub fn verify_request(
    req: &Received<'_>,
    body: &[u8],
    key: &VerifyingKey,
    now: i64,
    window: u64,
) -> Result<(), SignatureError> {
    verify_headers(req, key, now, window)?;
    let digest = (req.field)(CONTENT_DIGEST_HEADER).unwrap_or_default();
    check_content_digest(&digest, body)
}

// ---------------------------------------------------------------------------
// structured fields (RFC 8941), the part these headers use
// ---------------------------------------------------------------------------

mod sf {
    //! Parsing Dictionaries of Items and Inner Lists, per RFC 8941 §4.2.
    //! Decimals are not supported: nothing Recall reads uses them, and a
    //! field that fails to parse is ignored as a whole (§4.2).

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Item {
        Integer(i64),
        String(String),
        Token(String),
        Boolean(bool),
        Bytes(Vec<u8>),
    }

    pub(super) type Params = Vec<(String, Item)>;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Member {
        Item(Item, Params),
        InnerList(Vec<(Item, Params)>, Params),
    }

    /// §4.1.6: a string, quoted, with `"` and `\` escaped.
    pub(super) fn write_string(out: &mut String, s: &str) {
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    }

    struct Parser<'a> {
        s: &'a [u8],
        at: usize,
    }

    /// §4.2 with §4.2.2: `None` for anything that does not parse.
    pub(super) fn dictionary(input: &str) -> Option<Vec<(String, Member)>> {
        if !input.is_ascii() {
            return None;
        }
        let mut p = Parser {
            s: input.as_bytes(),
            at: 0,
        };
        p.skip_sp();
        let mut dict: Vec<(String, Member)> = Vec::new();
        while !p.done() {
            let key = p.key()?;
            let member = if p.eat(b'=') {
                p.item_or_inner_list()?
            } else {
                Member::Item(Item::Boolean(true), p.params()?)
            };
            // §4.2.2 step 2.4: a later duplicate replaces the earlier one.
            match dict.iter_mut().find(|(k, _)| *k == key) {
                Some(existing) => existing.1 = member,
                None => dict.push((key, member)),
            }
            p.skip_ows();
            if p.done() {
                break;
            }
            if !p.eat(b',') {
                return None;
            }
            p.skip_ows();
            if p.done() {
                return None;
            }
        }
        Some(dict)
    }

    impl Parser<'_> {
        fn done(&self) -> bool {
            self.at >= self.s.len()
        }

        fn peek(&self) -> Option<u8> {
            self.s.get(self.at).copied()
        }

        fn eat(&mut self, c: u8) -> bool {
            if self.peek() == Some(c) {
                self.at += 1;
                true
            } else {
                false
            }
        }

        fn skip_sp(&mut self) {
            while self.peek() == Some(b' ') {
                self.at += 1;
            }
        }

        fn skip_ows(&mut self) {
            while matches!(self.peek(), Some(b' ' | b'\t')) {
                self.at += 1;
            }
        }

        /// §4.2.3.3.
        fn key(&mut self) -> Option<String> {
            let start = self.at;
            match self.peek()? {
                b'a'..=b'z' | b'*' => self.at += 1,
                _ => return None,
            }
            while matches!(
                self.peek(),
                Some(b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'*')
            ) {
                self.at += 1;
            }
            Some(String::from_utf8_lossy(&self.s[start..self.at]).into_owned())
        }

        /// §4.2.1.1.
        fn item_or_inner_list(&mut self) -> Option<Member> {
            if self.eat(b'(') {
                let mut items = Vec::new();
                loop {
                    self.skip_sp();
                    if self.eat(b')') {
                        return Some(Member::InnerList(items, self.params()?));
                    }
                    if self.done() {
                        return None;
                    }
                    let item = self.bare_item()?;
                    items.push((item, self.params()?));
                    if !matches!(self.peek(), Some(b' ' | b')')) {
                        return None;
                    }
                }
            }
            let item = self.bare_item()?;
            Some(Member::Item(item, self.params()?))
        }

        /// §4.2.3.2.
        fn params(&mut self) -> Option<Params> {
            let mut params: Params = Vec::new();
            while self.eat(b';') {
                self.skip_sp();
                let key = self.key()?;
                let value = if self.eat(b'=') {
                    self.bare_item()?
                } else {
                    Item::Boolean(true)
                };
                match params.iter_mut().find(|(k, _)| *k == key) {
                    Some(existing) => existing.1 = value,
                    None => params.push((key, value)),
                }
            }
            Some(params)
        }

        /// §4.2.3.1.
        fn bare_item(&mut self) -> Option<Item> {
            match self.peek()? {
                b'-' | b'0'..=b'9' => self.integer(),
                b'"' => self.string(),
                b':' => self.bytes(),
                b'?' => self.boolean(),
                b'A'..=b'Z' | b'a'..=b'z' | b'*' => self.token(),
                _ => None,
            }
        }

        /// §4.2.4, integers only.
        fn integer(&mut self) -> Option<Item> {
            let negative = self.eat(b'-');
            let start = self.at;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            let digits = &self.s[start..self.at];
            if digits.is_empty() || digits.len() > 15 || self.peek() == Some(b'.') {
                return None;
            }
            let n: i64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
            Some(Item::Integer(if negative { -n } else { n }))
        }

        /// §4.2.5.
        fn string(&mut self) -> Option<Item> {
            self.at += 1;
            let mut out = String::new();
            loop {
                let c = self.peek()?;
                self.at += 1;
                match c {
                    b'"' => return Some(Item::String(out)),
                    b'\\' => match self.peek()? {
                        c @ (b'"' | b'\\') => {
                            self.at += 1;
                            out.push(c as char);
                        }
                        _ => return None,
                    },
                    0x20..=0x7e => out.push(c as char),
                    _ => return None,
                }
            }
        }

        /// §4.2.6.
        fn token(&mut self) -> Option<Item> {
            let start = self.at;
            self.at += 1;
            while let Some(c) = self.peek() {
                let tchar = c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~:/".contains(&c);
                if !tchar {
                    break;
                }
                self.at += 1;
            }
            Some(Item::Token(
                String::from_utf8_lossy(&self.s[start..self.at]).into_owned(),
            ))
        }

        /// §4.2.7. Padding is optional on the way in, as the RFC asks of
        /// parsers.
        fn bytes(&mut self) -> Option<Item> {
            self.at += 1;
            let start = self.at;
            while self.peek()? != b':' {
                let c = self.peek()?;
                if !(c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=') {
                    return None;
                }
                self.at += 1;
            }
            let b64 = &self.s[start..self.at];
            self.at += 1;
            const LENIENT: GeneralPurpose = GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                GeneralPurposeConfig::new()
                    .with_decode_padding_mode(DecodePaddingMode::Indifferent)
                    .with_decode_allow_trailing_bits(true),
            );
            LENIENT.decode(b64).ok().map(Item::Bytes)
        }

        /// §4.2.8.
        fn boolean(&mut self) -> Option<Item> {
            self.at += 1;
            let v = match self.peek()? {
                b'0' => false,
                b'1' => true,
                _ => return None,
            };
            self.at += 1;
            Some(Item::Boolean(v))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Known answers from the RFCs
    //
    // RFC 9421, 9530 and 8941 were read from the plain-text copies vendored
    // in https://github.com/mnot/rfc-refs (rfcs/rfc9421.txt, rfc9530.txt,
    // rfc8941.txt at commit b4d8a7b2e544c78c754275f08535d06234bbd9c2), and
    // RFC 9421's appendix was cross-checked against the working group's
    // source for it, https://github.com/httpwg/http-extensions
    // archive/draft-ietf-httpbis-message-signatures.md at commit
    // f66f269dfd744e778f53fb38ce96ce04a1323fff. rfc-editor.org itself was
    // not reachable from where these were written. Lines the RFC wraps
    // under RFC 8792 ('\' at the end of a line) are joined here.
    // -----------------------------------------------------------------------

    /// RFC 9421 Appendix B.1.4, `test-key-ed25519`, from its JWK form.
    const TEST_KEY_ED25519_D: &str = "n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU";
    const TEST_KEY_ED25519_X: &str = "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs";
    /// The same key's PKCS #8 PEM bodies, from the same appendix.
    const TEST_KEY_ED25519_PUBLIC_PEM: &str =
        "MCowBQYDK2VwAyEAJrQLj5P/89iXES9+vFgrIy29clF9CC/oPPsw3c5D0bs=";
    const TEST_KEY_ED25519_PRIVATE_PEM: &str =
        "MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF";

    fn test_key() -> SigningKey {
        let d: [u8; 32] = URL_SAFE_NO_PAD
            .decode(TEST_KEY_ED25519_D)
            .unwrap()
            .try_into()
            .unwrap();
        SigningKey::from_bytes(&d)
    }

    /// B.1.4: the JWK's `d` is the private key whose public half is `x`,
    /// and both agree with the PEM encodings of the same key: the raw key
    /// is the last 32 bytes of each DER body.
    #[test]
    fn rfc9421_b_1_4_test_key_ed25519_is_one_key_in_both_encodings() {
        let key = test_key();
        assert_eq!(encode_public_key(&key.verifying_key()), TEST_KEY_ED25519_X);

        let public_der = STANDARD.decode(TEST_KEY_ED25519_PUBLIC_PEM).unwrap();
        assert_eq!(
            &public_der[public_der.len() - 32..],
            key.verifying_key().as_bytes()
        );
        let private_der = STANDARD.decode(TEST_KEY_ED25519_PRIVATE_PEM).unwrap();
        assert_eq!(
            &private_der[private_der.len() - 32..],
            key.to_bytes().as_slice()
        );

        assert_eq!(
            parse_public_key(TEST_KEY_ED25519_X).unwrap(),
            key.verifying_key()
        );
    }

    /// B.2.6: "Signing a Request Using ed25519". Ed25519 is deterministic,
    /// so signing the same base with the same key must give exactly the
    /// RFC's bytes, not merely something that verifies.
    #[test]
    fn rfc9421_b_2_6_signing_a_request_using_ed25519() {
        // The test-request of Appendix B.2.
        let target = Target {
            method: "POST",
            authority: &normalize_authority("example.com"),
            path: "/foo",
            query: Some("param=Value&Pet=dog"),
        };
        let field = |name: &str| -> Option<String> {
            match name {
                "date" => Some("Tue, 20 Apr 2021 02:07:55 GMT".into()),
                "content-type" => Some("application/json".into()),
                "content-length" => Some("18".into()),
                _ => None,
            }
        };

        let signature_input = concat!(
            r#"sig-b26=("date" "@method" "@path" "@authority" "#,
            r#""content-type" "content-length");created=1618884473"#,
            r#";keyid="test-key-ed25519""#,
        );
        let input = SignatureInput::parse(signature_input, "sig-b26").unwrap();
        // Parsing and serializing again is the identity: the
        // @signature-params line is rebuilt from what was parsed.
        assert_eq!(format!("sig-b26={}", input.serialize()), signature_input);

        let base = input
            .signature_base(|name| component_value(&target, &field, name))
            .unwrap();
        let want_base = concat!(
            "\"date\": Tue, 20 Apr 2021 02:07:55 GMT\n",
            "\"@method\": POST\n",
            "\"@path\": /foo\n",
            "\"@authority\": example.com\n",
            "\"content-type\": application/json\n",
            "\"content-length\": 18\n",
            r#""@signature-params": ("date" "@method" "@path" "@authority" "#,
            r#""content-type" "content-length");created=1618884473"#,
            r#";keyid="test-key-ed25519""#,
        );
        assert_eq!(base, want_base);

        let signature_field = concat!(
            "sig-b26=:wqcAqbmYJ2ji2glfAMaRy4gruYYnx2nEFN2HN6jrnDnQCK1",
            "u02Gb04v9EDgwUPiu4A0w6vuQv5lIp5WPpBKRCw==:",
        );
        let want = parse_signature(signature_field, "sig-b26").unwrap();
        assert_eq!(sign(&test_key(), &base), want);
        verify(&test_key().verifying_key(), &base, &want).unwrap();

        let mut tampered = want.clone();
        tampered[0] ^= 1;
        assert_eq!(
            verify(&test_key().verifying_key(), &base, &tampered),
            Err(SignatureError::BadSignature)
        );
    }

    /// RFC 9530 Appendix B.1 and B.2: `{"hello": "world"}` and a line
    /// feed, and empty content.
    #[test]
    fn rfc9530_content_digest_examples() {
        assert_eq!(
            content_digest(b"{\"hello\": \"world\"}\n"),
            "sha-256=:RK/0qy18MlBSVnWgjwz6lZEWjP/lF5HF9bvEF8FabDg=:"
        );
        assert_eq!(
            content_digest(b""),
            "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:"
        );
    }

    /// RFC 9421 Appendix B.2's test-request carries a sha-512 digest only.
    /// It parses, and is refused for having no sha-256 rather than
    /// mistaken for a mismatch.
    #[test]
    fn a_digest_without_sha_256_is_named_as_such() {
        let field = concat!(
            "sha-512=:WZDPaVn/7XgHaAy8pmojAkGWoRx2UFChF41A2svX+T",
            "aPm+AbwAgBWnrIiYllu7BNNyealdVLvRwEmTHWXvJwew==:",
        );
        assert_eq!(
            check_content_digest(field, b"{\"hello\": \"world\"}"),
            Err(SignatureError::NoDigest)
        );
    }

    // -----------------------------------------------------------------------
    // Recall's profile
    // -----------------------------------------------------------------------

    const NOW: i64 = 1_790_000_000;

    fn target(query: Option<&'static str>) -> Target<'static> {
        Target {
            method: "GET",
            authority: "recall.example.com",
            path: "/sync",
            query,
        }
    }

    /// Headers as a server would read them from the signed request.
    fn headers(signed: &SignedHeaders) -> impl Fn(&str) -> Option<String> + '_ {
        move |name: &str| match name {
            CONTENT_DIGEST_HEADER => Some(signed.content_digest.clone()),
            SIGNATURE_INPUT_HEADER => Some(signed.signature_input.clone()),
            SIGNATURE_HEADER => Some(signed.signature.clone()),
            crate::PROTOCOL_HEADER => Some("1".to_string()),
            _ => None,
        }
    }

    fn check(
        signed: &SignedHeaders,
        target: &Target<'_>,
        body: &[u8],
        now: i64,
    ) -> Result<(), SignatureError> {
        let field = headers(signed);
        let input = SignatureInput::parse(&field(SIGNATURE_INPUT_HEADER).unwrap(), LABEL)?;
        let signature = parse_signature(&field(SIGNATURE_HEADER).unwrap(), LABEL)?;
        let received = Received {
            input: &input,
            signature: &signature,
            target: *target,
            field: &field,
        };
        verify_request(
            &received,
            body,
            &test_key().verifying_key(),
            now,
            WINDOW_SECONDS,
        )
    }

    #[test]
    fn a_signed_request_has_the_documented_shape_and_verifies() {
        let t = target(Some("project_key=acme%2Fapp"));
        let signed = sign_request(&test_key(), "dev_abc", &t, "1", b"", NOW, "n0nce").unwrap();
        assert_eq!(
            signed.signature_input,
            concat!(
                r#"sig1=("@method" "@authority" "@path" "@query" "content-digest" "recall-protocol")"#,
                r#";created=1790000000;keyid="dev_abc";nonce="n0nce";alg="ed25519""#,
            )
        );
        assert_eq!(
            signed.content_digest,
            "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:"
        );
        assert!(signed.signature.starts_with("sig1=:") && signed.signature.ends_with(':'));
        check(&signed, &t, b"", NOW).unwrap();
    }

    #[test]
    fn every_covered_part_of_the_request_is_bound() {
        let body = br#"{"project_key":"acme/app"}"#;
        let t = Target {
            method: "POST",
            authority: "recall.example.com",
            path: "/sync",
            query: None,
        };
        let signed = sign_request(&test_key(), "dev_abc", &t, "1", body, NOW, "n").unwrap();
        check(&signed, &t, body, NOW).unwrap();

        let other = [
            Target { method: "PUT", ..t },
            Target {
                authority: "evil.example.com",
                ..t
            },
            Target {
                path: "/admin/stats",
                ..t
            },
            Target {
                query: Some("x=1"),
                ..t
            },
        ];
        for changed in other {
            assert_eq!(
                check(&signed, &changed, body, NOW),
                Err(SignatureError::BadSignature),
                "{changed:?}"
            );
        }
        assert_eq!(
            check(&signed, &t, b"{\"project_key\":\"evil\"}", NOW),
            Err(SignatureError::DigestMismatch),
            "a different body"
        );

        // The protocol header is covered too.
        let field = |name: &str| match name {
            crate::PROTOCOL_HEADER => Some("2".to_string()),
            other => headers(&signed)(other),
        };
        let input = SignatureInput::parse(&signed.signature_input, LABEL).unwrap();
        let sig = parse_signature(&signed.signature, LABEL).unwrap();
        let received = Received {
            input: &input,
            signature: &sig,
            target: t,
            field: &field,
        };
        assert_eq!(
            verify_request(
                &received,
                body,
                &test_key().verifying_key(),
                NOW,
                WINDOW_SECONDS
            ),
            Err(SignatureError::BadSignature)
        );
    }

    #[test]
    fn created_must_be_inside_the_window_either_way() {
        let t = target(None);
        let signed = sign_request(&test_key(), "dev_abc", &t, "1", b"", NOW, "n").unwrap();
        check(&signed, &t, b"", NOW + 60).unwrap();
        check(&signed, &t, b"", NOW - 60).unwrap();
        assert_eq!(
            check(&signed, &t, b"", NOW + 61),
            Err(SignatureError::Clock {
                skew: 61,
                window: 60
            })
        );
        assert!(matches!(
            check(&signed, &t, b"", NOW - 3600),
            Err(SignatureError::Clock { .. })
        ));
    }

    /// Strict verification, not plain RFC 8032 (the review's mutation M5,
    /// `verify_strict` to `verify`). A signature whose R is the identity, a
    /// point of small order, satisfies the plain equation when S = k·a, so
    /// anyone who can make one verifies without it being what a signer
    /// produces. Plain verification accepts it; this must not.
    #[test]
    fn a_signature_with_a_small_order_r_is_refused() {
        use curve25519_dalek::Scalar;
        use ed25519_dalek::Verifier;
        use sha2::Sha512;

        let key = test_key();
        let public = key.verifying_key();
        let base = "\"@method\": GET";
        let mut r = [0u8; 32];
        r[0] = 1; // the identity point, compressed
        let k = Scalar::from_bytes_mod_order_wide(
            &Sha512::new()
                .chain_update(r)
                .chain_update(public.as_bytes())
                .chain_update(base.as_bytes())
                .finalize()
                .into(),
        );
        let s = k * key.to_scalar();
        let mut forged = [0u8; 64];
        forged[..32].copy_from_slice(&r);
        forged[32..].copy_from_slice(s.as_bytes());

        assert!(
            public
                .verify(base.as_bytes(), &Signature::from_bytes(&forged))
                .is_ok(),
            "plain verification accepts it, which is the point of the test"
        );
        assert_eq!(
            verify(&public, base, &forged),
            Err(SignatureError::BadSignature)
        );
    }

    /// A signature whose S is not reduced below the group order L is the
    /// same equation in a second encoding, so one request would have two
    /// valid signatures. ed25519-dalek refuses it in both its plain and
    /// strict checks; this pins that it stays refused.
    #[test]
    fn a_signature_whose_s_is_not_reduced_is_refused() {
        use curve25519_dalek::Scalar;

        // L = 2^252 + 27742317777372353535851937790883648493, little-endian.
        let l: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
        ];
        assert_eq!(Scalar::from_bytes_mod_order(l), Scalar::ZERO, "that is L");

        let key = test_key();
        let base = "\"@method\": GET";
        let good = sign(&key, base);
        let mut bad = good.clone();
        let mut carry = 0u16;
        for i in 0..32 {
            let sum = u16::from(good[32 + i]) + u16::from(l[i]) + carry;
            bad[32 + i] = sum as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0, "S + L fits in 32 bytes");
        verify(&key.verifying_key(), base, &good).unwrap();
        assert_eq!(
            verify(&key.verifying_key(), base, &bad),
            Err(SignatureError::BadSignature)
        );
    }

    #[test]
    fn another_key_does_not_verify() {
        let t = target(None);
        let other = SigningKey::from_bytes(&[7; 32]);
        let signed = sign_request(&other, "dev_abc", &t, "1", b"", NOW, "n").unwrap();
        assert_eq!(
            check(&signed, &t, b"", NOW),
            Err(SignatureError::BadSignature)
        );
    }

    #[test]
    fn the_profile_requires_every_component_and_parameter() {
        let full = SignatureInput::recall(NOW, "dev_abc", "n");
        full.check_profile(NOW, 60).unwrap();

        let mut missing = full.clone();
        missing.components.retain(|c| c != "@query");
        assert_eq!(
            missing.check_profile(NOW, 60),
            Err(SignatureError::NotCovered)
        );

        for (param, want) in [
            ("keyid", SignatureError::MissingParameter("keyid")),
            ("nonce", SignatureError::MissingParameter("nonce")),
            ("created", SignatureError::MissingParameter("created")),
        ] {
            let mut without = full.clone();
            without.params.retain(|(k, _)| k != param);
            assert_eq!(without.check_profile(NOW, 60), Err(want), "{param}");
        }

        // alg may be left out, the key decides; but it may not disagree.
        let mut no_alg = full.clone();
        no_alg.params.retain(|(k, _)| k != "alg");
        no_alg.check_profile(NOW, 60).unwrap();
        let mut rsa = full.clone();
        rsa.params[3].1 = Param::String("rsa-pss-sha512".into());
        assert_eq!(
            rsa.check_profile(NOW, 60),
            Err(SignatureError::Algorithm("rsa-pss-sha512".into()))
        );

        let long = SignatureInput::recall(NOW, "dev_abc", &"n".repeat(MAX_NONCE_LEN + 1));
        assert_eq!(long.check_profile(NOW, 60), Err(SignatureError::Nonce));
        let empty = SignatureInput::recall(NOW, "dev_abc", "");
        assert_eq!(empty.check_profile(NOW, 60), Err(SignatureError::Nonce));

        let mut expired = full.clone();
        expired
            .params
            .push(("expires".into(), Param::Integer(NOW - 1)));
        assert_eq!(expired.check_profile(NOW, 60), Err(SignatureError::Expired));
    }

    #[test]
    fn a_component_listed_twice_is_refused() {
        let mut twice = SignatureInput::recall(NOW, "dev_abc", "n");
        twice.components.push("@method".into());
        let t = target(None);
        assert_eq!(
            twice.signature_base(|n| component_value(&t, &|_| Some("1".into()), n)),
            Err(SignatureError::Duplicate("@method".into()))
        );
    }

    #[test]
    fn an_unknown_derived_component_or_absent_header_is_refused() {
        let t = target(None);
        let input = SignatureInput {
            components: vec!["@target-uri".into()],
            params: vec![],
        };
        assert_eq!(
            input.signature_base(|n| component_value(&t, &|_| None, n)),
            Err(SignatureError::MissingComponent("@target-uri".into()))
        );
        let input = SignatureInput {
            components: vec!["recall-protocol".into()],
            params: vec![],
        };
        assert_eq!(
            input.signature_base(|n| component_value(&t, &|_| None, n)),
            Err(SignatureError::MissingComponent("recall-protocol".into()))
        );
    }

    #[test]
    fn query_and_path_follow_rfc9421() {
        let t = Target {
            method: "GET",
            authority: "h",
            path: "",
            query: None,
        };
        assert_eq!(t.derived("@path").as_deref(), Some("/"));
        assert_eq!(t.derived("@query").as_deref(), Some("?"));
        let t = Target {
            query: Some("param=value&foo=bar&baz=bat%2Dman"),
            ..t
        };
        // §2.2.7's own example: percent-encoding is left alone.
        assert_eq!(
            t.derived("@query").as_deref(),
            Some("?param=value&foo=bar&baz=bat%2Dman")
        );
    }

    #[test]
    fn authority_is_lowercased_without_a_default_port() {
        assert_eq!(
            normalize_authority("Recall.Example.COM"),
            "recall.example.com"
        );
        assert_eq!(
            normalize_authority("recall.example.com:443"),
            "recall.example.com"
        );
        assert_eq!(normalize_authority("127.0.0.1:80"), "127.0.0.1");
        assert_eq!(normalize_authority("127.0.0.1:8787"), "127.0.0.1:8787");
        assert_eq!(normalize_authority("[::1]:443"), "[::1]");
    }

    #[test]
    fn public_keys_are_32_bytes_of_base64url_and_never_weak() {
        assert!(parse_public_key(TEST_KEY_ED25519_X).is_ok());
        for bad in [
            "",
            "not base64!",
            // Padded, which the wire format does not use.
            "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0bs=",
            // 31 bytes.
            "JrQLj5P_89iXES9-vFgrIy29clF9CC_oPPsw3c5D0",
        ] {
            assert_eq!(
                parse_public_key(bad),
                Err(SignatureError::PublicKey),
                "{bad:?}"
            );
        }
        // The identity point has small order: every signature would
        // verify under it.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert_eq!(
            parse_public_key(&URL_SAFE_NO_PAD.encode(identity)),
            Err(SignatureError::PublicKey)
        );
    }

    #[test]
    fn a_fingerprint_is_the_sha256_of_the_raw_key() {
        let key = test_key().verifying_key();
        let want = format!(
            "SHA256:{}",
            STANDARD_NO_PAD.encode(Sha256::digest(key.as_bytes()))
        );
        assert_eq!(fingerprint(&key), want);
        assert!(!fingerprint(&key).contains('='));
    }

    // -----------------------------------------------------------------------
    // structured-field parsing
    // -----------------------------------------------------------------------

    #[test]
    fn dictionaries_parse_per_rfc8941() {
        let d = sf::dictionary(r#"a=1, b="x\"y", c=:AQI=:;p, d=(1 "two");q=?0, e"#).unwrap();
        assert_eq!(d.len(), 5);
        assert_eq!(
            d[0],
            ("a".into(), sf::Member::Item(sf::Item::Integer(1), vec![]))
        );
        assert_eq!(
            d[1],
            (
                "b".into(),
                sf::Member::Item(sf::Item::String("x\"y".into()), vec![])
            )
        );
        assert_eq!(
            d[2],
            (
                "c".into(),
                sf::Member::Item(
                    sf::Item::Bytes(vec![1, 2]),
                    vec![("p".into(), sf::Item::Boolean(true))]
                )
            )
        );
        assert_eq!(
            d[4],
            (
                "e".into(),
                sf::Member::Item(sf::Item::Boolean(true), vec![])
            )
        );
        // A later duplicate replaces the earlier one.
        let d = sf::dictionary("a=1, a=2").unwrap();
        assert_eq!(
            d,
            vec![("a".into(), sf::Member::Item(sf::Item::Integer(2), vec![]))]
        );
        // Padding is optional on a byte sequence.
        assert_eq!(
            sf::dictionary("a=:AQI:").unwrap()[0].1,
            sf::Member::Item(sf::Item::Bytes(vec![1, 2]), vec![])
        );

        for bad in [
            "a=1,",
            "A=1",
            "a=1.5",
            "a=\"unterminated",
            "a=\"bad \\x escape\"",
            "a=(1 2",
            "a=(1\"x\")",
            "a=:not base64!:",
            "a=?2",
            "a=1 b=2",
            "a=1234567890123456",
            "a=\"é\"",
        ] {
            assert_eq!(sf::dictionary(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn strings_serialize_with_escapes_and_round_trip() {
        let input = SignatureInput {
            components: vec!["a\"b".into(), "c\\d".into()],
            params: vec![
                ("keyid".into(), Param::String("k\"1".into())),
                ("flag".into(), Param::Boolean(true)),
                ("off".into(), Param::Boolean(false)),
                ("t".into(), Param::Token("tok/en".into())),
                ("b".into(), Param::Bytes(vec![0xff])),
                ("created".into(), Param::Integer(-5)),
            ],
        };
        let text = input.serialize();
        assert_eq!(
            text,
            r#"("a\"b" "c\\d");keyid="k\"1";flag;off=?0;t=tok/en;b=:/w==:;created=-5"#
        );
        assert_eq!(
            SignatureInput::parse(&format!("sig1={text}"), "sig1").unwrap(),
            input
        );
    }

    #[test]
    fn the_label_must_be_sig1_and_components_may_not_carry_parameters() {
        let t = format!(
            "other={}",
            SignatureInput::recall(NOW, "d", "n").serialize()
        );
        assert_eq!(
            SignatureInput::parse(&t, LABEL),
            Err(SignatureError::NoLabel)
        );
        assert_eq!(
            parse_signature("other=:AA==:", LABEL),
            Err(SignatureError::NoLabel)
        );
        assert_eq!(
            SignatureInput::parse(r#"sig1=("@query-param";name="Pet")"#, LABEL),
            Err(SignatureError::Malformed("signature-input"))
        );
        assert_eq!(
            SignatureInput::parse("sig1=:AA==:", LABEL),
            Err(SignatureError::Malformed("signature-input"))
        );
        assert_eq!(
            parse_signature("sig1=(\"x\")", LABEL),
            Err(SignatureError::Malformed("signature"))
        );
    }
}
