// Self-service API key management for an authenticated business (used for rotation:
// create the new key, switch over, then revoke the old one).
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::{AuthedBusiness, CreatedKey, generate_key},
    error::{ApiJson, AppError},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateKeyRequest {
    label: Option<String>,
}

/// POST /v1/api_keys: adds another key to the caller's business and returns it once.
pub async fn create(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreateKeyRequest>,
) -> Result<(StatusCode, Json<CreatedKey>), AppError> {
    if req.label.as_ref().is_some_and(|l| l.chars().count() > 100) {
        return Err(AppError::InvalidRequest(
            "label must be at most 100 characters".into(),
        ));
    }
    let id = Uuid::now_v7();
    let key = generate_key();
    // business_id comes from the authenticated key, never from the request.
    sqlx::query("INSERT INTO api_keys (id, business_id, prefix, key_hash, label) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(auth.business_id)
        .bind(&key.prefix)
        .bind(&key.hash)
        .bind(&req.label)
        .execute(&state.pool)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedKey {
            id,
            prefix: key.prefix,
            key: key.full,
        }),
    ))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct KeyView {
    id: Uuid,
    prefix: String,
    label: Option<String>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
}

/// GET /v1/api_keys: lists the caller's keys. Never includes secrets or hashes.
pub async fn list(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<KeyView>>, AppError> {
    let keys = sqlx::query_as::<_, KeyView>(
        "SELECT id, prefix, label, created_at, last_used_at, revoked_at \
         FROM api_keys WHERE business_id = $1 ORDER BY created_at, id",
    )
    .bind(auth.business_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(keys))
}

/// DELETE /v1/api_keys/{id}: revokes one of the caller's keys. Revoking twice is harmless.
/// A key that belongs to another business answers 404, the same as one that does not exist.
pub async fn revoke(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let not_found = || AppError::NotFound("api key not found".into());
    let id = Uuid::parse_str(&id).map_err(|_| not_found())?;
    let revoked = sqlx::query_scalar::<_, Uuid>(
        "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now()) \
         WHERE id = $1 AND business_id = $2 RETURNING id",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&state.pool)
    .await?;
    match revoked {
        Some(_) => Ok(StatusCode::NO_CONTENT),
        None => Err(not_found()),
    }
}
