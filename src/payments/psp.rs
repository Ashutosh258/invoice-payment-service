use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::PaymentAttempt;
use crate::config::PaymentsConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PspOutcome {
    Succeeded { psp_ref: String },
    Failed { code: String, message: String },
    Unknown { reason: String },
}

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    charges_url: String,
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct ChargeResponse {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

impl PspClient {
    pub fn new(config: &PaymentsConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(config.psp_timeout)
            .build()?;
        Ok(Self {
            http,
            charges_url: format!("{}/v1/charges", config.psp_base_url.trim_end_matches('/')),
            timeout: config.psp_timeout,
        })
    }

    pub async fn charge(&self, attempt: &PaymentAttempt) -> PspOutcome {
        let started = std::time::Instant::now();
        let outcome = self.send(attempt).await;
        tracing::info!(
            payment_attempt_id = %attempt.id,
            invoice_id = %attempt.invoice_id,
            elapsed_ms = started.elapsed().as_millis() as u64,
            ?outcome,
            "PSP charge finished",
        );
        outcome
    }

    async fn send(&self, attempt: &PaymentAttempt) -> PspOutcome {
        let request = self
            .http
            .post(&self.charges_url)
            .header("idempotency-key", attempt.id.to_string())
            .json(&json!({
                "amount_cents": attempt.amount_cents,
                "currency": attempt.currency,
                "card_token": attempt.card_token,
            }));

        let response = match request.send().await {
            Ok(response) => response,
            // The request never left this machine, so the card was definitely not charged.
            Err(err) if err.is_connect() => {
                return PspOutcome::Failed {
                    code: "psp_unavailable".into(),
                    message: format!("could not connect to the payment processor: {err}"),
                };
            }
            Err(err) if err.is_timeout() => {
                return PspOutcome::Unknown {
                    reason: format!("no response within {:?}", self.timeout),
                };
            }
            Err(err) => {
                return PspOutcome::Unknown {
                    reason: err.to_string(),
                };
            }
        };

        let status = response.status();
        // The PSP may have charged before failing: resolve later by replaying the same key.
        if status.is_server_error()
            || status == StatusCode::CONFLICT
            || status == StatusCode::TOO_MANY_REQUESTS
        {
            return PspOutcome::Unknown {
                reason: format!("payment processor responded with {status}"),
            };
        }

        let body = match response.json::<ChargeResponse>().await {
            Ok(body) => body,
            Err(err) if status.is_success() => {
                return PspOutcome::Unknown {
                    reason: format!("unreadable success response: {err}"),
                };
            }
            Err(_) => {
                return PspOutcome::Failed {
                    code: "processing_error".into(),
                    message: format!("payment processor rejected the request with {status}"),
                };
            }
        };

        match (body.status.as_str(), body.psp_ref) {
            ("succeeded", Some(psp_ref)) => PspOutcome::Succeeded { psp_ref },
            ("failed", _) => {
                let code = body.code.unwrap_or_else(|| "card_declined".into());
                PspOutcome::Failed {
                    message: format!("the payment was declined ({code})"),
                    code,
                }
            }
            (other, _) => PspOutcome::Unknown {
                reason: format!("unexpected charge status {other:?} with HTTP {status}"),
            },
        }
    }
}
