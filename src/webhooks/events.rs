use chrono::Utc;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    InvoiceCreated,
    InvoiceFinalized,
    InvoicePaid,
    InvoicePaymentFailed,
    InvoiceVoided,
    InvoiceMarkedUncollectible,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvoiceCreated => "invoice.created",
            Self::InvoiceFinalized => "invoice.finalized",
            Self::InvoicePaid => "invoice.paid",
            Self::InvoicePaymentFailed => "invoice.payment_failed",
            Self::InvoiceVoided => "invoice.voided",
            Self::InvoiceMarkedUncollectible => "invoice.marked_uncollectible",
        }
    }
}

// Must run in the same transaction as the state change it describes (outbox).
pub async fn record(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: EventType,
    data: Value,
) -> sqlx::Result<Uuid> {
    let event_id = Uuid::now_v7();
    let payload = json!({
        "id": event_id,
        "type": event_type.as_str(),
        "created_at": Utc::now(),
        "data": data,
    });

    sqlx::query("INSERT INTO events (id, business_id, event_type, payload) VALUES ($1, $2, $3, $4)")
        .bind(event_id)
        .bind(business_id)
        .bind(event_type.as_str())
        .bind(&payload)
        .execute(&mut *conn)
        .await?;

    let endpoint_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM webhook_endpoints WHERE business_id = $1 AND disabled_at IS NULL")
            .bind(business_id)
            .fetch_all(&mut *conn)
            .await?;

    if !endpoint_ids.is_empty() {
        let delivery_ids: Vec<Uuid> = endpoint_ids.iter().map(|_| Uuid::now_v7()).collect();
        sqlx::query(
            "INSERT INTO webhook_deliveries (id, event_id, endpoint_id)
             SELECT delivery_id, $2, endpoint_id FROM UNNEST($1::uuid[], $3::uuid[]) AS t(delivery_id, endpoint_id)",
        )
        .bind(&delivery_ids)
        .bind(event_id)
        .bind(&endpoint_ids)
        .execute(&mut *conn)
        .await?;
    }

    tracing::debug!(%event_id, event_type = event_type.as_str(), deliveries = endpoint_ids.len(), "event recorded");
    Ok(event_id)
}
