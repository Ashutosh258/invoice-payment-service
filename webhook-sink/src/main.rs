use axum::Router;
use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, Uri};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let addr = std::env::var("WEBHOOK_SINK_ADDR").unwrap_or_else(|_| "0.0.0.0:9100".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "webhook sink listening");

    axum::serve(listener, Router::new().fallback(receive))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn receive(uri: Uri, headers: HeaderMap, body: Bytes) -> StatusCode {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-")
            .to_owned()
    };
    let status = if uri.path().starts_with("/fail") {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };

    let event_type = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|event| event["type"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".into());

    tracing::info!(
        event_type,
        path = uri.path(),
        webhook_id = header("webhook-id"),
        webhook_timestamp = header("webhook-timestamp"),
        webhook_signature = header("webhook-signature"),
        responding_with = status.as_u16(),
        body = %String::from_utf8_lossy(&body),
        "webhook received",
    );
    status
}
