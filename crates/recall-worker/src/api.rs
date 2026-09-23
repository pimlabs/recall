//! The worker's side of the HTTP API: JSON requests, signed as a device
//! signs them (`recall_wire::signature`, RFC 9421), or unsigned for the two
//! enrolment routes, which a machine calls before it has an identity.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use recall_wire::signature::{self, normalize_authority, SigningKey, Target};
use recall_wire::ErrorResponse;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Why a request got no usable answer.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// It never arrived, or no answer came back: worth trying again.
    #[error("{0}")]
    Transport(String),
    /// The server answered, and not with success.
    #[error("{status}: {message}")]
    Status {
        /// The status code.
        status: StatusCode,
        /// The body's `error`, or the body itself when it is not JSON.
        message: String,
        /// `Retry-After`, in seconds, when the server sent one.
        retry_after: Option<u64>,
    },
    /// A success whose body was not what the route promises.
    #[error("unexpected answer: {0}")]
    Body(String),
}

impl ApiError {
    /// The status code, for an answer that had one.
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            ApiError::Status { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The server's `error`, for an answer that had one.
    pub fn message(&self) -> &str {
        match self {
            ApiError::Status { message, .. } => message,
            _ => "",
        }
    }
}

/// A client for one server.
#[derive(Debug, Clone)]
pub struct Api {
    http: reqwest::Client,
    /// `scheme://host[:port]` and any path prefix, without a trailing
    /// slash.
    base: String,
    /// What the server will read as `@authority`.
    authority: String,
    /// The path prefix `base` carries, empty for none.
    prefix: String,
}

impl Api {
    /// A client for the server at `url`.
    pub fn new(url: &str) -> Result<Self, ApiError> {
        let parsed = reqwest::Url::parse(url).map_err(|e| ApiError::Transport(e.to_string()))?;
        let host = parsed
            .host_str()
            .ok_or_else(|| ApiError::Transport(format!("{url} has no host")))?;
        // The Host header reqwest sends: the port only when it is not the
        // scheme's default. normalize_authority drops :80 and :443 on the
        // server's side too, so the two agree either way.
        let authority = normalize_authority(&match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        });
        let http = reqwest::Client::builder()
            .user_agent(crate::user_agent())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ApiError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base: url.trim_end_matches('/').to_string(),
            authority,
            prefix: parsed.path().trim_end_matches('/').to_string(),
        })
    }

    /// Posts `body` unsigned, for enrolling.
    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        self.send(path, body, None, timeout).await
    }

    /// Posts `body` signed with `key` as device `keyid`.
    pub async fn post_signed<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        key: &SigningKey,
        keyid: &str,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        self.send(path, body, Some((key, keyid)), timeout).await
    }

    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        signer: Option<(&SigningKey, &str)>,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        let bytes = serde_json::to_vec(body).map_err(|e| ApiError::Body(e.to_string()))?;
        let mut req = self
            .http
            .post(format!("{}{path}", self.base))
            .timeout(timeout)
            .header("content-type", "application/json")
            .header(
                recall_wire::PROTOCOL_HEADER,
                recall_wire::PROTOCOL.to_string(),
            );
        if let Some((key, keyid)) = signer {
            let full_path = format!("{}{path}", self.prefix);
            let signed = signature::sign_request(
                key,
                keyid,
                &Target {
                    method: "POST",
                    authority: &self.authority,
                    path: &full_path,
                    query: None,
                },
                &recall_wire::PROTOCOL.to_string(),
                &bytes,
                unix_now(),
                &nonce()?,
            )
            .map_err(|e| ApiError::Body(e.to_string()))?;
            req = req
                .header(signature::CONTENT_DIGEST_HEADER, signed.content_digest)
                .header(signature::SIGNATURE_INPUT_HEADER, signed.signature_input)
                .header(signature::SIGNATURE_HEADER, signed.signature);
        }
        let resp = req
            .body(bytes)
            .send()
            .await
            .map_err(|e| ApiError::Transport(describe(&e)))?;
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let text = resp
            .bytes()
            .await
            .map_err(|e| ApiError::Transport(describe(&e)))?;
        if !status.is_success() {
            let message = serde_json::from_slice::<ErrorResponse>(&text)
                .map(|e| e.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&text).trim().to_string());
            return Err(ApiError::Status {
                status,
                message,
                retry_after,
            });
        }
        serde_json::from_slice(&text).map_err(|e| ApiError::Body(e.to_string()))
    }
}

/// A reqwest error with its causes, which its own `Display` leaves out.
fn describe(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 128 random bits: never reused, which is all a nonce must be.
fn nonce() -> Result<String, ApiError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| ApiError::Transport(format!("no randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authority_is_what_the_server_will_read() {
        let api = Api::new("http://recall-server:8787").unwrap();
        assert_eq!(
            (api.authority.as_str(), api.prefix.as_str()),
            ("recall-server:8787", "")
        );
        let api = Api::new("https://Recall.Example.com/").unwrap();
        assert_eq!(api.authority, "recall.example.com");
        assert_eq!(api.base, "https://Recall.Example.com");
        let api = Api::new("https://example.com:8443/recall/").unwrap();
        assert_eq!(
            (api.authority.as_str(), api.prefix.as_str()),
            ("example.com:8443", "/recall")
        );
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(nonce().unwrap(), nonce().unwrap());
    }
}
