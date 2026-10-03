// Invoices. Every query filters by the business_id of the authenticated API key.
// The total is always computed by the server from the line items (see money.rs).
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthedBusiness,
    domain::invoice_state::{InvoiceState, can_transition},
    error::{ApiJson, ApiQuery, AppError},
    events,
    money::{LineItemInput, compute_total},
    pagination::{PageParams, Paginated, finish},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // a client-sent total_cents is rejected with 400
pub struct CreateInvoiceRequest {
    customer_id: Uuid,
    due_date: NaiveDate,
    line_items: Vec<LineItemInput>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct InvoiceRow {
    pub id: Uuid,
    customer_id: Uuid,
    state: String,
    currency: String,
    total_cents: i64,
    due_date: NaiveDate,
    paid_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct LineItem {
    position: i32,
    description: String,
    quantity: i32,
    unit_amount_cents: i64,
}

// `flatten` puts the invoice fields and `line_items` side by side in one JSON object.
#[derive(Serialize)]
pub struct InvoiceWithItems {
    #[serde(flatten)]
    invoice: InvoiceRow,
    line_items: Vec<LineItem>,
}

pub const COLUMNS: &str = "id, customer_id, state, currency::text AS currency, total_cents, due_date, paid_at, created_at, updated_at";

pub fn not_found() -> AppError {
    AppError::NotFound("invoice not found".into())
}

fn parse_id(text: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(text).map_err(|_| not_found())
}

async fn load_items(conn: &mut PgConnection, invoice_id: Uuid) -> Result<Vec<LineItem>, AppError> {
    let items = sqlx::query_as::<_, LineItem>(
        "SELECT position, description, quantity, unit_amount_cents FROM invoice_line_items \
         WHERE invoice_id = $1 ORDER BY position",
    )
    .bind(invoice_id)
    .fetch_all(conn)
    .await?;
    Ok(items)
}

async fn with_items(
    conn: &mut PgConnection,
    invoice: InvoiceRow,
) -> Result<InvoiceWithItems, AppError> {
    let line_items = load_items(conn, invoice.id).await?;
    Ok(InvoiceWithItems {
        invoice,
        line_items,
    })
}

/// Loads one invoice with its line items, scoped to the business. None if it is not theirs.
pub async fn load_full(
    conn: &mut PgConnection,
    business_id: Uuid,
    id: Uuid,
) -> Result<Option<InvoiceWithItems>, AppError> {
    let invoice = sqlx::query_as::<_, InvoiceRow>(&format!(
        "SELECT {COLUMNS} FROM invoices WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(business_id)
    .fetch_optional(&mut *conn)
    .await?;
    match invoice {
        Some(invoice) => Ok(Some(with_items(conn, invoice).await?)),
        None => Ok(None),
    }
}

pub fn to_json(invoice: &InvoiceWithItems) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(invoice)
        .map_err(|e| AppError::Internal(format!("could not serialize invoice: {e}")))
}

pub async fn create(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreateInvoiceRequest>,
) -> Result<(StatusCode, Json<InvoiceWithItems>), AppError> {
    for item in &req.line_items {
        if item.description.trim().is_empty() || item.description.chars().count() > 500 {
            return Err(AppError::InvalidRequest(
                "description must be 1 to 500 characters".into(),
            ));
        }
    }
    let total_cents = compute_total(&req.line_items)?;

    // One transaction: invoice, line items and the outbox event are saved together.
    let mut tx = state.pool.begin().await?;

    let customer = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(req.customer_id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await?;
    if customer.is_none() {
        return Err(AppError::NotFound("customer not found".into()));
    }

    let invoice = sqlx::query_as::<_, InvoiceRow>(&format!(
        "INSERT INTO invoices (id, business_id, customer_id, total_cents, due_date) \
         VALUES ($1, $2, $3, $4, $5) RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(req.customer_id)
    .bind(total_cents)
    .bind(req.due_date)
    .fetch_one(&mut *tx)
    .await?;

    for (index, item) in req.line_items.iter().enumerate() {
        sqlx::query(
            "INSERT INTO invoice_line_items (id, invoice_id, position, description, quantity, unit_amount_cents) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(invoice.id)
        .bind(index as i32 + 1) // positions start at 1
        .bind(item.description.trim())
        .bind(item.quantity as i32) // safe: compute_total limited it to 1..=1_000_000
        .bind(item.unit_amount_cents)
        .execute(&mut *tx)
        .await?;
    }

    let full = with_items(&mut tx, invoice).await?;
    events::emit(
        &mut tx,
        auth.business_id,
        "invoice.created",
        to_json(&full)?,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(full)))
}

pub async fn get_one(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<InvoiceWithItems>, AppError> {
    let id = parse_id(&id)?;
    let mut conn = state.pool.acquire().await?;
    let invoice = sqlx::query_as::<_, InvoiceRow>(&format!(
        "SELECT {COLUMNS} FROM invoices WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(not_found)?;
    Ok(Json(with_items(&mut conn, invoice).await?))
}

#[derive(Deserialize)]
pub struct StateFilter {
    state: Option<String>,
}

/// GET /v1/invoices?state=&limit=&cursor= . List rows omit line_items; fetch one invoice for those.
pub async fn list(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiQuery(filter): ApiQuery<StateFilter>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<Paginated<InvoiceRow>>, AppError> {
    let page = params.resolve()?;
    let wanted_state = match filter.state {
        Some(text) => Some(
            InvoiceState::parse(&text)
                .ok_or_else(|| {
                    AppError::InvalidRequest(
                        "state must be draft, open, paid, void or uncollectible".into(),
                    )
                })?
                .as_str(),
        ),
        None => None,
    };
    let rows = sqlx::query_as::<_, InvoiceRow>(&format!(
        "SELECT {COLUMNS} FROM invoices \
         WHERE business_id = $1 AND ($2::text IS NULL OR state = $2) \
           AND ($3::timestamptz IS NULL OR (created_at, id) < ($3, $4)) \
         ORDER BY created_at DESC, id DESC LIMIT $5"
    ))
    .bind(auth.business_id)
    .bind(wanted_state)
    .bind(page.after_created_at)
    .bind(page.after_id)
    .bind(page.limit + 1)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(finish(rows, page.limit, |i| (i.created_at, i.id))))
}

pub async fn finalize(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<InvoiceWithItems>, AppError> {
    transition(&auth, &state, &id, InvoiceState::Open, "invoice.finalized").await
}

pub async fn void(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<InvoiceWithItems>, AppError> {
    transition(&auth, &state, &id, InvoiceState::Void, "invoice.voided").await
}

pub async fn mark_uncollectible(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<InvoiceWithItems>, AppError> {
    transition(
        &auth,
        &state,
        &id,
        InvoiceState::Uncollectible,
        "invoice.marked_uncollectible",
    )
    .await
}

fn invalid_transition(from: &str, to: InvoiceState) -> AppError {
    AppError::Conflict {
        code: "invalid_state_transition".into(),
        message: format!("invoice is {from}; cannot move to {}", to.as_str()),
    }
}

// Shared by finalize, void and mark_uncollectible.
// The row lock is held only for these few queries; no network call happens inside.
async fn transition(
    auth: &AuthedBusiness,
    state: &AppState,
    id: &str,
    to: InvoiceState,
    event_type: &str,
) -> Result<Json<InvoiceWithItems>, AppError> {
    let id = parse_id(id)?;
    let mut tx = state.pool.begin().await?;

    // FOR UPDATE: lock the row so a concurrent payment or transition waits for us.
    let current: String = sqlx::query_scalar(
        "SELECT state FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(not_found)?;
    let from = InvoiceState::parse(&current).ok_or_else(|| {
        AppError::Internal(format!("unknown invoice state in database: {current}"))
    })?;

    if !can_transition(from, to) {
        return Err(invalid_transition(from.as_str(), to));
    }

    // A pending payment attempt means a charge may be in flight, so the invoice must stay as is.
    if matches!(to, InvoiceState::Void | InvoiceState::Uncollectible) {
        let pending = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM payment_attempts WHERE invoice_id = $1 AND status = 'pending')",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if pending {
            return Err(AppError::Conflict {
                code: "payment_in_progress".into(),
                message: "a payment attempt is in progress for this invoice".into(),
            });
        }
    }

    // Compare-and-swap: only update if the state is still the one we checked.
    let updated = sqlx::query_as::<_, InvoiceRow>(&format!(
        "UPDATE invoices SET state = $3, updated_at = now() \
         WHERE id = $1 AND state = $2 AND business_id = $4 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(from.as_str())
    .bind(to.as_str())
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await?;

    let invoice = match updated {
        Some(invoice) => invoice,
        None => {
            // Someone changed it between our read and write: report what it is now.
            let now: String =
                sqlx::query_scalar("SELECT state FROM invoices WHERE id = $1 AND business_id = $2")
                    .bind(id)
                    .bind(auth.business_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(not_found)?;
            return Err(invalid_transition(&now, to));
        }
    };

    let full = with_items(&mut tx, invoice).await?;
    events::emit(&mut tx, auth.business_id, event_type, to_json(&full)?).await?;
    tx.commit().await?;
    Ok(Json(full))
}
