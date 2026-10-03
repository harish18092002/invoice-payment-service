// Makes the current request's id available to code that has no access to the request,
// such as `AppError::into_response`.
use axum::{extract::Request, middleware::Next, response::Response};
use tower_http::request_id::{MakeRequestId, RequestId};
use uuid::Uuid;

// A task-local is a value that lives for the duration of one async task. Each request is
// handled in its own task scope, so concurrent requests never see each other's id.
tokio::task_local! {
    static CURRENT_REQUEST_ID: String;
}

/// The id of the request being handled, or an empty string outside a request.
pub fn current() -> String {
    CURRENT_REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_default()
}

/// Generates a fresh UUIDv7 for requests that arrive without an x-request-id header.
#[derive(Clone)]
pub struct MakeUuidRequestId;

impl MakeRequestId for MakeUuidRequestId {
    fn make_request_id<B>(&mut self, _request: &axum::http::Request<B>) -> Option<RequestId> {
        let id = Uuid::now_v7().to_string();
        let value = axum::http::HeaderValue::from_str(&id).ok()?;
        Some(RequestId::new(value))
    }
}

/// Middleware: copies the x-request-id header into the task-local for the rest of the request.
pub async fn scope_request_id(req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    CURRENT_REQUEST_ID.scope(id, next.run(req)).await
}
