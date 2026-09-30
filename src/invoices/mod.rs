pub mod handlers;
pub mod pricing;
pub mod state;

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgConnection, PgExecutor};
use uuid::Uuid;

pub use state::{InvalidTransition, InvoiceAction, InvoiceStatus};

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Invoice {
    pub id: Uuid,
    #[serde(skip)]
    pub business_id: Uuid,
    pub customer_id: Uuid,
    pub status: InvoiceStatus,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: NaiveDate,
    #[sqlx(skip)]
    pub line_items: Vec<LineItem>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
    pub paid_at: Option<DateTime<Utc>>,
    pub voided_at: Option<DateTime<Utc>>,
    pub marked_uncollectible_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct LineItem {
    pub id: Uuid,
    #[serde(skip)]
    pub invoice_id: Uuid,
    #[serde(skip)]
    pub position: i32,
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

pub async fn find(db: impl PgExecutor<'_>, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Invoice>> {
    sqlx::query_as("SELECT * FROM invoices WHERE id = $1 AND business_id = $2")
        .bind(id)
        .bind(business_id)
        .fetch_optional(db)
        .await
}

pub async fn lock(conn: &mut PgConnection, business_id: Uuid, id: Uuid) -> sqlx::Result<Option<Invoice>> {
    sqlx::query_as("SELECT * FROM invoices WHERE id = $1 AND business_id = $2 FOR UPDATE")
        .bind(id)
        .bind(business_id)
        .fetch_optional(conn)
        .await
}

pub async fn update_status(
    conn: &mut PgConnection,
    invoice: &Invoice,
    next: InvoiceStatus,
) -> sqlx::Result<Invoice> {
    let mut updated: Invoice = sqlx::query_as(
        "UPDATE invoices
            SET status = $3,
                updated_at = now(),
                finalized_at            = CASE WHEN $3 = 'open'          THEN now() ELSE finalized_at END,
                paid_at                 = CASE WHEN $3 = 'paid'          THEN now() ELSE paid_at END,
                voided_at               = CASE WHEN $3 = 'void'          THEN now() ELSE voided_at END,
                marked_uncollectible_at = CASE WHEN $3 = 'uncollectible' THEN now() ELSE marked_uncollectible_at END
          WHERE id = $1 AND status = $2
      RETURNING *",
    )
    .bind(invoice.id)
    .bind(invoice.status)
    .bind(next)
    .fetch_one(&mut *conn)
    .await?;

    updated.line_items = invoice.line_items.clone();
    if updated.line_items.is_empty() {
        attach_line_items(conn, std::slice::from_mut(&mut updated)).await?;
    }
    Ok(updated)
}

pub async fn attach_line_items(db: impl PgExecutor<'_>, invoices: &mut [Invoice]) -> sqlx::Result<()> {
    if invoices.is_empty() {
        return Ok(());
    }

    let ids: Vec<Uuid> = invoices.iter().map(|invoice| invoice.id).collect();
    let items: Vec<LineItem> = sqlx::query_as(
        "SELECT * FROM invoice_line_items WHERE invoice_id = ANY($1) ORDER BY invoice_id, position",
    )
    .bind(&ids)
    .fetch_all(db)
    .await?;

    let mut by_invoice: HashMap<Uuid, Vec<LineItem>> = HashMap::new();
    for item in items {
        by_invoice.entry(item.invoice_id).or_default().push(item);
    }
    for invoice in invoices {
        invoice.line_items = by_invoice.remove(&invoice.id).unwrap_or_default();
    }
    Ok(())
}
