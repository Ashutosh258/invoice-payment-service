mod common;

use common::TestApp;
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn the_server_computes_totals_and_refuses_client_totals(
    opts: PgPoolOptions,
    connect: PgConnectOptions,
) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let customer_id = app.create_customer().await;

    let (status, body) = app
        .post(
            "/v1/invoices",
            json!({
                "customer_id": customer_id,
                "due_date": "2099-01-31",
                "total_cents": 1,
                "line_items": [{ "description": "Pro plan", "quantity": 1, "unit_amount_cents": 10_000 }],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("total_cents"));

    let (status, body) = app
        .post(
            "/v1/invoices",
            json!({
                "customer_id": customer_id,
                "due_date": "2099-01-31",
                "line_items": [{ "description": "Pro plan", "quantity": 1, "unit_amount_cents": 99.99 }],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = app
        .post(
            "/v1/invoices",
            json!({
                "customer_id": customer_id,
                "due_date": "2099-01-31",
                "line_items": [
                    { "description": "Seats", "quantity": 7, "unit_amount_cents": 1_299 },
                    { "description": "Support", "quantity": 1, "unit_amount_cents": 5_000 },
                ],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], "draft");
    assert_eq!(body["total_cents"], 7 * 1_299 + 5_000);
    assert_eq!(body["line_items"][0]["amount_cents"], 9_093);
    assert_eq!(body["line_items"][1]["description"], "Support");
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn invalid_transitions_are_rejected_with_a_clear_error(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id = app.open_invoice().await;

    let (status, body) = app
        .post(&format!("/v1/invoices/{invoice_id}/finalize"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "invalid_state_transition");
    assert_eq!(
        body["error"]["message"],
        "cannot finalize an invoice that is open"
    );

    let (status, body) = app
        .post(
            &format!("/v1/invoices/{invoice_id}/mark_uncollectible"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "uncollectible");
    let (status, _) = app.pay(&invoice_id, "tok_success", "late").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = app
        .post(&format!("/v1/invoices/{invoice_id}/void"), json!({}))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["message"], "cannot void an invoice that is paid");

    let customer_id = app.create_customer().await;
    let (_, draft) = app
        .post(
            "/v1/invoices",
            json!({
                "customer_id": customer_id,
                "due_date": "2099-01-31",
                "line_items": [{ "description": "Thing", "quantity": 1, "unit_amount_cents": 500 }],
            }),
        )
        .await;
    let draft_id = draft["id"].as_str().unwrap();
    let (status, body) = app.pay(draft_id, "tok_success", "too-early").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "invoice_not_payable");

    let (status, _) = app
        .post(&format!("/v1/invoices/{draft_id}/void"), json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = app.pay(draft_id, "tok_success", "too-late").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["message"], "cannot pay an invoice that is void");
    assert_eq!(app.psp.charge_requests(), 1);
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn lists_filter_by_status_and_paginate(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let paid = app.open_invoice().await;
    app.pay(&paid, "tok_success", "p").await;
    let open_a = app.open_invoice().await;
    let open_b = app.open_invoice().await;

    let (status, body) = app.get("/v1/invoices?status=open").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [open_b.as_str(), open_a.as_str()], "newest first");

    let (_, page1) = app.get("/v1/invoices?limit=2").await;
    assert_eq!(page1["has_more"], true);
    let last = page1["data"][1]["id"].as_str().unwrap();
    let (_, page2) = app
        .get(&format!("/v1/invoices?limit=2&starting_after={last}"))
        .await;
    assert_eq!(page2["has_more"], false);
    assert_eq!(page2["data"][0]["id"], paid.as_str());

    let (status, body) = app.get("/v1/invoices?status=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_request");
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn businesses_cannot_see_or_touch_each_others_data(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let invoice_id = app.open_invoice().await;
    let customer_id = app.invoice(&invoice_id).await["customer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let intruder = app.other_business_key().await;
    let http = reqwest::Client::new();

    for path in [
        format!("/v1/invoices/{invoice_id}"),
        format!("/v1/customers/{customer_id}"),
    ] {
        let response = http
            .get(app.url(&path))
            .bearer_auth(&intruder)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }

    let response = http
        .post(app.url(&format!("/v1/invoices/{invoice_id}/pay")))
        .bearer_auth(&intruder)
        .header("Idempotency-Key", "x")
        .json(&json!({ "card_token": "tok_success" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = http
        .post(app.url("/v1/invoices"))
        .bearer_auth(&intruder)
        .json(&json!({
            "customer_id": customer_id,
            "due_date": "2099-01-31",
            "line_items": [{ "description": "Sneaky", "quantity": 1, "unit_amount_cents": 100 }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(app.psp.charge_requests(), 0);
}

#[sqlx::test(migrator = "invoice_service::MIGRATOR")]
async fn revoked_keys_stop_working_immediately(opts: PgPoolOptions, connect: PgConnectOptions) {
    let app = TestApp::spawn(common::pool(opts, connect).await).await;
    let http = reqwest::Client::new();

    let (status, new_key) = app.post("/v1/api_keys", json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
    let secret = new_key["secret"].as_str().unwrap();
    assert!(secret.starts_with(new_key["prefix"].as_str().unwrap()));

    let with_new_key = || http.get(app.url("/v1/customers")).bearer_auth(secret).send();
    assert_eq!(with_new_key().await.unwrap().status(), StatusCode::OK);

    let (status, _) = app
        .send(
            http.delete(app.url(&format!("/v1/api_keys/{}", new_key["id"].as_str().unwrap())))
                .bearer_auth(&app.api_key),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let response = with_new_key().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");

    let (status, _) = app.get("/v1/customers").await;
    assert_eq!(status, StatusCode::OK);
}
