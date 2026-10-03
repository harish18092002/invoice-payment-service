mod common;

use common::setup;
use serde_json::json;

#[tokio::test]
async fn same_key_and_body_replays_the_identical_response_without_a_second_charge() {
    let ctx = setup().await;
    let invoice_id = ctx.create_open_invoice().await;

    let first = ctx.pay(&invoice_id, "idem-same", "tok_success").await;
    assert_eq!(first.status, 200, "{}", first.raw);
    let stats_after_first = ctx.psp_stats().await;

    let second = ctx.pay(&invoice_id, "idem-same", "tok_success").await;
    assert_eq!(second.status, first.status);
    assert_eq!(
        second.raw, first.raw,
        "a replay must be identical, byte for byte"
    );

    let stats_after_second = ctx.psp_stats().await;
    assert_eq!(
        stats_after_second.post_calls, stats_after_first.post_calls,
        "a replay must not call the PSP"
    );
    assert_eq!(
        stats_after_second.charges_created,
        stats_after_first.charges_created
    );
    assert_eq!(ctx.attempts(&invoice_id).await.len(), 1);
}

#[tokio::test]
async fn same_key_with_a_different_body_is_rejected() {
    let ctx = setup().await;
    let invoice_id = ctx.create_open_invoice().await;

    let first = ctx
        .pay(&invoice_id, "idem-reuse", "tok_card_declined")
        .await;
    assert_eq!(first.status, 402, "{}", first.raw);
    let before = ctx.psp_stats().await;

    let reused = ctx
        .pay_body(
            &invoice_id,
            "idem-reuse",
            json!({ "card_token": "tok_success" }),
        )
        .await;
    assert_eq!(reused.status, 422, "{}", reused.raw);
    assert_eq!(reused.body["error"]["code"], "idempotency_key_reuse");

    // Nothing was executed: no PSP call, invoice untouched.
    let after = ctx.psp_stats().await;
    assert_eq!(after.post_calls, before.post_calls);
    assert_eq!(ctx.invoice_state(&invoice_id).await, "open");
}
