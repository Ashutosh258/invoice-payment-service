mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use common::TestApp;
use invoice_service::webhooks::dispatcher::deliver_due_webhooks;
use invoice_service::webhooks::signing;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[derive(Debug, Clone)]
struct Received {
    id: String,
    timestamp: i64,
    signature: String,
    body: Bytes,
}

#[derive(Clone, Default)]
struct Receiver {
    received: Arc<Mutex<Vec<Received>>>,
}

impl Receiver {
    async fn start(status: StatusCode) -> (Self, String) {
        let receiver = Receiver::default();
        let router = Router::new()
            .fallback(
                move |State(receiver): State<Receiver>, headers: HeaderMap, body: Bytes| async move {
                    let header = |name: &str| headers.get(name).unwrap().to_str().unwrap().to_owned();
                    receiver.received.lock().unwrap().push(Received {
                        id: header("webhook-id"),
                        timestamp: header("webhook-timestamp").parse().unwrap(),
                        signature: header("webhook-signature"),
                        body,
                    });
                    status
                },
            )
            .with_state(receiver.clone());
        let addr = common::serve(router).await;
        (receiver, format!("http://{addr}/hooks"))
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn state_changes_are_delivered_as_signed_webhooks(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let (receiver, url) = Receiver::start(StatusCode::OK).await;

    let (status, endpoint) = app.post("/v1/webhook_endpoints", json!({ "url": url })).await;
    assert_eq!(status, StatusCode::CREATED, "{endpoint}");
    let secret = endpoint["secret"].as_str().unwrap().to_owned();

    let invoice_id = app.open_invoice().await;
    let (status, _) = app.pay(&invoice_id, "tok_card_declined", "decline").await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    let (status, _) = app.pay(&invoice_id, "tok_success", "succeed").await;
    assert_eq!(status, StatusCode::OK);

    assert!(receiver.received().is_empty());
    assert_eq!(deliver_due_webhooks(&app.state).await.unwrap(), 4);

    let received = receiver.received();
    let mut types: Vec<String> = received
        .iter()
        .map(|delivery| {
            assert!(
                signing::verify(
                    &secret,
                    &delivery.id,
                    delivery.timestamp,
                    &delivery.body,
                    &delivery.signature,
                    chrono::Utc::now().timestamp(),
                ),
                "signature must verify with the endpoint secret"
            );
            let body: Value = serde_json::from_slice(&delivery.body).unwrap();
            assert_eq!(body["id"], delivery.id.as_str(), "webhook-id is the event id");
            body["type"].as_str().unwrap().to_owned()
        })
        .collect();
    types.sort();
    assert_eq!(
        types,
        [
            "invoice.created",
            "invoice.finalized",
            "invoice.paid",
            "invoice.payment_failed"
        ]
    );

    assert_eq!(deliver_due_webhooks(&app.state).await.unwrap(), 0);

    let (_, events) = app.get("/v1/events?type=invoice.paid").await;
    assert_eq!(events["data"][0]["data"]["invoice"]["id"], invoice_id.as_str());
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn failing_endpoints_are_retried_until_the_budget_runs_out(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let (receiver, url) = Receiver::start(StatusCode::SERVICE_UNAVAILABLE).await;
    let (_, endpoint) = app.post("/v1/webhook_endpoints", json!({ "url": url })).await;
    let endpoint_id = endpoint["id"].as_str().unwrap();

    app.create_customer().await;
    let customer_id = app.create_customer().await;
    app.post(
        "/v1/invoices",
        json!({
            "customer_id": customer_id,
            "due_date": "2099-01-31",
            "line_items": [{ "description": "Thing", "quantity": 1, "unit_amount_cents": 100 }],
        }),
    )
    .await;

    assert_eq!(deliver_due_webhooks(&app.state).await.unwrap(), 1);
    let (_, deliveries) = app
        .get(&format!("/v1/webhook_endpoints/{endpoint_id}/deliveries"))
        .await;
    assert_eq!(deliveries["data"][0]["status"], "pending");
    assert_eq!(deliveries["data"][0]["last_response_status"], 503);

    assert_eq!(deliver_due_webhooks(&app.state).await.unwrap(), 1);
    let (_, deliveries) = app
        .get(&format!("/v1/webhook_endpoints/{endpoint_id}/deliveries"))
        .await;
    assert_eq!(deliveries["data"][0]["status"], "failed");
    assert_eq!(deliveries["data"][0]["attempt_count"], 2);

    assert_eq!(deliver_due_webhooks(&app.state).await.unwrap(), 0);
    let received = receiver.received();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].id, received[1].id, "retries keep the same webhook-id");
}
