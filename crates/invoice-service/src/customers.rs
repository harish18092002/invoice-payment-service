// Customers. Every query filters by the business_id of the authenticated API key.
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
    auth::AuthedBusiness,
    error::{ApiJson, ApiQuery, AppError},
    pagination::{PageParams, Paginated, finish},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCustomerRequest {
    name: String,
    email: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Customer {
    id: Uuid,
    name: String,
    email: String,
    created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, name, email, created_at";

/// Basic shape check only: exactly one @, text on both sides, no whitespace.
fn valid_email(email: &str) -> bool {
    if email.is_empty() || email.chars().count() > 254 || email.chars().any(char::is_whitespace) {
        return false;
    }
    match email.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !domain.contains('@'),
        None => false,
    }
}

pub async fn create(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<Customer>), AppError> {
    let name = req.name.trim().to_string();
    let email = req.email.trim().to_string();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(AppError::InvalidRequest(
            "name must be 1 to 200 characters".into(),
        ));
    }
    if !valid_email(&email) {
        return Err(AppError::InvalidRequest("email is not valid".into()));
    }
    let customer = sqlx::query_as::<_, Customer>(&format!(
        "INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4) RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(&name)
    .bind(&email)
    .fetch_one(&state.pool)
    .await?;
    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get_one(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Customer>, AppError> {
    // A malformed id, a missing customer and another business's customer all give the same 404.
    let not_found = || AppError::NotFound("customer not found".into());
    let id = Uuid::parse_str(&id).map_err(|_| not_found())?;
    let customer = sqlx::query_as::<_, Customer>(&format!(
        "SELECT {COLUMNS} FROM customers WHERE id = $1 AND business_id = $2"
    ))
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&state.pool)
    .await?;
    customer.map(Json).ok_or_else(not_found)
}

pub async fn list(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    ApiQuery(params): ApiQuery<PageParams>,
) -> Result<Json<Paginated<Customer>>, AppError> {
    let page = params.resolve()?;
    // Fetch one extra row to learn whether another page exists. The cursor condition is
    // skipped on the first page, when the cursor binds are NULL.
    let rows = sqlx::query_as::<_, Customer>(&format!(
        "SELECT {COLUMNS} FROM customers \
         WHERE business_id = $1 AND ($2::timestamptz IS NULL OR (created_at, id) < ($2, $3)) \
         ORDER BY created_at DESC, id DESC LIMIT $4"
    ))
    .bind(auth.business_id)
    .bind(page.after_created_at)
    .bind(page.after_id)
    .bind(page.limit + 1)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(finish(rows, page.limit, |c| (c.created_at, c.id))))
}

#[cfg(test)]
mod tests {
    use super::valid_email;

    #[test]
    fn email_shape() {
        assert!(valid_email("a@b.co"));
        assert!(valid_email("a@localhost"));
        for bad in [
            "",
            "ab",
            "@b.co",
            "a@",
            "a@@b.co",
            "a@b@c",
            "a b@c.co",
            " a@b.co",
            &format!("{}@b.co", "x".repeat(260)),
        ] {
            assert!(!valid_email(bad), "accepted {bad:?}");
        }
    }
}
