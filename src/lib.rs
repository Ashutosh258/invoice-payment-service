pub mod api_keys;
pub mod businesses;
pub mod config;
pub mod customers;
pub mod error;
pub mod extract;
pub mod invoices;
pub mod pagination;
pub mod payments;
pub mod webhooks;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::{delete, get, post};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::watch;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Config;
use crate::payments::psp::PspClient;

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub psp: PspClient,
    pub webhook_http: reqwest::Client,
}

impl AppState {
    pub fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let psp = PspClient::new(&config.payments)?;
        let webhook_http = reqwest::Client::builder()
            .timeout(config.webhooks.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("invoice-service-webhooks/1")
            .build()?;

        Ok(Self {
            db,
            config: Arc::new(config),
            psp,
            webhook_http,
        })
    }
}

pub async fn connect_db(url: &str) -> anyhow::Result<PgPool> {
    let mut attempt = 1;
    loop {
        match PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(err) if attempt < 10 => {
                tracing::warn!(attempt, error = %err, "database not reachable yet; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                attempt += 1;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

pub fn router(state: AppState) -> Router {
    use crate::{customers, invoices, payments, webhooks};

    let v1 = Router::new()
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/{id}", get(customers::get))
        .route(
            "/invoices",
            post(invoices::handlers::create).get(invoices::handlers::list),
        )
        .route("/invoices/{id}", get(invoices::handlers::get))
        .route("/invoices/{id}/finalize", post(invoices::handlers::finalize))
        .route("/invoices/{id}/void", post(invoices::handlers::void))
        .route(
            "/invoices/{id}/mark_uncollectible",
            post(invoices::handlers::mark_uncollectible),
        )
        .route("/invoices/{id}/pay", post(payments::handlers::pay))
        .route(
            "/invoices/{id}/payment_attempts",
            get(payments::handlers::list_for_invoice),
        )
        .route(
            "/webhook_endpoints",
            post(webhooks::handlers::create_endpoint).get(webhooks::handlers::list_endpoints),
        )
        .route(
            "/webhook_endpoints/{id}",
            delete(webhooks::handlers::disable_endpoint),
        )
        .route(
            "/webhook_endpoints/{id}/deliveries",
            get(webhooks::handlers::list_deliveries),
        )
        .route("/events", get(webhooks::handlers::list_events))
        .route("/api_keys", post(api_keys::create).get(api_keys::list))
        .route("/api_keys/{id}", delete(api_keys::revoke));

    let trace = TraceLayer::new_for_http()
        .make_span_with(|request: &Request<_>| {
            let request_id = request
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("-");
            tracing::info_span!(
                "request",
                method = %request.method(),
                path = %request.uri().path(),
                request_id,
                business_id = tracing::field::Empty,
            )
        })
        .on_response(DefaultOnResponse::new().level(Level::INFO));

    Router::new()
        .nest("/v1", v1)
        .route("/health", get(health))
        .with_state(state)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(trace)
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

async fn health(State(state): State<AppState>) -> StatusCode {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

pub async fn run_worker<F, Fut>(
    name: &'static str,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
    mut tick: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<usize>>,
{
    tracing::info!(worker = name, "worker started");
    while !*shutdown.borrow() {
        let found_work = match tick().await {
            Ok(processed) => processed > 0,
            Err(err) => {
                tracing::error!(worker = name, error = %err, "worker tick failed");
                false
            }
        };
        if found_work {
            continue;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {}
        }
    }
    tracing::info!(worker = name, "worker stopped");
}
