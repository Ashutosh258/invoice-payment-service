use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::AppState;
use crate::api_keys::Authenticated;
use crate::error::ApiError;
use crate::extract::{Json, Path, Query};
use crate::pagination::{self, Page};

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Customer {
    pub id: Uuid,
    #[serde(skip)]
    pub business_id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCustomer {
    pub name: String,
    pub email: String,
}

impl CreateCustomer {
    fn validate(&self) -> Result<(), ApiError> {
        let name_len = self.name.trim().chars().count();
        if !(1..=200).contains(&name_len) {
            return Err(ApiError::validation("name must be 1 to 200 characters"));
        }
        let email = self.email.trim();
        if email.len() > 254 || email.contains(char::is_whitespace) || !email.contains('@') {
            return Err(ApiError::validation("email must be a valid email address"));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub struct ListCustomers {
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

pub async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(req): Json<CreateCustomer>,
) -> Result<(StatusCode, Json<Customer>), ApiError> {
    req.validate()?;

    let customer = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (id, business_id, name, email)
         VALUES ($1, $2, $3, $4)
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(req.name.trim())
    .bind(req.email.trim())
    .fetch_one(&state.db)
    .await?;

    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> Result<Json<Customer>, ApiError> {
    sqlx::query_as::<_, Customer>("SELECT * FROM customers WHERE id = $1 AND business_id = $2")
        .bind(id)
        .bind(auth.business_id)
        .fetch_optional(&state.db)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("customer"))
}

pub async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(params): Query<ListCustomers>,
) -> Result<Json<Page<Customer>>, ApiError> {
    let limit = pagination::limit(params.limit)?;

    let rows = sqlx::query_as::<_, Customer>(
        "SELECT * FROM customers
          WHERE business_id = $1
            AND ($2::uuid IS NULL OR id < $2)
          ORDER BY id DESC
          LIMIT $3",
    )
    .bind(auth.business_id)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;

    Ok(Json(Page::from_overfetched(rows, limit)))
}
