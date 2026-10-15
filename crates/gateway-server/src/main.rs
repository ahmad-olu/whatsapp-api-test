use std::sync::Arc;

use anyhow::{Context, Result};
use gateway_server::config::Config;
use gateway_server::db::Db;
use gateway_server::transport::GraphTransport;
use gateway_server::Gateway;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Local convenience; on Railway real env vars are used.
    let _ = dotenvy_lite();

    // No ANSI colors unless attached to a terminal (Railway's log viewer shows raw escapes).
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let config = Config::from_env()?;
    let port = config.port;

    tracing::info!(
        sink = ?config.sink,
        graph_version = %config.graph_version,
        signature_check = !config.insecure_skip_signature,
        access_token_set = config.access_token.is_some(),
        default_phone_number_id = ?config.default_phone_number_id,
        "starting whatsapp-gateway"
    );
    if config.insecure_skip_signature {
        tracing::warn!("INSECURE_SKIP_SIGNATURE is on: webhook signatures are NOT verified");
    }

    let db = Db::connect(&config.database_url).await?;
    let transport = Arc::new(GraphTransport::new(
        config.graph_base_url.clone(),
        config.graph_version.clone(),
        config.access_token.clone(),
        config.media_max_bytes,
    ));

    let gateway = Gateway::start(config, db, transport).await?;
    let app = gateway.router();

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("binding port {port}"))?;
    tracing::info!(port, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("shutting down workers");
    gateway.shutdown().await;
    Ok(())
}

/// Minimal `.env` loader (KEY=VALUE lines) so we need no extra dependency.
/// Existing environment variables always win.
fn dotenvy_lite() -> std::io::Result<()> {
    let text = std::fs::read_to_string(".env")?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if std::env::var_os(k).is_none() {
                // SAFETY: called at startup before any other threads read the environment.
                unsafe { std::env::set_var(k, v) };
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
