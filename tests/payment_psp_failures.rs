mod common;

use std::time::{Duration, Instant};

use common::{Options, TestApp};
use invoice_service::payments::reconciler::recheck_due_payments;
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn psp_timeout_returns_pending_quickly_and_resolves_without_double_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let app = TestApp::spawn_with(
        common::pool(opts, connect).await,
        Options {
            psp_timeout: Duration::from_millis(300),
            psp_timeout_delay: Duration::from_millis(1_000),
            ..Options::default()
        },
    )
    .await;
    let invoice_id = app.open_invoice().await;

    let started = Instant::now();
    let (status, attempt) = app.pay(&invoice_id, "tok_timeout", "slow-1").await;
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "the endpoint must not wait for a hung PSP"
    );
    assert_eq!(status, StatusCode::ACCEPTED, "{attempt}");
    assert_eq!(attempt["status"], "pending");

    assert_eq!(app.invoice(&invoice_id).await["status"], "open");

    let (status, body) = app.pay(&invoice_id, "tok_success", "impatient-retry").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "payment_in_progress");
    let (status, body) = app
        .post(&format!("/v1/invoices/{invoice_id}/void"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "payment_in_progress");

    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(app.psp.captured_charges(), 1);

    assert_eq!(recheck_due_payments(&app.state).await.unwrap(), 1);

    assert_eq!(app.invoice(&invoice_id).await["status"], "paid");
    let (status, attempt) = app.pay(&invoice_id, "tok_timeout", "slow-1").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the original key now reports the final outcome"
    );
    assert_eq!(attempt["status"], "succeeded");
    assert_eq!(app.psp.captured_charges(), 1, "the replay must not charge again");
    assert_eq!(app.event_count("invoice.paid").await, 1);
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn psp_outage_never_leaves_the_invoice_stuck(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn_with(
        common::pool(opts, connect).await,
        Options {
            rechecks: 2,
            ..Options::default()
        },
    )
    .await;
    let invoice_id = app.open_invoice().await;

    let (status, attempt) = app.pay(&invoice_id, "tok_network_error", "outage-1").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{attempt}");
    assert_eq!(attempt["status"], "pending");
    assert_eq!(app.invoice(&invoice_id).await["status"], "open");

    assert_eq!(recheck_due_payments(&app.state).await.unwrap(), 1);
    assert_eq!(app.payment_attempts(&invoice_id).await[0]["status"], "pending");
    assert_eq!(recheck_due_payments(&app.state).await.unwrap(), 1);

    let attempts = app.payment_attempts(&invoice_id).await;
    assert_eq!(attempts[0]["status"], "failed");
    assert_eq!(attempts[0]["failure_code"], "psp_unavailable");
    assert_eq!(app.invoice(&invoice_id).await["status"], "open");
    assert_eq!(app.event_count("invoice.payment_failed").await, 1);
    assert_eq!(recheck_due_payments(&app.state).await.unwrap(), 0);

    let (status, body) = app.pay(&invoice_id, "tok_success", "outage-2").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.invoice(&invoice_id).await["status"], "paid");
    assert_eq!(app.psp.captured_charges(), 1);
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn unreachable_psp_is_a_definite_failure(opts: PgPoolOptions, connect: PgConnectOptions) {
    let db = common::pool(opts, connect).await;
    let app = TestApp::spawn(db).await;
    let invoice_id = app.open_invoice().await;

    let mut config = (*app.state.config).clone();
    config.payments.psp_base_url = "http://127.0.0.1:9".into();
    let state = invoice_service::AppState::new(app.state.db.clone(), config).unwrap();
    let addr = common::serve(invoice_service::router(state)).await;

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/invoices/{invoice_id}/pay"))
        .bearer_auth(&app.api_key)
        .header("Idempotency-Key", "no-psp")
        .json(&json!({ "card_token": "tok_success" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["details"]["payment_attempt"]["failure_code"],
        "psp_unavailable"
    );
    assert_eq!(app.invoice(&invoice_id).await["status"], "open");
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn crash_between_charge_and_settle_is_recovered_without_double_charge(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id: Uuid = app.open_invoice().await.parse().unwrap();

    let attempt: invoice_service::payments::PaymentAttempt = sqlx::query_as(
        "INSERT INTO payment_attempts
                (id, business_id, invoice_id, status, amount_cents, card_token, idempotency_key, next_recheck_at)
         SELECT $1, business_id, id, 'pending', total_cents, 'tok_success', 'crashy', now()
           FROM invoices WHERE id = $2
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(invoice_id)
    .fetch_one(&app.state.db)
    .await
    .unwrap();
    let outcome = app.state.psp.charge(&attempt).await;
    assert!(matches!(
        outcome,
        invoice_service::payments::psp::PspOutcome::Succeeded { .. }
    ));

    let (status, body) = app.pay(&invoice_id.to_string(), "tok_success", "crashy").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    assert_eq!(recheck_due_payments(&app.state).await.unwrap(), 1);
    let (status, body) = app.pay(&invoice_id.to_string(), "tok_success", "crashy").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(app.invoice(&invoice_id.to_string()).await["status"], "paid");
    assert_eq!(app.psp.captured_charges(), 1, "the customer must be charged once");
}
