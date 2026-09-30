#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use invoice_service::config::{Config, PaymentsConfig, WebhookConfig};
use invoice_service::{AppState, businesses, router};
use mock_psp::MockPsp;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub struct TestApp {
    pub state: AppState,
    pub psp: MockPsp,
    pub base_url: String,
    pub api_key: String,
    http: reqwest::Client,
}

pub struct Options {
    pub psp_timeout: Duration,
    pub psp_timeout_delay: Duration,
    pub rechecks: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            psp_timeout: Duration::from_secs(5),
            psp_timeout_delay: Duration::from_secs(30),
            rechecks: 2,
        }
    }
}

// sqlx::test pools share a parent capped at 20 connections; the concurrency tests need more.
pub async fn pool(_: PgPoolOptions, connect_opts: PgConnectOptions) -> PgPool {
    PgPoolOptions::new()
        .max_connections(25)
        .connect_with(connect_opts)
        .await
        .expect("connecting to the test database")
}

impl TestApp {
    pub async fn spawn(db: PgPool) -> Self {
        Self::spawn_with(db, Options::default()).await
    }

    pub async fn spawn_with(db: PgPool, options: Options) -> Self {
        let psp = MockPsp::new(mock_psp::Config {
            latency: Duration::from_millis(50),
            timeout_delay: options.psp_timeout_delay,
        });
        let psp_addr = serve(psp.router()).await;

        let config = Config {
            database_url: String::new(),
            listen_addr: ([127, 0, 0, 1], 0).into(),
            payments: PaymentsConfig {
                psp_base_url: format!("http://{psp_addr}"),
                psp_timeout: options.psp_timeout,
                recheck_schedule: vec![Duration::ZERO; options.rechecks],
            },
            webhooks: WebhookConfig {
                request_timeout: Duration::from_secs(2),
                retry_schedule: vec![Duration::ZERO],
            },
            worker_poll_interval: Duration::from_millis(50),
        };
        let state = AppState::new(db, config).expect("building app state");
        let app_addr = serve(router(state.clone())).await;

        let (_, key) = businesses::create_with_api_key(&state.db, "Acme Test Co")
            .await
            .expect("creating test business");

        Self {
            state,
            psp,
            base_url: format!("http://{app_addr}"),
            api_key: key.secret,
            http: reqwest::Client::new(),
        }
    }

    pub async fn other_business_key(&self) -> String {
        let (_, key) = businesses::create_with_api_key(&self.state.db, "Someone Else Inc")
            .await
            .unwrap();
        key.secret
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(self.http.get(self.url(path)).bearer_auth(&self.api_key))
            .await
    }

    pub async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(
            self.http
                .post(self.url(path))
                .bearer_auth(&self.api_key)
                .json(&body),
        )
        .await
    }

    pub async fn pay(
        &self,
        invoice_id: &str,
        card_token: &str,
        idempotency_key: &str,
    ) -> (StatusCode, Value) {
        let request = self
            .http
            .post(self.url(&format!("/v1/invoices/{invoice_id}/pay")))
            .bearer_auth(&self.api_key)
            .header("Idempotency-Key", idempotency_key)
            .json(&json!({ "card_token": card_token }));
        self.send(request).await
    }

    pub async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.expect("request failed");
        let status = response.status();
        let body = response.json().await.unwrap_or(Value::Null);
        (status, body)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub async fn create_customer(&self) -> String {
        let (status, body) = self
            .post(
                "/v1/customers",
                json!({ "name": "Ada Lovelace", "email": "ada@example.com" }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }

    pub async fn open_invoice(&self) -> String {
        let customer_id = self.create_customer().await;
        let (status, body) = self
            .post(
                "/v1/invoices",
                json!({
                    "customer_id": customer_id,
                    "due_date": "2099-01-31",
                    "line_items": [
                        { "description": "Pro plan", "quantity": 1, "unit_amount_cents": 10_000 },
                        { "description": "Extra seats", "quantity": 3, "unit_amount_cents": 782 },
                        { "description": "Setup", "quantity": 1, "unit_amount_cents": 1 },
                    ],
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["total_cents"], 12_347);
        let id = body["id"].as_str().unwrap().to_owned();

        let (status, body) = self.post(&format!("/v1/invoices/{id}/finalize"), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        id
    }

    pub async fn invoice(&self, id: &str) -> Value {
        let (status, body) = self.get(&format!("/v1/invoices/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    pub async fn payment_attempts(&self, invoice_id: &str) -> Vec<Value> {
        let (status, body) = self
            .get(&format!("/v1/invoices/{invoice_id}/payment_attempts"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body.as_array().unwrap().clone()
    }

    pub async fn event_count(&self, event_type: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM events WHERE event_type = $1")
            .bind(event_type)
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}

pub async fn serve(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}
