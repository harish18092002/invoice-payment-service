mod common;

use std::sync::Arc;

use common::setup;
use tokio::{sync::Barrier, task::JoinSet};

// DESIGN.md section 3(a): many simultaneous pays on one invoice must produce exactly one charge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_concurrent_pays_charge_exactly_once() {
    let ctx = setup().await;
    let invoice_id = ctx.create_open_invoice().await;
    let before = ctx.psp_stats().await;

    // The barrier holds all 20 tasks until every one is ready, so the requests really overlap.
    let barrier = Arc::new(Barrier::new(20));
    let mut tasks = JoinSet::new();
    for i in 0..20 {
        let api = ctx.api.clone();
        let barrier = barrier.clone();
        let invoice_id = invoice_id.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            api.pay(&invoice_id, &format!("race-key-{i}"), "tok_success")
                .await
        });
    }
    let mut responses = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        responses.push(joined.expect("pay task panicked"));
    }

    let ok = responses.iter().filter(|r| r.status == 200).count();
    let conflicts: Vec<_> = responses.iter().filter(|r| r.status == 409).collect();
    assert_eq!(
        ok,
        1,
        "exactly one request should win; got {:?}",
        responses.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    assert_eq!(conflicts.len(), 19, "every other request should be a 409");
    for r in &conflicts {
        let code = r.body["error"]["code"].as_str().unwrap_or_default();
        assert!(
            code == "payment_in_progress" || code == "invoice_not_payable",
            "unexpected conflict code {code}: {}",
            r.raw
        );
    }

    // The winner's 200 is only sent after the result is saved, so these checks need no waiting.
    let after = ctx.psp_stats().await;
    assert_eq!(
        after.charges_created - before.charges_created,
        1,
        "the PSP must see exactly one charge"
    );
    assert_eq!(
        after.post_calls - before.post_calls,
        1,
        "only one request may even reach the PSP"
    );

    let attempts = ctx.attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1, "exactly one attempt row");
    assert_eq!(attempts[0]["status"], "succeeded");
    assert_eq!(ctx.invoice_state(&invoice_id).await, "paid");
    assert_eq!(
        ctx.events_for(&invoice_id, "invoice.paid").await.len(),
        1,
        "exactly one invoice.paid event"
    );
}
