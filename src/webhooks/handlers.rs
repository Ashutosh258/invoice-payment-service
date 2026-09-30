use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

use super::signing;
use crate::AppState;
use crate::api_keys::Authenticated;
use crate::error::ApiError;
use crate::extract::{Json, Path, Query};
use crate::pagination::{self, Page};

#[derive(Debug, Serialize, FromRow)]
pub struct WebhookEndpoint {
    pub id: Uuid,
    pub url: String,
    #[serde(skip)]
    pub secret: String,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct RegisteredEndpoint {
    #[serde(flatten)]
    pub endpoint: WebhookEndpoint,
    pub secret: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateEndpoint {
    pub url: String,
}

#[derive(Debug, Serialize, FromRow)]
pub struct Delivery {
    pub id: Uuid,
    pub event_id: Uuid,
    pub event_type: String,
    pub status: String,
    pub attempt_count: i32,
    pub last_response_status: Option<i32>,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

pub async fn create_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(req): Json<CreateEndpoint>,
) -> Result<(StatusCode, Json<RegisteredEndpoint>), ApiError> {
    let url = reqwest::Url::parse(req.url.trim())
        .map_err(|err| ApiError::validation(format!("url is not a valid URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(ApiError::validation("url must be an absolute http(s) URL"));
    }

    let secret = signing::generate_secret();
    let endpoint: WebhookEndpoint = sqlx::query_as(
        "INSERT INTO webhook_endpoints (id, business_id, url, secret)
         VALUES ($1, $2, $3, $4)
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(url.as_str())
    .bind(&secret)
    .fetch_one(&state.db)
    .await?;

    tracing::info!(endpoint_id = %endpoint.id, url = %endpoint.url, "webhook endpoint registered");
    Ok((StatusCode::CREATED, Json(RegisteredEndpoint { endpoint, secret })))
}

pub async fn list_endpoints(
    State(state): State<AppState>,
    auth: Authenticated,
) -> Result<Json<Vec<WebhookEndpoint>>, ApiError> {
    let endpoints = sqlx::query_as("SELECT * FROM webhook_endpoints WHERE business_id = $1 ORDER BY id DESC")
        .bind(auth.business_id)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(endpoints))
}

pub async fn disable_endpoint(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> Result<Json<WebhookEndpoint>, ApiError> {
    let mut tx = state.db.begin().await?;

    let endpoint: WebhookEndpoint = sqlx::query_as(
        "UPDATE webhook_endpoints
            SET disabled_at = COALESCE(disabled_at, now())
          WHERE id = $1 AND business_id = $2
      RETURNING *",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("webhook endpoint"))?;

    sqlx::query(
        "UPDATE webhook_deliveries SET status = 'canceled', updated_at = now()
          WHERE endpoint_id = $1 AND status = 'pending'",
    )
    .bind(endpoint.id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Json(endpoint))
}

pub async fn list_deliveries(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(endpoint_id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<Delivery>>, ApiError> {
    let limit = pagination::limit(params.limit)?;

    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM webhook_endpoints WHERE id = $1 AND business_id = $2)",
    )
    .bind(endpoint_id)
    .bind(auth.business_id)
    .fetch_one(&state.db)
    .await?;
    if !owned {
        return Err(ApiError::not_found("webhook endpoint"));
    }

    let rows: Vec<Delivery> = sqlx::query_as(
        "SELECT d.id, d.event_id, e.event_type, d.status, d.attempt_count, d.last_response_status,
                d.last_error, CASE WHEN d.status = 'pending' THEN d.next_attempt_at END AS next_attempt_at,
                d.delivered_at, d.created_at
           FROM webhook_deliveries d
           JOIN events e ON e.id = d.event_id
          WHERE d.endpoint_id = $1
            AND ($2::text IS NULL OR e.event_type = $2)
            AND ($3::uuid IS NULL OR d.id < $3)
          ORDER BY d.id DESC
          LIMIT $4",
    )
    .bind(endpoint_id)
    .bind(params.event_type)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page::from_overfetched(rows, limit)))
}

pub async fn list_events(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(params): Query<ListParams>,
) -> Result<Json<Page<Value>>, ApiError> {
    let limit = pagination::limit(params.limit)?;

    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT payload FROM events
          WHERE business_id = $1
            AND ($2::text IS NULL OR event_type = $2)
            AND ($3::uuid IS NULL OR id < $3)
          ORDER BY id DESC
          LIMIT $4",
    )
    .bind(auth.business_id)
    .bind(params.event_type)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page::from_overfetched(rows, limit)))
}
