use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use rand::distr::{Alphanumeric, SampleString};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgExecutor};
use uuid::Uuid;

use crate::AppState;
use crate::error::ApiError;
use crate::extract::{Json, Path};

const KEY_PREFIX: &str = "sk_";
const RANDOM_CHARS: usize = 40;
const DISPLAY_PREFIX_LEN: usize = 11;

#[derive(Debug, Serialize, FromRow)]
pub struct ApiKey {
    pub id: Uuid,
    #[serde(skip)]
    pub business_id: Uuid,
    #[sqlx(rename = "key_prefix")]
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct IssuedApiKey {
    #[serde(flatten)]
    pub api_key: ApiKey,
    pub secret: String,
}

pub fn hash(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

pub async fn issue(db: impl PgExecutor<'_>, business_id: Uuid) -> sqlx::Result<IssuedApiKey> {
    let secret = format!(
        "{KEY_PREFIX}{}",
        Alphanumeric.sample_string(&mut rand::rng(), RANDOM_CHARS)
    );

    let api_key = sqlx::query_as::<_, ApiKey>(
        "INSERT INTO api_keys (id, business_id, key_prefix, key_hash)
         VALUES ($1, $2, $3, $4)
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(&secret[..DISPLAY_PREFIX_LEN])
    .bind(hash(&secret))
    .fetch_one(db)
    .await?;

    Ok(IssuedApiKey { api_key, secret })
}

#[derive(Debug, Clone, Copy)]
pub struct Authenticated {
    pub business_id: Uuid,
}

impl FromRequestParts<AppState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let key = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| ApiError::unauthorized("expected an API key as `Authorization: Bearer sk_...`"))?;

        let business_id: Option<Uuid> =
            sqlx::query_scalar("SELECT business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL")
                .bind(hash(key.trim()))
                .fetch_optional(&state.db)
                .await?;

        let business_id = business_id.ok_or_else(|| ApiError::unauthorized("invalid or revoked API key"))?;
        tracing::Span::current().record("business_id", tracing::field::display(business_id));
        Ok(Self { business_id })
    }
}

pub async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
) -> Result<(StatusCode, Json<IssuedApiKey>), ApiError> {
    let issued = issue(&state.db, auth.business_id).await?;
    tracing::info!(api_key_id = %issued.api_key.id, "API key issued");
    Ok((StatusCode::CREATED, Json(issued)))
}

pub async fn list(State(state): State<AppState>, auth: Authenticated) -> Result<Json<Vec<ApiKey>>, ApiError> {
    let keys = sqlx::query_as::<_, ApiKey>("SELECT * FROM api_keys WHERE business_id = $1 ORDER BY id DESC")
        .bind(auth.business_id)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(keys))
}

pub async fn revoke(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> Result<Json<ApiKey>, ApiError> {
    let key = sqlx::query_as::<_, ApiKey>(
        "UPDATE api_keys
            SET revoked_at = COALESCE(revoked_at, now())
          WHERE id = $1 AND business_id = $2
      RETURNING *",
    )
    .bind(id)
    .bind(auth.business_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| ApiError::not_found("API key"))?;

    tracing::info!(api_key_id = %key.id, "API key revoked");
    Ok(Json(key))
}
