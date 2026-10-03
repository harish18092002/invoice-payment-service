// Paying an invoice: DESIGN.md section 3, step by step.
//
//   Tx A     claim the idempotency key, lock the invoice, create a `pending` attempt, commit.
//   PSP call in a spawned task, with NO transaction and NO lock held.
//   Tx B     `finalize`: record the outcome, mark the invoice paid, emit the event, store the response.
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthedBusiness,
    error::AppError,
    events,
    idempotency::{self, Claim, ExistingKey, MAX_KEY_LEN, StoredResponse},
    invoices::{self, InvoiceWithItems},
    psp::{ChargeOutcome, LookupOutcome},
};

/// Failure code used when the PSP provably never saw the charge.
pub const PSP_UNAVAILABLE: &str = "psp_unavailable";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PayRequest {
    card_token: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AttemptView {
    id: Uuid,
    invoice_id: Uuid,
    amount_cents: i64,
    status: String,
    psp_ref: Option<String>,
    failure_code: Option<String>,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

// The card token is deliberately not part of this view: tokens are never returned or logged.
const ATTEMPT_COLUMNS: &str =
    "id, invoice_id, amount_cents, status, psp_ref, failure_code, created_at, completed_at";

/// What `finalize` needs to know about an attempt.
#[derive(Clone)]
pub struct AttemptRef {
    pub id: Uuid,
    pub invoice_id: Uuid,
    pub business_id: Uuid,
}

/// The PSP's verdict on an attempt.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Succeeded { psp_ref: String },
    Failed { code: String },
}

fn pending_response(attempt_id: Uuid) -> StoredResponse {
    StoredResponse {
        status: 202,
        body: json!({ "attempt_id": attempt_id, "status": "pending" }),
    }
}

// Responses are always built as a serde_json::Value, both when first sent and when stored and
// replayed. A Value prints its keys in a fixed (sorted) order, so a replay is identical byte for byte.
fn respond(stored: StoredResponse) -> (StatusCode, Json<Value>) {
    let status = StatusCode::from_u16(stored.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(stored.body))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, AppError> {
    let missing = || AppError::InvalidRequest("the Idempotency-Key header is required".into());
    let key = headers
        .get("idempotency-key")
        .ok_or_else(missing)?
        .to_str()
        .map_err(|_| missing())?;
    if key.is_empty() {
        return Err(missing());
    }
    if key.chars().count() > MAX_KEY_LEN || key.chars().any(char::is_control) {
        return Err(AppError::InvalidRequest(format!(
            "Idempotency-Key must be at most {MAX_KEY_LEN} characters with no control characters"
        )));
    }
    Ok(key.to_string())
}

/// POST /v1/invoices/{id}/pay
pub async fn pay(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let key = idempotency_key(&headers)?;
    let invoice_id = Uuid::parse_str(&id).map_err(|_| invoices::not_found())?;

    // Parse the body ourselves (not with ApiJson) because we need the parsed Value for the hash.
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::InvalidRequest(format!("invalid JSON body: {e}")))?;
    let request: PayRequest = serde_json::from_value(parsed.clone())
        .map_err(|e| AppError::InvalidRequest(e.to_string()))?;
    if request.card_token.trim().is_empty() || request.card_token.len() > 200 {
        return Err(AppError::InvalidRequest(
            "card_token must be 1 to 200 characters".into(),
        ));
    }
    let hash =
        idempotency::request_hash("POST", &format!("/v1/invoices/{invoice_id}/pay"), &parsed);

    // ---------------------------------------------------------------- Tx A
    // Short on purpose. Its only job is to record the INTENT to charge durably (the pending
    // attempt) and then let go of every lock. The slow PSP call happens after the commit.
    let mut tx = state.pool.begin().await?;

    if let Claim::Existing(existing) =
        idempotency::claim(&mut tx, auth.business_id, &key, &hash).await?
    {
        drop(tx); // nothing to keep: roll back and answer from the stored key
        return replay(&state.pool, existing, &hash).await;
    }

    // FOR UPDATE locks the invoice row so that two simultaneous pay requests (or a pay and a
    // void) are handled one at a time: whoever locks first decides, the other sees the result.
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT state, total_cents FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state_text, total_cents)) = row else {
        return Err(invoices::not_found());
    };
    if state_text != "open" {
        return Err(AppError::Conflict {
            code: "invoice_not_payable".into(),
            message: format!("invoice is {state_text}; only open invoices can be paid"),
        });
    }

    // The partial unique index attempts_one_pending allows at most ONE pending attempt per
    // invoice. It is the database's own guarantee against a double charge: even if a bug let
    // two requests past the checks above, the second insert fails here instead of reaching the PSP.
    let attempt_id = Uuid::now_v7();
    let inserted = sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, business_id, amount_cents, card_token, status) \
         VALUES ($1, $2, $3, $4, $5, 'pending')",
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(auth.business_id)
    .bind(total_cents) // the amount is the invoice total at this moment; line items cannot change
    .bind(&request.card_token)
    .execute(&mut *tx)
    .await;
    if let Err(e) = inserted {
        if is_unique_violation(&e, "attempts_one_pending") {
            return Err(AppError::Conflict {
                code: "payment_in_progress".into(),
                message: "a payment attempt is already in progress for this invoice".into(),
            });
        }
        return Err(e.into());
    }
    idempotency::attach_attempt(&mut tx, auth.business_id, &key, attempt_id).await?;
    tx.commit().await?; // the row lock is released here, before any network call

    // ---------------------------------------------------------------- PSP call
    // A spawned task is independent of this HTTP request: if the client disconnects or we stop
    // waiting, the task keeps running and still finalises the attempt. The oneshot channel is a
    // one-use pipe that lets the handler receive the task's answer if it arrives in time.
    let attempt = AttemptRef {
        id: attempt_id,
        invoice_id,
        business_id: auth.business_id,
    };
    let (done_tx, done_rx) = oneshot::channel::<Option<StoredResponse>>();
    let task_state = state.clone(); // Arc clone: a cheap extra handle to the same AppState
    tokio::spawn(async move {
        let result = run_charge(&task_state, attempt, request.card_token, total_cents).await;
        let _ = done_tx.send(result); // fails only if the handler stopped waiting; that is fine
    });

    match tokio::time::timeout(Duration::from_secs(state.config.psp_wait_secs), done_rx).await {
        Ok(Ok(Some(stored))) => Ok(respond(stored)),
        // Still pending (slow or ambiguous PSP), the task ended without an answer, or we ran out
        // of patience: tell the caller it is in progress. The task or the reconciler finishes it.
        _ => Ok(respond(pending_response(attempt_id))),
    }
}

/// Runs in the spawned task: charge, resolve a non-timeout ambiguity once, then finalize.
/// Returns the stored response, or None if the attempt is left pending (always so after a timeout).
async fn run_charge(
    state: &AppState,
    attempt: AttemptRef,
    card_token: String,
    amount_cents: i64,
) -> Option<StoredResponse> {
    let reference = attempt.id.to_string();
    let charge_result = state
        .psp
        .charge(&reference, amount_cents, &card_token)
        .await;
    // TEST ONLY (CRASH_AFTER_PSP_CALL=true, off by default): die after the PSP has answered but
    // before the result is saved. The attempt is left pending in the database, which is exactly the
    // situation the reconciler exists to repair.
    if state.config.crash_after_psp_call {
        tracing::error!(attempt_id = %attempt.id, "CRASH_AFTER_PSP_CALL is set: exiting before finalize");
        std::process::exit(1);
    }
    let outcome = match charge_result {
        ChargeOutcome::Succeeded { psp_ref } => Outcome::Succeeded { psp_ref },
        ChargeOutcome::Failed { code } => Outcome::Failed { code },
        // Our deadline fired, so the request may still be on its way to the PSP. A lookup now could
        // say "no record" and be wrong, and failing the attempt would free the invoice for a second
        // payment while the first charge lands later. Leave it pending: the reconciler asks again
        // and only concludes "no charge" once the attempt is old enough (DESIGN.md section 3).
        ChargeOutcome::TimedOut => return None,
        ChargeOutcome::Ambiguous => {
            // A connection error or 5xx: we do not know whether money moved, so we ask instead of
            // guessing. Only "the PSP has never heard of it" proves there is no charge. Any other
            // answer stays pending and the reconciler settles it.
            match state.psp.lookup(&reference).await {
                LookupOutcome::NotFound => Outcome::Failed {
                    code: PSP_UNAVAILABLE.to_string(),
                },
                _ => return None,
            }
        }
    };
    match finalize(&state.pool, &attempt, outcome, &uuid_text()).await {
        Ok(stored) => Some(stored),
        Err(e) => {
            // The attempt stays pending in the database; the reconciler will pick it up.
            tracing::error!(attempt_id = %attempt.id, "finalize failed: {e}");
            None
        }
    }
}

fn uuid_text() -> String {
    Uuid::now_v7().to_string()
}

/// Records the outcome of an attempt. ONE function, used by the spawned task and by the
/// reconciler, so both finish a payment in exactly the same way.
///
/// Everything happens in one transaction. `request_id` only labels the stored error envelope.
pub async fn finalize(
    pool: &PgPool,
    attempt: &AttemptRef,
    outcome: Outcome,
    request_id: &str,
) -> Result<StoredResponse, AppError> {
    let mut tx = pool.begin().await?;

    // Lock the invoice first. Tx A also locks the invoice before touching attempts; keeping the
    // same order everywhere prevents two transactions each holding what the other needs.
    sqlx::query("SELECT id FROM invoices WHERE id = $1 FOR UPDATE")
        .bind(attempt.invoice_id)
        .fetch_one(&mut *tx)
        .await?;

    let (status, psp_ref, failure_code) = match &outcome {
        Outcome::Succeeded { psp_ref } => ("succeeded", Some(psp_ref.as_str()), None),
        Outcome::Failed { code } => ("failed", None, Some(code.as_str())),
    };

    // Compare-and-swap on `pending`: whoever gets here first wins. Zero rows means the attempt
    // was already finalised by the task or the reconciler, so we change nothing and just return
    // the answer that was stored then.
    let updated = sqlx::query_as::<_, AttemptView>(&format!(
        "UPDATE payment_attempts SET status = $2, psp_ref = $3, failure_code = $4, completed_at = now() \
         WHERE id = $1 AND status = 'pending' RETURNING {ATTEMPT_COLUMNS}"
    ))
    .bind(attempt.id)
    .bind(status)
    .bind(psp_ref)
    .bind(failure_code)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(attempt_view) = updated else {
        return idempotency::load_response(&mut tx, attempt.id)
            .await?
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "attempt {} is finalised but has no stored response",
                    attempt.id
                ))
            });
    };

    let succeeded = matches!(outcome, Outcome::Succeeded { .. });
    if succeeded {
        let paid = sqlx::query("UPDATE invoices SET state = 'paid', paid_at = now(), updated_at = now() WHERE id = $1 AND state = 'open'")
            .bind(attempt.invoice_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if paid == 0 {
            // The money moved, so the attempt stays succeeded even though the invoice was not open
            // any more. Someone needs to look at this.
            tracing::error!(
                attempt_id = %attempt.id,
                invoice_id = %attempt.invoice_id,
                "payment succeeded but the invoice was not open; attempt kept as succeeded"
            );
        }
    }

    let full: InvoiceWithItems =
        invoices::load_full(&mut tx, attempt.business_id, attempt.invoice_id)
            .await?
            .ok_or_else(invoices::not_found)?;
    let attempt_json =
        serde_json::to_value(&attempt_view).map_err(|e| AppError::Internal(e.to_string()))?;

    // The event snapshot: the invoice plus the attempt that caused it.
    let mut object = invoices::to_json(&full)?;
    object["payment_attempt"] = attempt_json.clone();
    let event_type = if succeeded {
        "invoice.paid"
    } else {
        "invoice.payment_failed"
    };
    events::emit(&mut tx, attempt.business_id, event_type, object).await?;

    let stored = match outcome {
        Outcome::Succeeded { .. } => StoredResponse {
            status: 200,
            body: json!({ "invoice": invoices::to_json(&full)?, "payment_attempt": attempt_json }),
        },
        Outcome::Failed { code } => {
            let error = if code == PSP_UNAVAILABLE {
                AppError::PspUnavailable
            } else {
                AppError::PaymentFailed(code)
            };
            let (http_status, body) = error.envelope(request_id);
            StoredResponse {
                status: http_status.as_u16(),
                body,
            }
        }
    };
    idempotency::store_response(&mut tx, attempt.id, &stored).await?;
    tx.commit().await?;
    Ok(stored)
}

/// Answers a request whose Idempotency-Key was already used.
async fn replay(
    pool: &PgPool,
    existing: ExistingKey,
    hash: &[u8],
) -> Result<(StatusCode, Json<Value>), AppError> {
    if existing.request_hash != hash {
        return Err(AppError::Unprocessable {
            code: "idempotency_key_reuse".into(),
            message: "this Idempotency-Key was already used with a different request".into(),
        });
    }
    if let Some(stored) = existing.response {
        return Ok(respond(stored)); // finished earlier: same answer, byte for byte
    }
    let Some(attempt_id) = existing.attempt_id else {
        return Err(AppError::Internal(
            "idempotency key has neither an attempt nor a response".into(),
        ));
    };
    let mut conn = pool.acquire().await?;
    let status: String = sqlx::query_scalar("SELECT status FROM payment_attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(&mut *conn)
        .await?;
    if status == "pending" {
        return Ok(respond(pending_response(attempt_id))); // still running: report the live state
    }
    // It finished between our two reads; the final answer is stored by now.
    match idempotency::load_response(&mut conn, attempt_id).await? {
        Some(stored) => Ok(respond(stored)),
        None => Err(AppError::Internal(
            "finalised attempt has no stored response".into(),
        )),
    }
}

fn is_unique_violation(error: &sqlx::Error, constraint: &str) -> bool {
    match error {
        sqlx::Error::Database(db) => {
            db.code().as_deref() == Some("23505") && db.constraint() == Some(constraint)
        }
        _ => false,
    }
}

/// GET /v1/invoices/{id}/payment_attempts
pub async fn list_attempts(
    auth: AuthedBusiness,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let invoice_id = Uuid::parse_str(&id).map_err(|_| invoices::not_found())?;
    let exists =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM invoices WHERE id = $1 AND business_id = $2")
            .bind(invoice_id)
            .bind(auth.business_id)
            .fetch_optional(&state.pool)
            .await?;
    if exists.is_none() {
        return Err(invoices::not_found());
    }
    let attempts = sqlx::query_as::<_, AttemptView>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM payment_attempts WHERE invoice_id = $1 AND business_id = $2 ORDER BY created_at, id"
    ))
    .bind(invoice_id)
    .bind(auth.business_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({ "data": attempts })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(key: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(k) = key {
            h.insert("idempotency-key", HeaderValue::from_str(k).unwrap());
        }
        h
    }

    #[test]
    fn idempotency_key_header_rules() {
        assert!(idempotency_key(&headers(None)).is_err());
        assert!(idempotency_key(&headers(Some(""))).is_err());
        assert_eq!(
            idempotency_key(&headers(Some("abc-123"))).unwrap(),
            "abc-123"
        );
        assert!(idempotency_key(&headers(Some(&"k".repeat(255)))).is_ok());
        assert!(idempotency_key(&headers(Some(&"k".repeat(256)))).is_err());
    }

    #[test]
    fn pending_body_shape() {
        let id = Uuid::now_v7();
        let r = pending_response(id);
        assert_eq!(r.status, 202);
        assert_eq!(r.body["status"], "pending");
        assert_eq!(r.body["attempt_id"], id.to_string());
    }

    #[test]
    fn stored_envelope_for_psp_unavailable_is_502() {
        let (status, body) = AppError::PspUnavailable.envelope("req-1");
        assert_eq!(status.as_u16(), 502);
        assert_eq!(body["error"]["code"], "psp_unavailable");
        assert_eq!(body["error"]["request_id"], "req-1");
    }
}
