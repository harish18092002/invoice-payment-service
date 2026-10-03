// The reconciler (DESIGN.md section 3(c)): finishes payment attempts that are stuck `pending`.
//
// An attempt is stuck when the normal path never recorded its result: the service crashed after
// the PSP answered, the PSP was slow, or the PSP's answer was ambiguous. The attempt row was saved
// BEFORE the PSP was called, so it is always there to be found. Here we ask the PSP what happened
// to that reference and then finish the attempt with the very same `finalize` the payment path uses.
use std::time::Duration;

use sqlx::PgPool;
use tokio::{task::JoinSet, time::MissedTickBehavior};
use uuid::Uuid;

use crate::{
    payments::{AttemptRef, Outcome, PSP_UNAVAILABLE, finalize},
    psp::{LookupOutcome, PspClient},
};

const BATCH_SIZE: i64 = 20;

pub struct Settings {
    pub interval: Duration,
    pub min_age_secs: f64,
    pub not_found_after_secs: f64,
}

struct Stuck {
    attempt: AttemptRef,
    age_secs: f64,
}

/// What to do with a stuck attempt, given what the PSP said.
#[derive(Debug, PartialEq)]
enum Decision {
    Finalize(Outcome),
    Leave(&'static str),
}

// Pure function so the rules can be unit-tested without a database or a PSP.
fn decide(lookup: LookupOutcome, age_secs: f64, not_found_after_secs: f64) -> Decision {
    match lookup {
        LookupOutcome::Succeeded { psp_ref } => Decision::Finalize(Outcome::Succeeded { psp_ref }),
        LookupOutcome::Failed { code } => Decision::Finalize(Outcome::Failed { code }),
        LookupOutcome::Processing => Decision::Leave("the PSP is still processing it"),
        // "No record" could also mean the request is still travelling to the PSP, so it only counts
        // as proof of "no charge" once the attempt is old enough that a late arrival is implausible.
        LookupOutcome::NotFound if age_secs > not_found_after_secs => {
            Decision::Finalize(Outcome::Failed {
                code: PSP_UNAVAILABLE.to_string(),
            })
        }
        LookupOutcome::NotFound => {
            Decision::Leave("the PSP has no record yet, but the attempt is too young to conclude")
        }
        LookupOutcome::Ambiguous => Decision::Leave("the PSP lookup was inconclusive"),
    }
}

pub async fn run(pool: PgPool, psp: PspClient, settings: Settings) {
    let mut ticker = tokio::time::interval(settings.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let settings = std::sync::Arc::new(settings);
    loop {
        ticker.tick().await;
        match pick_stuck(&pool, settings.min_age_secs).await {
            Ok(stuck) => {
                // Lookups for one batch run concurrently; each is a short read with its own timeout.
                let mut batch = JoinSet::new();
                for item in stuck {
                    batch.spawn(resolve_one(
                        pool.clone(),
                        psp.clone(),
                        settings.clone(),
                        item,
                    ));
                }
                while batch.join_next().await.is_some() {}
            }
            Err(e) => tracing::error!("reconciler could not list pending attempts: {e}"),
        }
    }
}

/// Lists pending attempts that are old enough. The transaction exists only for this one query and
/// ends before any PSP call: no lock or transaction is ever held across the network.
///
/// FOR UPDATE SKIP LOCKED skips attempts that are being finalised right this moment. It is not what
/// keeps us correct: if two reconcilers (or the reconciler and the payment task) work on the same
/// attempt, `finalize` only lets the first one change it and returns the stored answer to the rest.
async fn pick_stuck(pool: &PgPool, min_age_secs: f64) -> Result<Vec<Stuck>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(Uuid, Uuid, Uuid, f64)> = sqlx::query_as(
        "SELECT id, invoice_id, business_id, EXTRACT(EPOCH FROM (now() - created_at))::float8 \
         FROM payment_attempts \
         WHERE status = 'pending' AND created_at <= now() - make_interval(secs => $1) \
         ORDER BY created_at LIMIT $2 FOR UPDATE SKIP LOCKED",
    )
    .bind(min_age_secs)
    .bind(BATCH_SIZE)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(id, invoice_id, business_id, age_secs)| Stuck {
            attempt: AttemptRef {
                id,
                invoice_id,
                business_id,
            },
            age_secs,
        })
        .collect())
}

async fn resolve_one(
    pool: PgPool,
    psp: PspClient,
    settings: std::sync::Arc<Settings>,
    item: Stuck,
) {
    let attempt_id = item.attempt.id;
    // The PSP knows the charge by our attempt id (that is what we sent as the reference).
    let lookup = psp.lookup(&attempt_id.to_string()).await;
    match decide(lookup, item.age_secs, settings.not_found_after_secs) {
        Decision::Leave(why) => {
            tracing::info!(attempt_id = %attempt_id, age_secs = item.age_secs as u64, "reconciler left attempt pending: {why}");
        }
        Decision::Finalize(outcome) => {
            let label = match &outcome {
                Outcome::Succeeded { .. } => "succeeded".to_string(),
                Outcome::Failed { code } => format!("failed ({code})"),
            };
            match finalize(&pool, &item.attempt, outcome, &Uuid::now_v7().to_string()).await {
                Ok(_) => {
                    tracing::info!(attempt_id = %attempt_id, outcome = %label, "reconciler resolved attempt")
                }
                Err(e) => {
                    tracing::error!(attempt_id = %attempt_id, "reconciler could not finalize: {e}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: f64 = 120.0;

    fn ok() -> LookupOutcome {
        LookupOutcome::Succeeded {
            psp_ref: "p1".into(),
        }
    }

    #[test]
    fn a_definite_answer_is_finalized_at_any_age() {
        for age in [50.0, 500.0] {
            assert!(matches!(
                decide(ok(), age, LIMIT),
                Decision::Finalize(Outcome::Succeeded { .. })
            ));
            assert!(matches!(
                decide(
                    LookupOutcome::Failed {
                        code: "card_declined".into()
                    },
                    age,
                    LIMIT
                ),
                Decision::Finalize(Outcome::Failed { .. })
            ));
        }
    }

    #[test]
    fn processing_and_ambiguous_are_left_alone() {
        for lookup in [LookupOutcome::Processing, LookupOutcome::Ambiguous] {
            assert!(matches!(
                decide(lookup, 10_000.0, LIMIT),
                Decision::Leave(_)
            ));
        }
    }

    #[test]
    fn not_found_only_fails_the_attempt_after_the_age_limit() {
        assert!(matches!(
            decide(LookupOutcome::NotFound, 60.0, LIMIT),
            Decision::Leave(_)
        ));
        assert!(matches!(
            decide(LookupOutcome::NotFound, 120.0, LIMIT),
            Decision::Leave(_)
        )); // exactly at the limit: not yet
        match decide(LookupOutcome::NotFound, 120.5, LIMIT) {
            Decision::Finalize(Outcome::Failed { code }) => assert_eq!(code, PSP_UNAVAILABLE),
            other => panic!("expected psp_unavailable failure, got {other:?}"),
        }
    }
}
