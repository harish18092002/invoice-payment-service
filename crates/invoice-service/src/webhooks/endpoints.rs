// Webhook endpoint registration. The signing secret is returned once, at creation.
// (It has to be stored readable, because signing needs the raw secret; listing never shows it.)
use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use rand::{RngCore, TryRngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthedBusiness,
    error::{ApiJson, AppError},
    webhooks::signer::to_hex,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEndpointRequest {
    url: String,
}

#[derive(Serialize)]
pub struct CreatedEndpoint {
    id: Uuid,
    url: String,
    enabled: bool,
    created_at: DateTime<Utc>,
    /// Shown only in this response.
    secret: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct EndpointView {
    id: Uuid,
    url: String,
    enabled: bool,
    created_at: DateTime<Utc>,
}

/// Only http and https URLs with a host are accepted.
fn valid_url(text: &str) -> bool {
    if text.len() > 2048 {
        return false;
    }
    match reqwest::Url::parse(text) {
        Ok(url) => matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        Err(_) => false,
    }
}

/// whsec_ followed by 32 random bytes as 64 hex characters.
fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.unwrap_err().fill_bytes(&mut bytes);
    format!("whsec_{}", to_hex(&bytes))
}

pub async fn create(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreateEndpointRequest>,
) -> Result<(StatusCode, Json<CreatedEndpoint>), AppError> {
    let url = req.url.trim().to_string();
    if !valid_url(&url) {
        return Err(AppError::InvalidRequest(
            "url must be a valid http or https URL".into(),
        ));
    }
    let secret = generate_secret();
    let (id, created_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret) VALUES ($1, $2, $3, $4) RETURNING id, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(&url)
    .bind(&secret)
    .fetch_one(&state.pool)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedEndpoint {
            id,
            url,
            enabled: true,
            created_at,
            secret,
        }),
    ))
}

pub async fn list(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<EndpointView>>, AppError> {
    let endpoints = sqlx::query_as::<_, EndpointView>(
        "SELECT id, url, enabled, created_at FROM webhook_endpoints WHERE business_id = $1 ORDER BY created_at, id",
    )
    .bind(auth.business_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(endpoints))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_rules() {
        assert!(valid_url("http://mock-psp:9000/sink/demo"));
        assert!(valid_url("https://example.com/hook"));
        for bad in [
            "",
            "example.com",
            "ftp://example.com",
            "javascript:alert(1)",
            "http://",
            "file:///etc/passwd",
        ] {
            assert!(!valid_url(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn secret_format() {
        let s = generate_secret();
        assert!(s.starts_with("whsec_"));
        assert_eq!(s.len(), 6 + 64);
        assert!(s[6..].bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(s, generate_secret());
    }
}
