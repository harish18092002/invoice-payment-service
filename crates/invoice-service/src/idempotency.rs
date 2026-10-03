// Idempotency keys (DESIGN.md sections 1 and 3).
//
// A client may send the same request more than once (a timeout, a retry button). The key in the
// Idempotency-Key header lets us recognise "the same request" and answer it the same way
// without paying twice. One row per (business, key) in idempotency_keys holds:
//   request_hash   what the first request looked like, to catch a key reused for something else
//   attempt_id     the payment attempt that request created
//   response_*     the final answer, written when the attempt is finalised
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

pub const MAX_KEY_LEN: usize = 255;

/// A response saved so it can be replayed.
pub struct StoredResponse {
    pub status: u16,
    pub body: Value,
}

pub struct ExistingKey {
    pub request_hash: Vec<u8>,
    pub attempt_id: Option<Uuid>,
    pub response: Option<StoredResponse>,
}

pub enum Claim {
    /// This request is the first to use the key; we now own it.
    Fresh,
    /// The key was used before.
    Existing(ExistingKey),
}

/// sha256 over "METHOD|path|canonical body". Parsing the body into a `Value` first makes the
/// text canonical: object keys come out sorted and whitespace is gone, so two requests that
/// differ only in key order or spacing hash the same.
pub fn request_hash(method: &str, path: &str, body: &Value) -> Vec<u8> {
    Sha256::digest(format!("{method}|{path}|{body}").as_bytes()).to_vec()
}

/// Tries to insert the key row. ON CONFLICT DO NOTHING means a second request with the same key
/// does not fail: it inserts nothing, and we read the existing row instead. If another request
/// is inserting the same key right now, Postgres makes us wait until that one commits or rolls
/// back, so we never see a half-finished claim.
pub async fn claim(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &str,
    hash: &[u8],
) -> Result<Claim, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_hash) VALUES ($1, $2, $3) \
         ON CONFLICT (business_id, key) DO NOTHING",
    )
    .bind(business_id)
    .bind(key)
    .bind(hash)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(Claim::Fresh);
    }
    let (request_hash, attempt_id, status, body): (
        Vec<u8>,
        Option<Uuid>,
        Option<i32>,
        Option<Value>,
    ) = sqlx::query_as(
        "SELECT request_hash, attempt_id, response_status, response_body FROM idempotency_keys \
         WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key)
    .fetch_one(&mut *conn)
    .await?;
    let response = match (status, body) {
        (Some(status), Some(body)) => Some(StoredResponse {
            status: status as u16,
            body,
        }),
        _ => None,
    };
    Ok(Claim::Existing(ExistingKey {
        request_hash,
        attempt_id,
        response,
    }))
}

/// Links the claimed key to the attempt it created.
pub async fn attach_attempt(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &str,
    attempt_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE idempotency_keys SET attempt_id = $3 WHERE business_id = $1 AND key = $2")
        .bind(business_id)
        .bind(key)
        .bind(attempt_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Saves the final response for the key that created this attempt.
pub async fn store_response(
    conn: &mut PgConnection,
    attempt_id: Uuid,
    response: &StoredResponse,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE idempotency_keys SET response_status = $2, response_body = $3 WHERE attempt_id = $1")
        .bind(attempt_id)
        .bind(response.status as i32)
        .bind(&response.body)
        .execute(conn)
        .await?;
    Ok(())
}

/// The stored final response for an attempt, if it has been finalised.
pub async fn load_response(
    conn: &mut PgConnection,
    attempt_id: Uuid,
) -> Result<Option<StoredResponse>, sqlx::Error> {
    let row: Option<(Option<i32>, Option<Value>)> = sqlx::query_as(
        "SELECT response_status, response_body FROM idempotency_keys WHERE attempt_id = $1",
    )
    .bind(attempt_id)
    .fetch_optional(conn)
    .await?;
    Ok(match row {
        Some((Some(status), Some(body))) => Some(StoredResponse {
            status: status as u16,
            body,
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_ignores_key_order_and_spacing() {
        let a: Value = serde_json::from_str(r#"{"card_token":"tok_success","x":1}"#).unwrap();
        let b: Value =
            serde_json::from_str("{ \"x\": 1,\n \"card_token\": \"tok_success\" }").unwrap();
        assert_eq!(
            request_hash("POST", "/p", &a),
            request_hash("POST", "/p", &b)
        );
    }

    #[test]
    fn hash_changes_with_method_path_or_body() {
        let body = json!({"card_token":"tok_success"});
        let base = request_hash("POST", "/v1/invoices/1/pay", &body);
        assert_ne!(base, request_hash("PUT", "/v1/invoices/1/pay", &body));
        assert_ne!(base, request_hash("POST", "/v1/invoices/2/pay", &body));
        assert_ne!(
            base,
            request_hash(
                "POST",
                "/v1/invoices/1/pay",
                &json!({"card_token":"tok_card_declined"})
            )
        );
    }
}
