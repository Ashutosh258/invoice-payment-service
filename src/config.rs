use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: SocketAddr,
    pub payments: PaymentsConfig,
    pub webhooks: WebhookConfig,
    pub worker_poll_interval: Duration,
}

#[derive(Debug, Clone)]
pub struct PaymentsConfig {
    pub psp_base_url: String,
    pub psp_timeout: Duration,
    pub recheck_schedule: Vec<Duration>,
}

#[derive(Debug, Clone)]
pub struct WebhookConfig {
    pub request_timeout: Duration,
    pub retry_schedule: Vec<Duration>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            listen_addr: env_or("LISTEN_ADDR", "0.0.0.0:8080".parse()?)?,
            payments: PaymentsConfig {
                psp_base_url: env_or("PSP_BASE_URL", "http://localhost:9090".to_owned())?,
                psp_timeout: Duration::from_millis(env_or("PSP_TIMEOUT_MS", 5_000)?),
                recheck_schedule: DEFAULT_RECHECK_SCHEDULE
                    .iter()
                    .copied()
                    .map(Duration::from_secs)
                    .collect(),
            },
            webhooks: WebhookConfig {
                request_timeout: Duration::from_millis(env_or("WEBHOOK_TIMEOUT_MS", 10_000)?),
                retry_schedule: DEFAULT_WEBHOOK_RETRY_SCHEDULE
                    .iter()
                    .copied()
                    .map(Duration::from_secs)
                    .collect(),
            },
            worker_poll_interval: Duration::from_millis(env_or("WORKER_POLL_INTERVAL_MS", 1_000)?),
        })
    }
}

const DEFAULT_RECHECK_SCHEDULE: &[u64] = &[10, 30, 60, 120, 300, 600];

const DEFAULT_WEBHOOK_RETRY_SCHEDULE: &[u64] = &[5, 300, 1_800, 7_200, 18_000, 36_000, 36_000];

fn env_or<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(raw) => raw
            .parse()
            .with_context(|| format!("invalid value for {name}: {raw:?}")),
        Err(_) => Ok(default),
    }
}
