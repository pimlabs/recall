//! Every reply this server sends is JSON, including its errors. Keeping
//! that in one place is what makes it true — an error returned as bare text
//! would be a break in the API surface, not a cosmetic difference.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use recall_wire::ErrorResponse;
use serde::Serialize;

pub(super) fn json<T: Serialize>(status: StatusCode, body: &T) -> Response {
    (status, Json(body)).into_response()
}

pub(super) fn error(status: StatusCode, message: &str) -> Response {
    json(
        status,
        &ErrorResponse {
            error: message.to_string(),
        },
    )
}

pub(super) fn internal(e: anyhow::Error) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

/// An error reply before it becomes one: its status and its message.
///
/// The helpers that decide a request must be refused pass this back in a
/// `Result`, rather than the finished `Response`, which is several times
/// larger and would be copied through every `?` on the way out. It turns
/// into exactly what [`error`] sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Refusal {
    status: StatusCode,
    message: String,
}

impl Refusal {
    pub(super) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    /// What [`internal`] sends.
    pub(super) fn internal(e: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        error(self.status, &self.message)
    }
}
