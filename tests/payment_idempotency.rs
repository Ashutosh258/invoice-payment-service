mod common;

use common::TestApp;
use reqwest::StatusCode;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn retry_returns_the_same_response_without_calling_the_psp_again(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id = app.open_invoice().await;

    let (first_status, first_body) = app.pay(&invoice_id, "tok_success", "order-42-attempt-1").await;
    assert_eq!(first_status, StatusCode::OK, "{first_body}");
    assert_eq!(first_body["status"], "succeeded");
    assert_eq!(app.psp.charge_requests(), 1);

    for _ in 0..3 {
        let (status, body) = app.pay(&invoice_id, "tok_success", "order-42-attempt-1").await;
        assert_eq!(status, first_status);
        assert_eq!(body, first_body, "a replay must return the original response");
    }

    assert_eq!(app.psp.charge_requests(), 1, "replays must not reach the PSP");
    assert_eq!(app.psp.captured_charges(), 1);
    assert_eq!(app.payment_attempts(&invoice_id).await.len(), 1);
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn declined_payments_replay_as_declined(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id = app.open_invoice().await;

    let (status, first) = app.pay(&invoice_id, "tok_card_declined", "k1").await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{first}");
    assert_eq!(first["error"]["code"], "payment_failed");
    assert_eq!(
        first["error"]["details"]["payment_attempt"]["failure_code"],
        "card_declined"
    );

    let (status, replay) = app.pay(&invoice_id, "tok_card_declined", "k1").await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert_eq!(replay, first);
    assert_eq!(app.psp.charge_requests(), 1);

    assert_eq!(app.invoice(&invoice_id).await["status"], "open");
    let (status, body) = app.pay(&invoice_id, "tok_success", "k2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.invoice(&invoice_id).await["status"], "paid");
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn reusing_a_key_for_a_different_request_is_rejected(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id = app.open_invoice().await;
    let other_invoice_id = app.open_invoice().await;

    let (status, _) = app.pay(&invoice_id, "tok_card_declined", "reused").await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);

    let (status, body) = app.pay(&invoice_id, "tok_success", "reused").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "idempotency_key_reused");

    let (status, body) = app.pay(&other_invoice_id, "tok_card_declined", "reused").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "idempotency_key_reused");

    assert_eq!(app.psp.charge_requests(), 1);
    assert_eq!(app.invoice(&other_invoice_id).await["status"], "open");
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn paying_a_paid_invoice_is_rejected_without_a_charge(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id = app.open_invoice().await;

    let (status, _) = app.pay(&invoice_id, "tok_success", "first").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = app.pay(&invoice_id, "tok_success", "second").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "invoice_already_paid");
    assert_eq!(app.psp.charge_requests(), 1);
}
