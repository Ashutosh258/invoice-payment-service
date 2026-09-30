mod common;

use std::collections::HashSet;
use std::sync::Arc;

use common::TestApp;
use reqwest::StatusCode;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Barrier;

const CLIENTS: usize = 20;

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn concurrent_payments_charge_the_invoice_exactly_once(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = Arc::new(TestApp::spawn(common::pool(opts, connect).await).await);
    let invoice_id = app.open_invoice().await;

    let barrier = Arc::new(Barrier::new(CLIENTS));
    let requests = (0..CLIENTS).map(|i| {
        let (app, invoice_id, barrier) = (app.clone(), invoice_id.clone(), barrier.clone());
        tokio::spawn(async move {
            barrier.wait().await;
            app.pay(&invoice_id, "tok_success", &format!("client-{i}")).await
        })
    });
    let responses: Vec<_> = futures_join_all(requests).await;

    let succeeded: Vec<_> = responses
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .collect();
    assert_eq!(
        succeeded.len(),
        1,
        "exactly one payment may succeed: {responses:#?}"
    );

    for (status, body) in responses.iter().filter(|(status, _)| *status != StatusCode::OK) {
        assert_eq!(*status, StatusCode::CONFLICT, "{body}");
        let code = body["error"]["code"].as_str().unwrap();
        assert!(
            ["payment_in_progress", "invoice_already_paid"].contains(&code),
            "{body}"
        );
    }

    assert_eq!(
        app.psp.captured_charges(),
        1,
        "the card must be charged exactly once"
    );
    assert_eq!(app.psp.charge_requests(), 1, "losers must never reach the PSP");

    let invoice = app.invoice(&invoice_id).await;
    assert_eq!(invoice["status"], "paid");
    let attempts = app.payment_attempts(&invoice_id).await;
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0]["status"], "succeeded");
    assert_eq!(attempts[0]["amount_cents"], invoice["total_cents"]);
    assert_eq!(app.event_count("invoice.paid").await, 1);
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn concurrent_retries_with_one_key_share_a_single_attempt(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let app = Arc::new(TestApp::spawn(common::pool(opts, connect).await).await);
    let invoice_id = app.open_invoice().await;

    let barrier = Arc::new(Barrier::new(CLIENTS));
    let requests = (0..CLIENTS).map(|_| {
        let (app, invoice_id, barrier) = (app.clone(), invoice_id.clone(), barrier.clone());
        tokio::spawn(async move {
            barrier.wait().await;
            app.pay(&invoice_id, "tok_success", "same-key").await
        })
    });
    let responses: Vec<_> = futures_join_all(requests).await;

    let attempt_ids: HashSet<&str> = responses
        .iter()
        .map(|(status, body)| {
            assert!(
                matches!(*status, StatusCode::OK | StatusCode::ACCEPTED),
                "{status}: {body}"
            );
            body["id"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        attempt_ids.len(),
        1,
        "every response must describe the same attempt"
    );

    assert_eq!(app.psp.captured_charges(), 1);
    assert_eq!(app.invoice(&invoice_id).await["status"], "paid");
}

async fn futures_join_all<T: Send + 'static>(
    handles: impl Iterator<Item = tokio::task::JoinHandle<T>>,
) -> Vec<T> {
    let mut results = Vec::new();
    for handle in handles.collect::<Vec<_>>() {
        results.push(handle.await.expect("request task panicked"));
    }
    results
}
