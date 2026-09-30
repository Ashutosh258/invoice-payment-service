pub mod handlers;
pub mod psp;
pub mod reconciler;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

use crate::AppState;
use crate::invoices::{self, Invoice, InvoiceAction};
use crate::webhooks::events::{self, EventType};
use psp::PspOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum AttemptStatus {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    #[serde(skip)]
    pub business_id: Uuid,
    pub invoice_id: Uuid,
    pub status: AttemptStatus,
    pub amount_cents: i64,
    pub currency: String,
    #[serde(skip)]
    pub card_token: String,
    pub idempotency_key: String,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub failure_message: Option<String>,
    #[serde(skip)]
    pub recheck_count: i32,
    #[serde(skip)]
    pub next_recheck_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub async fn find_by_idempotency_key(
    db: impl PgExecutor<'_>,
    business_id: Uuid,
    key: &str,
) -> sqlx::Result<Option<PaymentAttempt>> {
    sqlx::query_as("SELECT * FROM payment_attempts WHERE business_id = $1 AND idempotency_key = $2")
        .bind(business_id)
        .bind(key)
        .fetch_optional(db)
        .await
}

pub async fn pending_attempt_for(conn: &mut PgConnection, invoice_id: Uuid) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar("SELECT id FROM payment_attempts WHERE invoice_id = $1 AND status = 'pending'")
        .bind(invoice_id)
        .fetch_optional(conn)
        .await
}

pub async fn settle(state: &AppState, attempt_id: Uuid, outcome: PspOutcome) -> sqlx::Result<PaymentAttempt> {
    // Lock order is always invoice, then attempt, to avoid deadlocks with begin/void.
    let mut tx = state.db.begin().await?;

    let invoice: Invoice = sqlx::query_as(
        "SELECT i.* FROM invoices i
           JOIN payment_attempts pa ON pa.invoice_id = i.id
          WHERE pa.id = $1
            FOR UPDATE OF i",
    )
    .bind(attempt_id)
    .fetch_one(&mut *tx)
    .await?;
    let attempt: PaymentAttempt = sqlx::query_as("SELECT * FROM payment_attempts WHERE id = $1 FOR UPDATE")
        .bind(attempt_id)
        .fetch_one(&mut *tx)
        .await?;

    if attempt.status != AttemptStatus::Pending {
        return Ok(attempt);
    }

    let attempt = match outcome {
        PspOutcome::Succeeded { psp_ref } => record_success(&mut tx, invoice, &attempt, &psp_ref).await?,
        PspOutcome::Failed { code, message } => {
            record_failure(&mut tx, invoice, &attempt, &code, &message).await?
        }
        PspOutcome::Unknown { reason } => {
            let schedule = &state.config.payments.recheck_schedule;
            match schedule.get(attempt.recheck_count as usize) {
                Some(delay) => {
                    tracing::warn!(
                        payment_attempt_id = %attempt.id,
                        rechecks_so_far = attempt.recheck_count,
                        ?delay,
                        %reason,
                        "payment outcome unknown; will re-check",
                    );
                    sqlx::query_as(
                        "UPDATE payment_attempts
                            SET next_recheck_at = now() + $2 * interval '1 millisecond', updated_at = now()
                          WHERE id = $1
                      RETURNING *",
                    )
                    .bind(attempt.id)
                    .bind(delay.as_millis() as i64)
                    .fetch_one(&mut *tx)
                    .await?
                }
                // Every re-check replayed the same idempotency key, so a PSP that had charged
                // would have returned the success by now. See DESIGN.md §3(b).
                None => {
                    tracing::error!(
                        payment_attempt_id = %attempt.id,
                        rechecks = attempt.recheck_count,
                        %reason,
                        "giving up on payment after exhausting re-checks; flag for settlement reconciliation",
                    );
                    let message = format!(
                        "no definitive answer from the payment processor after {} re-checks: {reason}",
                        attempt.recheck_count
                    );
                    record_failure(&mut tx, invoice, &attempt, "psp_unavailable", &message).await?
                }
            }
        }
    };

    tx.commit().await?;
    Ok(attempt)
}

async fn record_success(
    conn: &mut PgConnection,
    invoice: Invoice,
    attempt: &PaymentAttempt,
    psp_ref: &str,
) -> sqlx::Result<PaymentAttempt> {
    let attempt: PaymentAttempt = sqlx::query_as(
        "UPDATE payment_attempts
            SET status = 'succeeded', psp_ref = $2, next_recheck_at = NULL,
                completed_at = now(), updated_at = now()
          WHERE id = $1
      RETURNING *",
    )
    .bind(attempt.id)
    .bind(psp_ref)
    .fetch_one(&mut *conn)
    .await?;

    match invoice.status.apply(InvoiceAction::RecordPayment) {
        Ok(next) => {
            let invoice = invoices::update_status(conn, &invoice, next).await?;
            events::record(
                conn,
                invoice.business_id,
                EventType::InvoicePaid,
                json!({ "invoice": invoice, "payment_attempt": attempt }),
            )
            .await?;
            tracing::info!(invoice_id = %invoice.id, payment_attempt_id = %attempt.id, "invoice paid");
        }
        // Unreachable while void/uncollectible are blocked during a pending attempt.
        Err(err) => {
            tracing::error!(
                invoice_id = %invoice.id,
                payment_attempt_id = %attempt.id,
                %err,
                "PSP captured a payment the invoice cannot accept; needs a manual refund",
            );
        }
    }

    Ok(attempt)
}

async fn record_failure(
    conn: &mut PgConnection,
    mut invoice: Invoice,
    attempt: &PaymentAttempt,
    code: &str,
    message: &str,
) -> sqlx::Result<PaymentAttempt> {
    let attempt: PaymentAttempt = sqlx::query_as(
        "UPDATE payment_attempts
            SET status = 'failed', failure_code = $2, failure_message = $3, next_recheck_at = NULL,
                completed_at = now(), updated_at = now()
          WHERE id = $1
      RETURNING *",
    )
    .bind(attempt.id)
    .bind(code)
    .bind(message)
    .fetch_one(&mut *conn)
    .await?;

    invoices::attach_line_items(&mut *conn, std::slice::from_mut(&mut invoice)).await?;
    events::record(
        conn,
        invoice.business_id,
        EventType::InvoicePaymentFailed,
        json!({ "invoice": invoice, "payment_attempt": attempt }),
    )
    .await?;
    tracing::info!(invoice_id = %invoice.id, payment_attempt_id = %attempt.id, code, "payment failed");

    Ok(attempt)
}
