//! smiths-net binary entry point.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;
use axum::{Json, Router, routing::get};
use clap::Parser;
use smiths_core::{Config, Event, EventBus, LogFormat, Shutdown, SystemEvent};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// CLI flags.
#[derive(Debug, Parser)]
#[command(
    name = "smiths-net",
    version,
    about = "Lightweight AI-first SIP engine"
)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, env = "SMITHS_CONFIG", default_value = "examples/config.toml")]
    config: PathBuf,

    /// Override `observability.log_level` (`RUST_LOG` still takes precedence).
    #[arg(long, env = "SMITHS_LOG")]
    log: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    let level = cli
        .log
        .as_deref()
        .unwrap_or(&config.observability.log_level);
    init_tracing(level, config.observability.log_format)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config.display(),
        "smiths-net starting"
    );

    let shutdown = Shutdown::new();
    let bus = EventBus::new(1024);

    let health = tokio::spawn(serve_health(
        config.observability.health_bind,
        shutdown.token(),
    ));

    // Publish Ready even if there are no subscribers yet — it's a
    // no-op then, not an error worth propagating.
    if let Err(err) = bus.publish(Event::System(SystemEvent::Ready)) {
        warn!(?err, "no bus subscribers at startup (expected in Phase 0)");
    }
    info!(
        health_bind = %config.observability.health_bind,
        "smiths-net ready"
    );

    shutdown
        .wait_for_signal()
        .await
        .context("installing signal handlers")?;
    info!("shutdown signal received; draining");
    let _ = bus.publish(Event::System(SystemEvent::ShutdownRequested));

    match health.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(?err, "health server returned error on shutdown"),
        Err(err) => warn!(?err, "health server task panicked"),
    }

    let _ = bus.publish(Event::System(SystemEvent::ShutdownComplete));
    info!("graceful shutdown complete");
    Ok(())
}

fn init_tracing(level: &str, format: LogFormat) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .or_else(|_| EnvFilter::try_new("info"))
        .context("constructing tracing EnvFilter")?;

    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Json => registry.with(fmt::layer().json()).init(),
        LogFormat::Pretty => registry.with(fmt::layer()).init(),
    }
    Ok(())
}

async fn serve_health(bind: SocketAddr, cancel: CancellationToken) -> anyhow::Result<()> {
    let app = Router::new().route("/health", get(health_handler));
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding health endpoint on {bind}"))?;
    info!(%bind, "health endpoint listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .context("health server")?;
    info!("health endpoint stopped");
    Ok(())
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}
