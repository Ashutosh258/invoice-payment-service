use std::time::Duration;

use tokio::task::JoinSet;

use super::PaymentAttempt;
use crate::AppState;

const BATCH_SIZE: i64 = 20;
const LEASE_SLACK: Duration = Duration::from_secs(10);

pub async fn recheck_due_payments(state: &AppState) -> anyhow::Result<usize> {
    let lease = state.config.payments.psp_timeout + LEASE_SLACK;

    // Claiming pushes next_recheck_at past a lease; SKIP LOCKED lets instances share the work.
    let due: Vec<PaymentAttempt> = sqlx::query_as(
        "UPDATE payment_attempts
            SET recheck_count = recheck_count + 1,
                next_recheck_at = now() + $2 * interval '1 millisecond',
                updated_at = now()
          WHERE id IN (SELECT id FROM payment_attempts
                        WHERE status = 'pending' AND next_recheck_at <= now()
                        ORDER BY next_recheck_at
                        LIMIT $1
                          FOR UPDATE SKIP LOCKED)
      RETURNING *",
    )
    .bind(BATCH_SIZE)
    .bind(lease.as_millis() as i64)
    .fetch_all(&state.db)
    .await?;

    let checked = due.len();
    let mut checks = JoinSet::new();
    for attempt in due {
        let state = state.clone();
        checks.spawn(async move {
            tracing::info!(payment_attempt_id = %attempt.id, recheck = attempt.recheck_count, "re-checking payment");
            let outcome = state.psp.charge(&attempt).await;
            super::settle(&state, attempt.id, outcome).await
        });
    }
    while let Some(result) = checks.join_next().await {
        match result {
            Ok(Ok(attempt)) => {
                tracing::info!(payment_attempt_id = %attempt.id, status = ?attempt.status, "re-check done")
            }
            Ok(Err(err)) => tracing::error!(error = %err, "failed to settle re-checked payment"),
            Err(err) => tracing::error!(error = %err, "payment re-check task panicked"),
        }
    }

    Ok(checked)
}
