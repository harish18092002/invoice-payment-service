// The webhook dispatcher: a background task that delivers outbox rows (webhook_deliveries).
//
// Every poll it "claims" due rows by pushing their next_attempt_at 60 seconds into the future
// (a lease). If this process crashes mid-delivery, the lease expires and the row is picked up
// again. Claiming uses FOR UPDATE SKIP LOCKED, so several dispatchers could run side by side
// without taking the same row. The HTTP call itself happens outside any transaction.
use std::time::Duration;

use rand::Rng;
use sqlx::PgPool;
use tokio::{task::JoinSet, time::MissedTickBehavior};
use uuid::Uuid;

use crate::webhooks::signer;

const BATCH_SIZE: i64 = 10;
const MAX_ATTEMPTS: i32 = 8;
const LEASE_SECS: i32 = 60;
/// Seconds to wait after failed attempt 1, 2, ... 7 before the next one. Attempt 8 failing means dead.
pub const RETRY_DELAYS_SECS: [f64; 7] = [10.0, 60.0, 300.0, 1800.0, 7200.0, 21600.0, 43200.0];

struct Due {
    id: Uuid,
    event_id: Uuid,
    endpoint_id: Uuid,
    attempt_count: i32,
}

/// Runs forever. `delay_scale` multiplies the retry delays (1.0 in production).
pub async fn run(pool: PgPool, http: reqwest::Client, poll: Duration, delay_scale: f64) {
    let mut ticker = tokio::time::interval(poll);
    // If a poll takes longer than the interval (slow endpoints), do not fire a burst of catch-up ticks.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match claim_due(&pool).await {
            Ok(due) => {
                // Deliveries in one batch run concurrently, so one slow endpoint does not hold up the others.
                let mut batch = JoinSet::new();
                for delivery in due {
                    batch.spawn(deliver_one(
                        pool.clone(),
                        http.clone(),
                        delivery,
                        delay_scale,
                    ));
                }
                while batch.join_next().await.is_some() {}
            }
            Err(e) => tracing::error!("webhook dispatcher could not claim deliveries: {e}"),
        }
    }
}

async fn claim_due(pool: &PgPool) -> Result<Vec<Due>, sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, Uuid, i32)> = sqlx::query_as(
        "UPDATE webhook_deliveries SET next_attempt_at = now() + make_interval(secs => $1) \
         WHERE id IN ( \
             SELECT id FROM webhook_deliveries \
             WHERE status = 'pending' AND next_attempt_at <= now() \
             ORDER BY next_attempt_at LIMIT $2 FOR UPDATE SKIP LOCKED) \
         RETURNING id, event_id, endpoint_id, attempt_count",
    )
    .bind(LEASE_SECS as f64)
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, event_id, endpoint_id, attempt_count)| Due {
            id,
            event_id,
            endpoint_id,
            attempt_count,
        })
        .collect())
}

/// Base delay before the next attempt, given how many attempts have failed so far.
/// None means the budget is spent and the delivery is dead.
fn retry_delay_secs(failed_attempts: i32) -> Option<f64> {
    if failed_attempts >= MAX_ATTEMPTS {
        return None;
    }
    RETRY_DELAYS_SECS
        .get((failed_attempts - 1) as usize)
        .copied()
}

async fn deliver_one(pool: PgPool, http: reqwest::Client, due: Due, delay_scale: f64) {
    if let Err(e) = try_deliver(&pool, &http, &due, delay_scale).await {
        // The row stays leased and is retried when the lease runs out.
        tracing::error!(delivery_id = %due.id, "webhook delivery bookkeeping failed: {e}");
    }
}

async fn try_deliver(
    pool: &PgPool,
    http: &reqwest::Client,
    due: &Due,
    delay_scale: f64,
) -> Result<(), sqlx::Error> {
    let row: Option<(serde_json::Value, String, String)> = sqlx::query_as(
        "SELECT e.payload, w.url, w.secret FROM events e, webhook_endpoints w WHERE e.id = $1 AND w.id = $2",
    )
    .bind(due.event_id)
    .bind(due.endpoint_id)
    .fetch_optional(pool)
    .await?;
    let Some((payload, url, secret)) = row else {
        return Ok(());
    };

    // Serialize once and send exactly these bytes, so the signature covers what the receiver reads.
    let body = payload.to_string();
    // Signed fresh on every attempt: a retry after hours would otherwise fail the receiver's
    // 300 second timestamp check.
    let timestamp = chrono::Utc::now().timestamp();
    let signature = signer::header_value(&secret, timestamp, &body);

    let result = http
        .post(&url)
        .header("content-type", "application/json")
        .header("Dodo-Event-Id", due.event_id.to_string())
        .header("Dodo-Signature", signature)
        .body(body)
        .send()
        .await;

    // Outcome: (http status if any, error text if failed).
    let (status_code, error): (Option<i32>, Option<String>) = match result {
        Ok(resp) if resp.status().is_success() => (Some(resp.status().as_u16() as i32), None),
        Ok(resp) => (
            Some(resp.status().as_u16() as i32),
            Some(format!("receiver answered {}", resp.status().as_u16())),
        ),
        // A fixed description per failure kind: reqwest's own text is vague, and the endpoint
        // URL must not leak into the database or logs.
        Err(e) => {
            let reason = if e.is_timeout() {
                "request timed out after 5 seconds"
            } else if e.is_connect() {
                "could not connect to the endpoint"
            } else if e.is_redirect() {
                "endpoint answered with a redirect, which is not followed"
            } else {
                "request failed"
            };
            (None, Some(reason.to_string()))
        }
    };

    let attempts = due.attempt_count + 1;
    if error.is_none() {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'delivered', attempt_count = $2, last_status_code = $3, \
             last_error = NULL, delivered_at = now() WHERE id = $1 AND status = 'pending'",
        )
        .bind(due.id)
        .bind(attempts)
        .bind(status_code)
        .execute(pool)
        .await?;
    } else if let Some(base) = retry_delay_secs(attempts) {
        // +/-20% jitter spreads retries out so many failing deliveries do not all return at once.
        let jitter: f64 = rand::rng().random_range(0.8..1.2);
        let wait = base * delay_scale * jitter;
        sqlx::query(
            "UPDATE webhook_deliveries SET attempt_count = $2, last_status_code = $3, last_error = $4, \
             next_attempt_at = now() + make_interval(secs => $5) WHERE id = $1 AND status = 'pending'",
        )
        .bind(due.id)
        .bind(attempts)
        .bind(status_code)
        .bind(&error)
        .bind(wait)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE webhook_deliveries SET status = 'dead', attempt_count = $2, last_status_code = $3, \
             last_error = $4 WHERE id = $1 AND status = 'pending'",
        )
        .bind(due.id)
        .bind(attempts)
        .bind(status_code)
        .bind(&error)
        .execute(pool)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_follow_the_schedule_then_die_at_eight() {
        let expected = [10.0, 60.0, 300.0, 1800.0, 7200.0, 21600.0, 43200.0];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(
                retry_delay_secs(i as i32 + 1),
                Some(*want),
                "after failure {}",
                i + 1
            );
        }
        assert_eq!(retry_delay_secs(8), None);
        assert_eq!(retry_delay_secs(9), None);
    }

    #[test]
    fn schedule_totals_about_twenty_point_six_hours() {
        let total: f64 = RETRY_DELAYS_SECS.iter().sum();
        assert!((total / 3600.0 - 20.6).abs() < 0.1);
    }
}
