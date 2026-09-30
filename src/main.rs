use anyhow::Context;
use invoice_service::config::Config;
use invoice_service::payments::reconciler;
use invoice_service::webhooks::dispatcher;
use invoice_service::{AppState, MIGRATOR, businesses, connect_db, router, run_worker};
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: invoice-service [serve | create-business <name>]";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,tower_http=info".into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = Config::from_env()?;

    match args.first().map(String::as_str) {
        None | Some("serve") => serve(config).await,
        Some("create-business") => {
            let name = args.get(1).context(USAGE)?;
            create_business(config, name).await
        }
        Some(other) => anyhow::bail!("unknown command {other:?}\n{USAGE}"),
    }
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let db = connect_db(&config.database_url).await?;
    MIGRATOR.run(&db).await.context("running migrations")?;

    let listen_addr = config.listen_addr;
    let poll_interval = config.worker_poll_interval;
    let state = AppState::new(db, config)?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let workers = [
        tokio::spawn(run_worker(
            "webhook-dispatcher",
            poll_interval,
            shutdown_rx.clone(),
            {
                let state = state.clone();
                move || {
                    let state = state.clone();
                    async move { dispatcher::deliver_due_webhooks(&state).await }
                }
            },
        )),
        tokio::spawn(run_worker("payment-reconciler", poll_interval, shutdown_rx, {
            let state = state.clone();
            move || {
                let state = state.clone();
                async move { reconciler::recheck_due_payments(&state).await }
            }
        })),
    ];

    let listener = tokio::net::TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("binding {listen_addr}"))?;
    tracing::info!(%listen_addr, "invoice service listening");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("HTTP server stopped; waiting for workers to finish their current tick");
    let _ = shutdown_tx.send(true);
    for worker in workers {
        let _ = worker.await;
    }
    Ok(())
}

async fn create_business(config: Config, name: &str) -> anyhow::Result<()> {
    let db = connect_db(&config.database_url).await?;
    MIGRATOR.run(&db).await.context("running migrations")?;

    let (business_id, key) = businesses::create_with_api_key(&db, name).await?;
    println!("business_id: {business_id}");
    println!("api_key:     {}", key.secret);
    println!();
    println!("The API key is shown only once. Store it somewhere safe.");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
