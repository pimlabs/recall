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
