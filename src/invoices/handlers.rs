use axum::extract::State;
use axum::http::StatusCode;
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::pricing::{self, NewLineItem};
use super::{Invoice, InvoiceAction, InvoiceStatus, LineItem};
use crate::AppState;
use crate::api_keys::Authenticated;
use crate::error::ApiError;
use crate::extract::{Json, Path, Query};
use crate::pagination::{self, Page};
use crate::payments;
use crate::webhooks::events::{self, EventType};

#[derive(Debug, Deserialize)]
// Rejects a client-supplied total_cents instead of silently ignoring it.
#[serde(deny_unknown_fields)]
pub struct CreateInvoice {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<NewLineItem>,
}

#[derive(Debug, Deserialize)]
pub struct ListInvoices {
    pub status: Option<InvoiceStatus>,
    pub customer_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub starting_after: Option<Uuid>,
}

pub async fn create(
    State(state): State<AppState>,
    auth: Authenticated,
    Json(req): Json<CreateInvoice>,
) -> Result<(StatusCode, Json<Invoice>), ApiError> {
    let priced = pricing::price(req.line_items)?;
    if req.due_date < Utc::now().date_naive() {
        return Err(ApiError::validation("due_date cannot be in the past"));
    }

    let mut tx = state.db.begin().await?;

    let customer_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)")
            .bind(req.customer_id)
            .bind(auth.business_id)
            .fetch_one(&mut *tx)
            .await?;
    if !customer_exists {
        return Err(ApiError::validation(
            "customer_id does not refer to one of your customers",
        ));
    }

    let mut invoice: Invoice = sqlx::query_as(
        "INSERT INTO invoices (id, business_id, customer_id, status, total_cents, due_date)
         VALUES ($1, $2, $3, 'draft', $4, $5)
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(auth.business_id)
    .bind(req.customer_id)
    .bind(priced.total_cents)
    .bind(req.due_date)
    .fetch_one(&mut *tx)
    .await?;

    let items = &priced.line_items;
    let mut line_items: Vec<LineItem> = sqlx::query_as(
        "INSERT INTO invoice_line_items
                (id, invoice_id, position, description, quantity, unit_amount_cents, amount_cents)
         SELECT id, $2, position, description, quantity, unit_amount_cents, amount_cents
           FROM UNNEST($1::uuid[], $3::int[], $4::text[], $5::bigint[], $6::bigint[], $7::bigint[])
             AS t(id, position, description, quantity, unit_amount_cents, amount_cents)
         RETURNING *",
    )
    .bind(items.iter().map(|_| Uuid::now_v7()).collect::<Vec<_>>())
    .bind(invoice.id)
    .bind((0..items.len() as i32).collect::<Vec<_>>())
    .bind(items.iter().map(|li| li.description.clone()).collect::<Vec<_>>())
    .bind(items.iter().map(|li| li.quantity).collect::<Vec<_>>())
    .bind(items.iter().map(|li| li.unit_amount_cents).collect::<Vec<_>>())
    .bind(items.iter().map(|li| li.amount_cents).collect::<Vec<_>>())
    .fetch_all(&mut *tx)
    .await?;
    line_items.sort_by_key(|li| li.position);
    invoice.line_items = line_items;

    events::record(
        &mut tx,
        auth.business_id,
        EventType::InvoiceCreated,
        json!({ "invoice": invoice }),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(invoice_id = %invoice.id, total_cents = invoice.total_cents, "invoice created");
    Ok((StatusCode::CREATED, Json(invoice)))
}

pub async fn get(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
) -> Result<Json<Invoice>, ApiError> {
    let mut invoice = super::find(&state.db, auth.business_id, id)
        .await?
        .ok_or_else(|| ApiError::not_found("invoice"))?;
    super::attach_line_items(&state.db, std::slice::from_mut(&mut invoice)).await?;
    Ok(Json(invoice))
}

pub async fn list(
    State(state): State<AppState>,
    auth: Authenticated,
    Query(params): Query<ListInvoices>,
) -> Result<Json<Page<Invoice>>, ApiError> {
    let limit = pagination::limit(params.limit)?;

    let mut invoices: Vec<Invoice> = sqlx::query_as(
        "SELECT * FROM invoices
          WHERE business_id = $1
            AND ($2::text IS NULL OR status = $2)
            AND ($3::uuid IS NULL OR customer_id = $3)
            AND ($4::uuid IS NULL OR id < $4)
          ORDER BY id DESC
          LIMIT $5",
    )
    .bind(auth.business_id)
    .bind(params.status)
    .bind(params.customer_id)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    super::attach_line_items(&state.db, &mut invoices).await?;

    Ok(Json(Page::from_overfetched(invoices, limit)))
}

pub async fn finalize(
    state: State<AppState>,
    auth: Authenticated,
    id: Path<Uuid>,
) -> Result<Json<Invoice>, ApiError> {
    transition(
        state,
        auth,
        id,
        InvoiceAction::Finalize,
        EventType::InvoiceFinalized,
    )
    .await
}

pub async fn void(
    state: State<AppState>,
    auth: Authenticated,
    id: Path<Uuid>,
) -> Result<Json<Invoice>, ApiError> {
    transition(state, auth, id, InvoiceAction::Void, EventType::InvoiceVoided).await
}

pub async fn mark_uncollectible(
    state: State<AppState>,
    auth: Authenticated,
    id: Path<Uuid>,
) -> Result<Json<Invoice>, ApiError> {
    transition(
        state,
        auth,
        id,
        InvoiceAction::MarkUncollectible,
        EventType::InvoiceMarkedUncollectible,
    )
    .await
}

async fn transition(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(id): Path<Uuid>,
    action: InvoiceAction,
    event_type: EventType,
) -> Result<Json<Invoice>, ApiError> {
    let mut tx = state.db.begin().await?;

    let invoice = super::lock(&mut tx, auth.business_id, id)
        .await?
        .ok_or_else(|| ApiError::not_found("invoice"))?;

    let next = invoice
        .status
        .apply(action)
        .map_err(|err| ApiError::conflict("invalid_state_transition", err.to_string()))?;

    // A pending payment may still succeed, so the invoice cannot be voided underneath it.
    if let Some(attempt_id) = payments::pending_attempt_for(&mut tx, invoice.id).await? {
        return Err(ApiError::conflict(
            "payment_in_progress",
            format!("cannot {action} the invoice while a payment is in progress"),
        )
        .with_details(json!({ "payment_attempt_id": attempt_id })));
    }

    let invoice = super::update_status(&mut tx, &invoice, next).await?;
    events::record(
        &mut tx,
        auth.business_id,
        event_type,
        json!({ "invoice": invoice }),
    )
    .await?;
    tx.commit().await?;

    tracing::info!(invoice_id = %invoice.id, status = %invoice.status, "invoice transitioned");
    Ok(Json(invoice))
}
