// Transactional outbox. `emit` is called inside the same database transaction as the state
// change it describes, so either both are saved or neither is: no event is lost on a crash
// and no event describes a change that was rolled back. A background dispatcher (a later
// step) delivers the webhook_deliveries rows.
use std::sync::Arc;

use axum::{Json, extract::State};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthedBusiness,
    error::{ApiQuery, AppError},
};

/// Records an event and one pending delivery per enabled webhook endpoint of the business.
/// `conn` is the caller's open transaction (pass `&mut *tx`); this function never commits.
/// `object` is the resource the event is about, for example the invoice JSON.
pub async fn emit(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: &str,
    object: serde_json::Value,
) -> Result<(), AppError> {
    let event_id = Uuid::now_v7();
    let created_at = Utc::now();
    let payload = json!({
        "id": event_id,
        "type": event_type,
        "created_at": created_at,
        "data": { "object": object },
    });

    sqlx::query("INSERT INTO events (id, business_id, type, payload, created_at) VALUES ($1, $2, $3, $4, $5)")
        .bind(event_id)
        .bind(business_id)
        .bind(event_type)
        .bind(&payload)
        .bind(created_at)
        .execute(&mut *conn)
        .await?;

    let endpoint_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM webhook_endpoints WHERE business_id = $1 AND enabled ORDER BY id",
    )
    .bind(business_id)
    .fetch_all(&mut *conn)
    .await?;

    for endpoint_id in endpoint_ids {
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, event_id, endpoint_id) VALUES ($1, $2, $3)",
        )
        .bind(Uuid::now_v7())
        .bind(event_id)
        .bind(endpoint_id)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct EventsQuery {
    after: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct EventsPage {
    data: Vec<serde_json::Value>,
    has_more: bool,
}

/// GET /v1/events?after=<event id>&limit=: the business's events in id order (oldest first),
/// so a receiver that missed webhooks can catch up from the last event id it saw.
pub async fn list(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiQuery(query): ApiQuery<EventsQuery>,
) -> Result<Json<EventsPage>, AppError> {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(AppError::InvalidRequest(
            "limit must be between 1 and 100".into(),
        ));
    }
    let after = match query.after {
        Some(text) => Some(
            Uuid::parse_str(&text)
                .map_err(|_| AppError::InvalidRequest("after must be an event id".into()))?,
        ),
        None => None,
    };
    let mut rows: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE business_id = $1 AND ($2::uuid IS NULL OR id > $2) ORDER BY id LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(after)
    .bind(limit + 1) // one extra row tells us whether more exist
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    Ok(Json(EventsPage {
        data: rows,
        has_more,
    }))
}
