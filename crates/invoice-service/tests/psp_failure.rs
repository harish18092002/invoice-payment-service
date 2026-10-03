mod common;

use std::time::Duration;

use common::{eventually, setup};

// DESIGN.md section 3(b): a slow PSP. The caller gets 202 after PSP_WAIT_SECS, the attempt stays
// pending, a second pay is refused, and the invoice is paid once the PSP finishes.
#[tokio::test]
async fn slow_psp_returns_202_then_the_invoice_gets_paid_once() {
    let ctx = setup().await;
    let invoice_id = ctx.create_open_invoice().await;
    let before = ctx.psp_stats().await;
    let wait = ctx.psp_wait_secs as f64;

    let first = ctx.pay(&invoice_id, "slow-1", "tok_timeout").await;
    assert_eq!(first.status, 202, "{}", first.raw);
    assert_eq!(first.body["status"], "pending");
    let secs = first.elapsed.as_secs_f64();
    assert!(
        secs >= wait * 0.9 && secs < wait + 1.5,
        "202 should arrive after about {wait}s, took {secs}s"
    );

    // Still in flight at the PSP: the invoice is open and exactly one attempt is pending.
    assert_eq!(ctx.invoice_state(&invoice_id).await, "open");
    let attempts = ctx.attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["status"], "pending");

    // A second pay with a new key must not start another charge.
    let second = ctx.pay(&invoice_id, "slow-2", "tok_success").await;
    assert_eq!(second.status, 409, "{}", second.raw);
    assert_eq!(second.body["error"]["code"], "payment_in_progress");

    // The PSP finishes (the test stack makes it take 4 s) and either the payment task or the
    // reconciler records the result. Poll with a deadline instead of sleeping a fixed time.
    eventually(
        "the invoice to become paid",
        Duration::from_secs(30),
        || async { ctx.invoice_state(&invoice_id).await == "paid" },
    )
    .await;

    let attempts = ctx.attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["status"], "succeeded");
    let after = ctx.psp_stats().await;
    assert_eq!(
        after.charges_created - before.charges_created,
        1,
        "exactly one charge at the PSP"
    );
    assert_eq!(ctx.events_for(&invoice_id, "invoice.paid").await.len(), 1);

    // Replaying the first key now returns the final answer, not the 202.
    let replay = ctx.pay(&invoice_id, "slow-1", "tok_timeout").await;
    assert_eq!(replay.status, 200, "{}", replay.raw);
}

// DESIGN.md section 3, "Ambiguous PSP answers": a client-side timeout gets NO immediate verdict.
//
// tok_late is a request that is still on its way. The test stack makes the app give up on the PSP
// call after 5 s, while the PSP only records the charge at 7 s. In between, a lookup says "no
// record", which proves nothing. If the app took that as "no charge" it would fail the attempt at
// 5 s, the invoice would stay open (payable again, a double charge waiting to happen), and the
// charge that lands at 7 s would exist at the PSP with no record on our side.
// Correct behaviour: the attempt stays pending, the charge lands, the reconciler records it.
#[tokio::test]
async fn a_client_timeout_is_not_judged_no_charge_and_the_late_charge_is_recorded() {
    let ctx = setup().await;
    let invoice_id = ctx.create_open_invoice().await;
    let before = ctx.psp_stats().await;

    let first = ctx.pay(&invoice_id, "late-1", "tok_late").await;
    assert_eq!(first.status, 202, "{}", first.raw);

    // Wait for the attempt to reach a final state, then check it is the right one. Checking the
    // state (not a fixed time) keeps the test stable and gives a clear message if it goes wrong.
    eventually(
        "the attempt to leave pending",
        Duration::from_secs(20),
        || async { ctx.attempts(&invoice_id).await[0]["status"] != "pending" },
    )
    .await;

    let attempts = ctx.attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1, "no second attempt was needed");
    assert_eq!(
        attempts[0]["status"], "succeeded",
        "a timeout must not be judged 'no charge'; the attempt is {}",
        attempts[0]
    );
    assert_eq!(ctx.invoice_state(&invoice_id).await, "paid");

    // Exactly one charge exists at the PSP, and it is the one we recorded.
    let after = ctx.psp_stats().await;
    assert_eq!(after.charges_created - before.charges_created, 1);
    assert_eq!(after.charges_succeeded - before.charges_succeeded, 1);
    assert_eq!(ctx.events_for(&invoice_id, "invoice.paid").await.len(), 1);
    assert!(
        ctx.events_for(&invoice_id, "invoice.payment_failed")
            .await
            .is_empty()
    );
}

// A network error is ambiguous; the lookup finds no charge, so the attempt fails as psp_unavailable
// and the invoice stays payable.
#[tokio::test]
async fn network_error_returns_502_and_the_invoice_can_be_paid_with_a_new_key() {
    let ctx = setup().await;
    let invoice_id = ctx.create_open_invoice().await;
    let before = ctx.psp_stats().await;

    let failed = ctx.pay(&invoice_id, "net-1", "tok_network_error").await;
    assert_eq!(failed.status, 502, "{}", failed.raw);
    assert_eq!(failed.body["error"]["code"], "psp_unavailable");
    assert_eq!(ctx.invoice_state(&invoice_id).await, "open");
    let attempts = ctx.attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["status"], "failed");
    assert_eq!(attempts[0]["failure_code"], "psp_unavailable");

    let retry = ctx.pay(&invoice_id, "net-2", "tok_success").await;
    assert_eq!(retry.status, 200, "{}", retry.raw);
    assert_eq!(ctx.invoice_state(&invoice_id).await, "paid");

    // Only the successful retry ever created a charge at the PSP.
    let after = ctx.psp_stats().await;
    assert_eq!(after.charges_created - before.charges_created, 1);
}
