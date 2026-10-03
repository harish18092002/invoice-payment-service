use std::sync::Arc;

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};
use tower_http::{
    request_id::{PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

use crate::{
    auth, config::Config, customers, error::AppError, events, invoices, keys, payments, request_id,
    webhooks,
};

// Shared by all handlers. Wrapped in an Arc so cloning it per request is cheap.
#[allow(dead_code)] // handlers use these from the next step on
pub struct AppState {
    pub config: Config,
    pub pool: sqlx::PgPool,
    pub psp: crate::psp::PspClient,
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let routes = Router::new()
        .route("/health", get(health))
        .route("/admin/businesses", post(auth::create_business))
        .route("/v1/api_keys", get(keys::list).post(keys::create))
        .route("/v1/api_keys/{id}", delete(keys::revoke))
        .route(
            "/v1/customers",
            get(customers::list).post(customers::create),
        )
        .route("/v1/customers/{id}", get(customers::get_one))
        .route(
            "/v1/webhook_endpoints",
            get(webhooks::endpoints::list).post(webhooks::endpoints::create),
        )
        .route("/v1/events", get(events::list))
        .route("/v1/invoices", get(invoices::list).post(invoices::create))
        .route("/v1/invoices/{id}", get(invoices::get_one))
        .route("/v1/invoices/{id}/pay", post(payments::pay))
        .route(
            "/v1/invoices/{id}/payment_attempts",
            get(payments::list_attempts),
        )
        .route("/v1/invoices/{id}/finalize", post(invoices::finalize))
        .route("/v1/invoices/{id}/void", post(invoices::void))
        .route(
            "/v1/invoices/{id}/mark_uncollectible",
            post(invoices::mark_uncollectible),
        )
        .fallback(not_found)
        .with_state(state);
    with_common_layers(routes)
}

// Layers are listed outermost first: the id is set, logged, echoed back in the response
// header, and made available to the error type before any handler runs.
pub fn with_common_layers(router: Router) -> Router {
    router
        .layer(middleware::from_fn(request_id::scope_request_id))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
                let id = req
                    .headers()
                    .get("x-request-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-");
                tracing::info_span!("request", request_id = %id, method = %req.method(), path = %req.uri().path())
            }),
        )
        .layer(SetRequestIdLayer::x_request_id(request_id::MakeUuidRequestId))
}

async fn health() -> &'static str {
    "ok"
}

async fn not_found() -> AppError {
    AppError::NotFound("no such route".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiJson;
    use axum::routing::post;
    use serde::Deserialize;

    // Stand-in request type: unknown fields are rejected, like the real request structs will be.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Probe {
        #[allow(dead_code)]
        name: String,
    }

    async fn probe(ApiJson(_p): ApiJson<Probe>) -> &'static str {
        "ok"
    }

    // Starts the app on a random free port and returns its base URL.
    async fn spawn_app() -> String {
        let state = Arc::new(AppState {
            config: Config {
                database_url: String::new(),
                psp_url: String::new(),
                admin_token: String::new(),
                psp_wait_secs: 5,
                psp_timeout_secs: 35,
                webhook_delay_scale: 1.0,
                webhook_poll_ms: 1000,
                reconcile_interval_secs: 15,
                reconcile_min_age_secs: 45,
                reconcile_not_found_after_secs: 120,
                crash_after_psp_call: false,
            },
            // Lazy: no connection is opened until a query runs, and these tests run none.
            pool: sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap(),
            psp: crate::psp::PspClient::new(
                "http://localhost:1".into(),
                std::time::Duration::from_secs(1),
            ),
        });
        let router = build_router(state);
        let probe_router = with_common_layers(Router::new().route("/probe", post(probe)));
        let app = router.merge(probe_router);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn assert_envelope(resp: reqwest::Response, status: u16, code: &str) {
        assert_eq!(resp.status().as_u16(), status);
        let header_id = resp.headers()["x-request-id"].to_str().unwrap().to_string();
        let body: serde_json::Value = resp.json().await.unwrap();
        let err = &body["error"];
        assert_eq!(err["code"], code);
        assert!(err["type"].is_string() && err["message"].is_string());
        // The id in the body matches the id in the response header.
        assert_eq!(err["request_id"], header_id);
        assert!(!header_id.is_empty());
    }

    #[tokio::test]
    async fn health_is_ok() {
        let base = spawn_app().await;
        let resp = reqwest::get(format!("{base}/health")).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn unknown_route_returns_envelope() {
        let base = spawn_app().await;
        let resp = reqwest::get(format!("{base}/nope")).await.unwrap();
        assert_envelope(resp, 404, "not_found").await;
    }

    #[tokio::test]
    async fn malformed_json_returns_envelope() {
        let base = spawn_app().await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/probe"))
            .header("content-type", "application/json")
            .body("{not json")
            .send()
            .await
            .unwrap();
        assert_envelope(resp, 400, "invalid_request").await;
    }

    #[tokio::test]
    async fn unknown_field_returns_envelope() {
        let base = spawn_app().await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/probe"))
            .header("content-type", "application/json")
            .body(r#"{"name":"x","total_cents":5}"#)
            .send()
            .await
            .unwrap();
        assert_envelope(resp, 400, "invalid_request").await;
    }
}
