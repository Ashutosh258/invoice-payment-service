use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Config {
    pub latency: Duration,
    pub timeout_delay: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            latency: Duration::from_millis(100),
            timeout_delay: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
pub struct MockPsp {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    requests: Mutex<HashMap<String, KeyState>>,
    charge_requests: AtomicU64,
    captured_charges: AtomicU64,
}

enum KeyState {
    InFlight,
    Done(StoredResponse),
}

#[derive(Clone)]
struct StoredResponse {
    status: StatusCode,
    body: Value,
}

impl IntoResponse for StoredResponse {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[derive(Debug, Deserialize)]
struct ChargeRequest {
    amount_cents: i64,
    currency: String,
    card_token: String,
}

impl MockPsp {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                requests: Mutex::new(HashMap::new()),
                charge_requests: AtomicU64::new(0),
                captured_charges: AtomicU64::new(0),
            }),
        }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/v1/charges", post(create_charge))
            .with_state(self.clone())
    }

    pub fn charge_requests(&self) -> u64 {
        self.inner.charge_requests.load(Ordering::SeqCst)
    }

    pub fn captured_charges(&self) -> u64 {
        self.inner.captured_charges.load(Ordering::SeqCst)
    }
}

async fn create_charge(
    State(psp): State<MockPsp>,
    headers: HeaderMap,
    Json(request): Json<ChargeRequest>,
) -> Response {
    psp.inner.charge_requests.fetch_add(1, Ordering::SeqCst);

    let Some(key) = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return failed(StatusCode::BAD_REQUEST, "missing_idempotency_key").into_response();
    };

    if request.amount_cents <= 0 || request.currency != "usd" {
        return failed(StatusCode::BAD_REQUEST, "invalid_amount").into_response();
    }

    {
        let mut requests = psp.inner.requests.lock().unwrap();
        match requests.get(&key) {
            Some(KeyState::Done(stored)) => {
                tracing::info!(%key, "replaying stored result");
                return stored.clone().into_response();
            }
            Some(KeyState::InFlight) => {
                tracing::info!(%key, "request with this key is still in flight");
                return failed(StatusCode::CONFLICT, "request_in_progress").into_response();
            }
            None => {
                requests.insert(key.clone(), KeyState::InFlight);
            }
        }
    }

    // Detached so a caller that times out does not cancel the charge, like a real PSP.
    let task = tokio::spawn({
        let psp = psp.clone();
        let key = key.clone();
        async move {
            let result = execute(&psp, &request).await;
            let mut requests = psp.inner.requests.lock().unwrap();
            match &result {
                None => requests.remove(&key),
                Some(stored) => requests.insert(key, KeyState::Done(stored.clone())),
            };
            result
        }
    });

    match task.await {
        Ok(Some(stored)) => stored.into_response(),
        Ok(None) | Err(_) => failed(StatusCode::INTERNAL_SERVER_ERROR, "internal_error").into_response(),
    }
}

async fn execute(psp: &MockPsp, request: &ChargeRequest) -> Option<StoredResponse> {
    let config = &psp.inner.config;

    match request.card_token.as_str() {
        "tok_success" => {
            tokio::time::sleep(config.latency).await;
            Some(succeeded(psp))
        }
        "tok_timeout" => {
            tokio::time::sleep(config.timeout_delay).await;
            Some(succeeded(psp))
        }
        "tok_insufficient_funds" => {
            tokio::time::sleep(config.latency).await;
            Some(failed(StatusCode::PAYMENT_REQUIRED, "insufficient_funds"))
        }
        "tok_card_declined" => {
            tokio::time::sleep(config.latency).await;
            Some(failed(StatusCode::PAYMENT_REQUIRED, "card_declined"))
        }
        "tok_network_error" => {
            tracing::warn!("simulating a PSP outage");
            None
        }
        _ => Some(failed(StatusCode::BAD_REQUEST, "invalid_card_token")),
    }
}

fn succeeded(psp: &MockPsp) -> StoredResponse {
    psp.inner.captured_charges.fetch_add(1, Ordering::SeqCst);
    StoredResponse {
        status: StatusCode::OK,
        body: json!({ "status": "succeeded", "psp_ref": Uuid::new_v4() }),
    }
}

fn failed(status: StatusCode, code: &str) -> StoredResponse {
    StoredResponse {
        status,
        body: json!({ "status": "failed", "code": code }),
    }
}
