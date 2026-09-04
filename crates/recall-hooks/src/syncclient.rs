//! Talks to the Recall server.

use std::time::Duration;

use recall_wire::{Health, PushRequest, PushResponse, SyncResponse, ValidationError};
use serde::de::DeserializeOwned;

/// A generous but bounded timeout. The server may be running a semantic
/// merge through the `claude` CLI, which takes several seconds — but these
/// calls happen inside a user's session, so they cannot hang forever.
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The server answered, and said no. Carries enough to tell the user
    /// *why* — a 401 from a stale token reads very differently from a 500.
    #[error("server returned {code}: {body}")]
    Status { code: u16, body: String },
    #[error("refusing to send an invalid request: {0}")]
    Invalid(#[from] ValidationError),
    #[error("could not reach the server: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("server sent a response this client can't parse: {0}")]
    Decode(#[from] serde_json::Error),
}

/// A Recall API client.
#[derive(Debug, Clone)]
pub struct Client {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base_url: &str, token: &str) -> Result<Self, Error> {
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Sends one memory file, or one delete.
    ///
    /// Validated client-side first, so a request the server would reject
    /// never leaves this machine and the user sees the real reason rather
    /// than a 400.
    pub async fn push(&self, req: &PushRequest) -> Result<PushResponse, Error> {
        req.validate()?;
        let body = serde_json::to_vec(req)?;
        let request = self
            .http
            .post(format!("{}/sync", self.base_url))
            .header("Content-Type", "application/json")
            .body(body);
        self.send(request).await
    }

    /// Fetches every file the server holds for a project, tombstones
    /// included — the caller needs to see those to remove local copies.
    pub async fn pull(&self, project_key: &str) -> Result<SyncResponse, Error> {
        let request = self
            .http
            .get(format!("{}/sync", self.base_url))
            .query(&[("project_key", project_key)]);
        self.send(request).await
    }

    /// Reads the health endpoint. It is unauthenticated, but the token is
    /// sent anyway — harmless, and it keeps one code path.
    pub async fn health(&self) -> Result<Health, Error> {
        let request = self.http.get(format!("{}/health", self.base_url));
        self.send(request).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, Error> {
        let response = request.bearer_auth(&self.token).send().await?;
        let status = response.status();
        // Read as bytes rather than text: the body is only ever decoded as
        // JSON, and this keeps the client off reqwest's optional charset
        // feature.
        let body = response.bytes().await?;

        if !status.is_success() {
            return Err(Error::Status {
                code: status.as_u16(),
                body: String::from_utf8_lossy(&body).trim().to_string(),
            });
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::FakeServer;

    #[tokio::test]
    async fn pushes_and_pulls_against_a_real_server() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();

        let req = PushRequest {
            project_key: "acme/app".into(),
            file_path: "MEMORY.md".into(),
            content: Some("hello".into()),
            source_env: "test".into(),
            deleted: false,
        };
        let resp = client.push(&req).await.unwrap();
        assert!(resp.ok);
        assert_eq!(server.pushes(), vec![req]);

        let resp = client.pull("acme/app").await.unwrap();
        assert_eq!(
            resp.project_key, "acme/app",
            "query parameter did not arrive"
        );
    }

    /// A project key with a slash has to survive query escaping.
    #[tokio::test]
    async fn escapes_the_project_key() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();
        let resp = client.pull("acme/app with spaces&more").await.unwrap();
        assert_eq!(resp.project_key, "acme/app with spaces&more");
    }

    #[tokio::test]
    async fn sends_the_bearer_token() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "s3cret").unwrap();
        client.health().await.unwrap();
        assert_eq!(
            server.last_authorization().as_deref(),
            Some("Bearer s3cret")
        );
    }

    /// A non-2xx must surface as a typed error carrying both halves — the
    /// hook's exit code comes off the status, the user's explanation off the
    /// body.
    #[tokio::test]
    async fn a_non_2xx_becomes_a_status_error() {
        let server = FakeServer::start().await;
        server.fail_with(403, r#"{"error":"invalid token"}"#);
        let client = Client::new(&server.url, "token").unwrap();

        let err = client.pull("acme/app").await.unwrap_err();
        match err {
            Error::Status { code, ref body } => {
                assert_eq!(code, 403);
                assert!(body.contains("invalid token"), "body lost: {body}");
            }
            other => panic!("expected a status error, got {other:?}"),
        }
        assert!(err.to_string().contains("403"));
    }

    /// Client-side validation stops a bad request before it is sent at all.
    #[tokio::test]
    async fn refuses_to_send_an_invalid_request() {
        let server = FakeServer::start().await;
        let client = Client::new(&server.url, "token").unwrap();

        let err = client
            .push(&PushRequest {
                project_key: String::new(),
                file_path: "MEMORY.md".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
        assert!(
            server.pushes().is_empty(),
            "an invalid request was sent anyway"
        );
    }

    #[tokio::test]
    async fn trailing_slashes_in_the_base_url_do_not_double_up() {
        let server = FakeServer::start().await;
        let client = Client::new(&format!("{}///", server.url), "token").unwrap();
        assert!(client.health().await.is_ok());
    }
}
