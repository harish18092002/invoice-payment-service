// The single error type for the API. Every failure becomes the same JSON envelope:
// {"error":{"type","code","message","request_id"}}.
#![allow(dead_code)] // some variants are first used in later steps

use axum::{
    Json,
    extract::{FromRequest, FromRequestParts, Query, Request, rejection::JsonRejection},
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::{Serialize, de::DeserializeOwned};

use crate::request_id;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid request: {0}")]
    InvalidRequest(String), // 400
    #[error("unauthorized")]
    Unauthorized, // 401
    #[error("payment failed: {0}")]
    PaymentFailed(String), // 402, carries the PSP failure code
    #[error("not found: {0}")]
    NotFound(String), // 404
    #[error("conflict: {code}")]
    Conflict { code: String, message: String }, // 409
    #[error("unprocessable: {code}")]
    Unprocessable { code: String, message: String }, // 422
    #[error("payment provider unavailable")]
    PspUnavailable, // 502
    #[error("internal error: {0}")]
    Internal(String), // 500, detail is logged but never returned
}

#[derive(Serialize)]
struct Envelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    #[serde(rename = "type")]
    kind: &'static str,
    code: String,
    message: String,
    request_id: String,
}

impl AppError {
    /// The HTTP status and the JSON envelope for this error. Used when the response is built
    /// outside a request (for example, a stored idempotent response written by a background task).
    pub fn envelope(self, request_id: &str) -> (StatusCode, serde_json::Value) {
        let (status, kind, code, message) = match self {
            AppError::InvalidRequest(msg) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request".to_string(),
                msg,
            ),
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "unauthorized".to_string(),
                "missing or invalid API key".to_string(),
            ),
            AppError::PaymentFailed(psp_code) => (
                StatusCode::PAYMENT_REQUIRED,
                "payment_error",
                psp_code,
                "the payment was declined".to_string(),
            ),
            AppError::NotFound(msg) => (
                StatusCode::NOT_FOUND,
                "not_found_error",
                "not_found".to_string(),
                msg,
            ),
            AppError::Conflict { code, message } => {
                (StatusCode::CONFLICT, "conflict_error", code, message)
            }
            AppError::Unprocessable { code, message } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unprocessable_error",
                code,
                message,
            ),
            AppError::PspUnavailable => (
                StatusCode::BAD_GATEWAY,
                "psp_error",
                "psp_unavailable".to_string(),
                "the payment provider is unavailable, please retry".to_string(),
            ),
            AppError::Internal(detail) => {
                // Log the real cause with the request id, but never send it to the client.
                tracing::error!(request_id = %request_id, detail = %detail, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "internal".to_string(),
                    "something went wrong".to_string(),
                )
            }
        };
        let body = Envelope {
            error: ErrorBody {
                kind,
                code,
                message,
                request_id: request_id.to_string(),
            },
        };
        // A struct of plain strings always serializes.
        (
            status,
            serde_json::to_value(body).expect("envelope serializes"),
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, body) = self.envelope(&request_id::current());
        (status, Json(body)).into_response()
    }
}

// Our own JSON extractor. Axum's built-in `Json` answers bad input with a plain-text body;
// this wrapper runs it and turns every rejection into our envelope with status 400.
// Handlers take `ApiJson<MyRequest>` instead of `Json<MyRequest>`.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(json_rejection_to_error(rejection)),
        }
    }
}

// Same idea as ApiJson, for query strings: a bad `?limit=abc` becomes our 400 envelope.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(ApiQuery(value)),
            Err(rejection) => Err(AppError::InvalidRequest(rejection.body_text())),
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(format!("database error: {e}"))
    }
}

fn json_rejection_to_error(rejection: JsonRejection) -> AppError {
    // Covers malformed JSON, unknown fields, wrong types and a missing content type.
    AppError::InvalidRequest(rejection.body_text())
}
