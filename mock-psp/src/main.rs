use std::time::Duration;

use anyhow::Context;
use mock_psp::{Config, MockPsp};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let addr = std::env::var("MOCK_PSP_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".into());
    let mut config = Config::default();
    if let Some(ms) = env_millis("MOCK_PSP_LATENCY_MS")? {
        config.latency = ms;
    }
    if let Some(ms) = env_millis("MOCK_PSP_TIMEOUT_DELAY_MS")? {
        config.timeout_delay = ms;
    }

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, ?config, "mock PSP listening");

    axum::serve(listener, MockPsp::new(config).router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn env_millis(name: &str) -> anyhow::Result<Option<Duration>> {
    match std::env::var(name) {
        Ok(value) => {
            let ms = value
                .parse()
                .with_context(|| format!("{name} must be an integer"))?;
            Ok(Some(Duration::from_millis(ms)))
        }
        Err(_) => Ok(None),
    }
}
