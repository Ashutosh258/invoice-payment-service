use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::{AttemptStatus, PaymentAttempt};
use crate::AppState;
use crate::api_keys::Authenticated;
use crate::error::ApiError;
use crate::extract::{Json, Path};
use crate::invoices::{self, InvoiceAction, InvoiceStatus};

const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const MAX_IDEMPOTENCY_KEY_LEN: usize = 255;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayInvoice {
    pub card_token: String,
}

pub async fn pay(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<PayInvoice>,
) -> Result<Response, ApiError> {
    let key = idempotency_key(&headers)?;
    let card_token = req.card_token.trim();
    if card_token.is_empty() || card_token.len() > 255 {
        return Err(ApiError::validation("card_token must be 1 to 255 characters"));
    }

    if let Some(existing) = super::find_by_idempotency_key(&state.db, auth.business_id, &key).await? {
        return replay(existing, invoice_id, card_token);
    }

    let attempt = match begin(&state, auth.business_id, invoice_id, &key, card_token).await? {
        Begun::Created(attempt) => attempt,
        Begun::KeyAlreadyUsed => {
            let existing = super::find_by_idempotency_key(&state.db, auth.business_id, &key)
                .await?
                .ok_or_else(|| ApiError::internal("idempotency key vanished after a unique violation"))?;
            return replay(existing, invoice_id, card_token);
        }
    };

    // No transaction is held across the PSP call; the pending attempt blocks other payments.
    let outcome = state.psp.charge(&attempt).await;

    let attempt = super::settle(&state, attempt.id, outcome).await?;
    Ok(render(&attempt))
}

#[allow(
    clippy::large_enum_variant,
    reason = "built once per request and moved straight out"
)]
enum Begun {
    Created(PaymentAttempt),
    KeyAlreadyUsed,
}

async fn begin(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    key: &str,
    card_token: &str,
) -> Result<Begun, ApiError> {
    let mut tx = state.db.begin().await?;

    let invoice = invoices::lock(&mut tx, business_id, invoice_id)
        .await?
        .ok_or_else(|| ApiError::not_found("invoice"))?;

    // Re-check under the lock: a concurrent request with this key may have just committed.
    if super::find_by_idempotency_key(&mut *tx, business_id, key)
        .await?
        .is_some()
    {
        return Ok(Begun::KeyAlreadyUsed);
    }

    if invoice.status == InvoiceStatus::Paid {
        return Err(ApiError::conflict(
            "invoice_already_paid",
            "this invoice has already been paid",
        ));
    }
    if let Err(err) = invoice.status.apply(InvoiceAction::RecordPayment) {
        return Err(ApiError::conflict("invoice_not_payable", err.to_string()));
    }
    if let Some(pending_id) = super::pending_attempt_for(&mut tx, invoice.id).await? {
        return Err(payment_in_progress(pending_id));
    }

    let payments = &state.config.payments;
    let first_recheck = payments.psp_timeout + payments.recheck_schedule.first().copied().unwrap_or_default();

    let inserted = sqlx::query_as::<_, PaymentAttempt>(
        "INSERT INTO payment_attempts
                (id, business_id, invoice_id, status, amount_cents, card_token, idempotency_key, next_recheck_at)
         VALUES ($1, $2, $3, 'pending', $4, $5, $6, now() + $7 * interval '1 millisecond')
         RETURNING *",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(invoice.id)
    .bind(invoice.total_cents)
    .bind(card_token)
    .bind(key)
    .bind(first_recheck.as_millis() as i64)
    .fetch_one(&mut *tx)
    .await;

    let attempt = match inserted {
        Ok(attempt) => attempt,
        Err(sqlx::Error::Database(err)) if err.constraint() == Some("payment_attempts_idempotency_key") => {
            return Ok(Begun::KeyAlreadyUsed);
        }
        Err(sqlx::Error::Database(err))
            if err.constraint() == Some("payment_attempts_one_pending_per_invoice") =>
        {
            return Err(ApiError::conflict(
                "payment_in_progress",
                "another payment for this invoice is in progress",
            ));
        }
        Err(err) => return Err(err.into()),
    };

    tx.commit().await?;
    tracing::info!(payment_attempt_id = %attempt.id, %invoice_id, amount_cents = attempt.amount_cents, "payment attempt started");
    Ok(Begun::Created(attempt))
}

fn replay(existing: PaymentAttempt, invoice_id: Uuid, card_token: &str) -> Result<Response, ApiError> {
    if existing.invoice_id != invoice_id || existing.card_token != card_token {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "idempotency_key_reused",
            "this Idempotency-Key was already used for a different request",
        ));
    }

    let mut response = render(&existing);
    response
        .headers_mut()
        .insert("idempotent-replayed", HeaderValue::from_static("true"));
    Ok(response)
}

fn render(attempt: &PaymentAttempt) -> Response {
    match attempt.status {
        AttemptStatus::Succeeded => (StatusCode::OK, Json(attempt)).into_response(),
        AttemptStatus::Pending => (StatusCode::ACCEPTED, Json(attempt)).into_response(),
        AttemptStatus::Failed => ApiError::new(
            StatusCode::PAYMENT_REQUIRED,
            "payment_failed",
            attempt
                .failure_message
                .clone()
                .unwrap_or_else(|| "the payment failed".into()),
        )
        .with_details(json!({ "payment_attempt": attempt }))
        .into_response(),
    }
}

fn payment_in_progress(attempt_id: Uuid) -> ApiError {
    ApiError::conflict(
        "payment_in_progress",
        "another payment for this invoice is in progress; wait for its outcome before trying again",
    )
    .with_details(json!({ "payment_attempt_id": attempt_id }))
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .ok_or_else(|| ApiError::bad_request("an Idempotency-Key header is required to pay an invoice"))?;
    let key = value
        .to_str()
        .map_err(|_| ApiError::bad_request("Idempotency-Key must be printable ASCII"))?
        .trim();
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(ApiError::bad_request(format!(
            "Idempotency-Key must be 1 to {MAX_IDEMPOTENCY_KEY_LEN} characters"
        )));
    }
    Ok(key.to_owned())
}

pub async fn list_for_invoice(
    State(state): State<AppState>,
    auth: Authenticated,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<Vec<PaymentAttempt>>, ApiError> {
    invoices::find(&state.db, auth.business_id, invoice_id)
        .await?
        .ok_or_else(|| ApiError::not_found("invoice"))?;

    let attempts = sqlx::query_as("SELECT * FROM payment_attempts WHERE invoice_id = $1 ORDER BY id")
        .bind(invoice_id)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(attempts))
}
