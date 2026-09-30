use std::time::Duration;

use chrono::Utc;
use rand::Rng;
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use sqlx::FromRow;
use tokio::task::JoinSet;
use uuid::Uuid;

use super::signing;
use crate::AppState;

const BATCH_SIZE: i64 = 50;
const LEASE: Duration = Duration::from_secs(60);

#[derive(Debug, FromRow)]
struct DueDelivery {
    id: Uuid,
    attempt_count: i32,
    event_id: Uuid,
    event_type: String,
    payload: Value,
    url: String,
    secret: String,
}

pub async fn deliver_due_webhooks(state: &AppState) -> anyhow::Result<usize> {
    let due: Vec<DueDelivery> = sqlx::query_as(
        "WITH claimed AS (
             UPDATE webhook_deliveries
                SET next_attempt_at = now() + $2 * interval '1 millisecond', updated_at = now()
              WHERE id IN (SELECT id FROM webhook_deliveries
                            WHERE status = 'pending' AND next_attempt_at <= now()
                            ORDER BY next_attempt_at
                            LIMIT $1
                              FOR UPDATE SKIP LOCKED)
          RETURNING id, event_id, endpoint_id, attempt_count
         )
         SELECT c.id, c.attempt_count, e.id AS event_id, e.event_type, e.payload, w.url, w.secret
           FROM claimed c
           JOIN events e ON e.id = c.event_id
           JOIN webhook_endpoints w ON w.id = c.endpoint_id",
    )
    .bind(BATCH_SIZE)
    .bind(LEASE.as_millis() as i64)
    .fetch_all(&state.db)
    .await?;

    let attempted = due.len();
    let mut deliveries = JoinSet::new();
    for delivery in due {
        let state = state.clone();
        deliveries.spawn(async move { deliver(&state, delivery).await });
    }
    while let Some(result) = deliveries.join_next().await {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::error!(error = %err, "failed to record webhook delivery result"),
            Err(err) => tracing::error!(error = %err, "webhook delivery task panicked"),
        }
    }
    Ok(attempted)
}

async fn deliver(state: &AppState, delivery: DueDelivery) -> anyhow::Result<()> {
    let attempt = delivery.attempt_count + 1;
    let body = serde_json::to_vec(&delivery.payload)?;
    let msg_id = delivery.event_id.to_string();
    let timestamp = Utc::now().timestamp();
    let signature = signing::sign(&delivery.secret, &msg_id, timestamp, &body)?;

    let result = state
        .webhook_http
        .post(&delivery.url)
        .header(CONTENT_TYPE, "application/json")
        .header("webhook-id", &msg_id)
        .header("webhook-timestamp", timestamp.to_string())
        .header("webhook-signature", signature)
        .body(body)
        .send()
        .await;

    let (response_status, error) = match result {
        Ok(response) if response.status().is_success() => (Some(response.status().as_u16()), None),
        Ok(response) => (
            Some(response.status().as_u16()),
            Some(format!("endpoint responded with {}", response.status())),
        ),
        Err(err) => (None, Some(err.without_url().to_string())),
    };
    let response_status = response_status.map(i32::from);

    let Some(error) = error else {
        sqlx::query(
            "UPDATE webhook_deliveries
                SET status = 'succeeded', attempt_count = $2, last_response_status = $3, last_error = NULL,
                    delivered_at = now(), updated_at = now()
              WHERE id = $1",
        )
        .bind(delivery.id)
        .bind(attempt)
        .bind(response_status)
        .execute(&state.db)
        .await?;
        tracing::info!(delivery_id = %delivery.id, event_type = delivery.event_type, attempt, "webhook delivered");
        return Ok(());
    };

    match retry_delay(&state.config.webhooks.retry_schedule, attempt) {
        Some(delay) => {
            sqlx::query(
                "UPDATE webhook_deliveries
                    SET attempt_count = $2, last_response_status = $3, last_error = $4,
                        next_attempt_at = now() + $5 * interval '1 millisecond', updated_at = now()
                  WHERE id = $1",
            )
            .bind(delivery.id)
            .bind(attempt)
            .bind(response_status)
            .bind(&error)
            .bind(delay.as_millis() as i64)
            .execute(&state.db)
            .await?;
            tracing::warn!(delivery_id = %delivery.id, attempt, ?delay, %error, "webhook delivery failed; will retry");
        }
        None => {
            sqlx::query(
                "UPDATE webhook_deliveries
                    SET status = 'failed', attempt_count = $2, last_response_status = $3, last_error = $4,
                        updated_at = now()
                  WHERE id = $1",
            )
            .bind(delivery.id)
            .bind(attempt)
            .bind(response_status)
            .bind(&error)
            .execute(&state.db)
            .await?;
            tracing::error!(delivery_id = %delivery.id, attempt, %error, "webhook delivery exhausted its retry budget");
        }
    }
    Ok(())
}

fn retry_delay(schedule: &[Duration], attempts_made: i32) -> Option<Duration> {
    let base = *schedule.get(usize::try_from(attempts_made).ok()?.checked_sub(1)?)?;
    Some(base.mul_f64(rand::rng().random_range(0.9..=1.1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_follows_the_schedule_then_stops() {
        let schedule = [Duration::from_secs(10), Duration::from_secs(100)];

        let first = retry_delay(&schedule, 1).unwrap();
        assert!((9_000..=11_000).contains(&first.as_millis()));
        let second = retry_delay(&schedule, 2).unwrap();
        assert!((90_000..=110_000).contains(&second.as_millis()));
        assert_eq!(retry_delay(&schedule, 3), None);
    }
}
