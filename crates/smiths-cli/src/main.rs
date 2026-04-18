//! smiths-net binary entry point.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::{Json, Router, routing::get};
use clap::{Parser, ValueEnum};
use smiths_core::{
    AiRegistry, Config, Event, EventBus, LogFormat, MediaFabric, SdpNegotiator, Shutdown,
    SipTransport, SystemEvent,
};
use smiths_mcp::{ControlState, ToolContext};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Which transport to run MCP on. Can be combined with SIP — MCP
/// is additive; SIP / health / A2A all come from `config` as usual.
/// stdin EOF terminates the process.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
enum McpMode {
    /// Run MCP over stdio alongside any other enabled subsystems.
    /// Logs are routed to stderr so stdout stays on the JSON-RPC wire.
    Stdio,
}

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

    /// Run only the MCP server on the chosen transport. Suppresses SIP
    /// bind-up and the HTTP health endpoint so the process behaves as a
    /// clean MCP server for an LLM host.
    #[arg(long, value_enum)]
    mcp: Option<McpMode>,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // wiring of all subsystems belongs in one place
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let config = Config::load(&cli.config)
        .with_context(|| format!("loading config from {}", cli.config.display()))?;

    // stdio MCP must not pollute stdout with logs or framing garbage.
    // Route everything to stderr and shut off pretty/JSON frames.
    let level = cli
        .log
        .as_deref()
        .unwrap_or(&config.observability.log_level);
    init_tracing(level, config.observability.log_format, cli.mcp.is_some())?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config.display(),
        "smiths-net starting"
    );

    let shutdown = Shutdown::new();
    let bus = EventBus::new(1024);

    // Control-plane state is always spawned so MCP/A2A can serve tools
    // with live data. It's cheap and draining is cooperative.
    let (control_state, control_task) = ControlState::spawn(&bus, shutdown.token());

    // Load plugins from the configured directory. Failures are per-
    // plugin and logged; they don't block startup.
    let ai_registry = smiths_plugin::AiRegistry::new();
    match smiths_plugin::load_plugins(&config.plugins.dir, &ai_registry).await {
        Ok(report) => {
            if !report.loaded.is_empty() {
                info!(loaded = ?report.loaded, "plugins ready");
            }
            if !report.failed.is_empty() {
                for (dir, err) in &report.failed {
                    warn!(%dir, %err, "plugin load failed");
                }
            }
        }
        Err(e) => warn!(?e, "plugin scan failed"),
    }

    let registry = Arc::new(smiths_mcp::tools::builtin_registry());
    let ai_registry_dyn: Arc<dyn AiRegistry> = Arc::new(ai_registry.clone());
    let tool_ctx = ToolContext::new(control_state, ai_registry_dyn);

    // MCP stdio is now additive: it runs alongside SIP / health / A2A
    // rather than replacing them, so agents can receive push
    // notifications about calls the engine is serving.
    let mcp_stdio_task: Option<JoinHandle<()>> = if cli.mcp == Some(McpMode::Stdio) {
        let reg = Arc::clone(&registry);
        let ctx = tool_ctx.clone();
        let bus = bus.clone();
        let cancel = shutdown.token();
        Some(tokio::spawn(async move {
            if let Err(e) = smiths_mcp::mcp::run_stdio(reg, ctx, bus, cancel).await {
                warn!(?e, "MCP stdio server error");
            }
        }))
    } else {
        None
    };

    // ---- health HTTP endpoint ----
    let health = tokio::spawn(serve_health(
        config.observability.health_bind,
        shutdown.token(),
    ));

    // ---- SIP subsystem ----
    // One shared media fabric across every bind — media endpoints are
    // handed out by token, not by socket address, so a single fabric
    // serves all signaling transports.
    let media_fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());

    let mut sip_handles: Vec<JoinHandle<()>> = Vec::new();
    let udp_enabled = config.sip.transports.contains(&SipTransport::Udp);
    if !udp_enabled {
        warn!("no UDP SIP transport configured; signaling disabled");
    }
    for bind in &config.sip.bind {
        if !udp_enabled {
            break;
        }
        let addr = bind.socket_addr();
        match spawn_sip_udp(
            addr,
            bus.clone(),
            shutdown.token(),
            Arc::clone(&media_fabric),
        )
        .await
        {
            Ok(handles) => sip_handles.extend(handles),
            Err(e) => warn!(%bind, ?e, "failed to start SIP on bind; continuing"),
        }
    }

    // ---- A2A HTTP adapter (optional) ----
    let mut adapter_handles: Vec<JoinHandle<()>> = Vec::new();
    if config.a2a.enabled {
        let bind = config.a2a.bind;
        let reg = Arc::clone(&registry);
        let ctx = tool_ctx.clone();
        let cancel = shutdown.token();
        adapter_handles.push(tokio::spawn(async move {
            if let Err(e) = smiths_mcp::a2a::serve_http(bind, reg, ctx, cancel).await {
                warn!(%bind, ?e, "A2A HTTP server error");
            }
        }));
    }

    if let Err(err) = bus.publish(Event::System(SystemEvent::Ready)) {
        warn!(?err, "no bus subscribers at startup");
    }
    info!(
        health_bind = %config.observability.health_bind,
        sip_binds = ?config.sip.bind,
        sip_transports = ?config.sip.transports,
        a2a_enabled = config.a2a.enabled,
        "smiths-net ready"
    );

    // Either SIGINT/SIGTERM or MCP-stdio exit (stdin EOF) triggers
    // shutdown. The latter is how an LLM host kills an MCP subprocess.
    if let Some(mcp_task) = mcp_stdio_task {
        tokio::select! {
            r = shutdown.wait_for_signal() => r.context("installing signal handlers")?,
            _ = mcp_task => info!("MCP stdio closed; shutting down"),
        }
    } else {
        shutdown
            .wait_for_signal()
            .await
            .context("installing signal handlers")?;
    }
    shutdown.trigger();
    info!("shutdown signal received; draining");
    let _ = bus.publish(Event::System(SystemEvent::ShutdownRequested));

    for h in sip_handles {
        if let Err(err) = h.await {
            warn!(?err, "SIP task panicked during shutdown");
        }
    }
    for h in adapter_handles {
        if let Err(err) = h.await {
            warn!(?err, "control adapter task panicked during shutdown");
        }
    }
    match health.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(?err, "health server returned error on shutdown"),
        Err(err) => warn!(?err, "health server task panicked"),
    }
    let _ = control_task.await;

    // Drain plugin sidecars.
    ai_registry.shutdown_all().await;

    let _ = bus.publish(Event::System(SystemEvent::ShutdownComplete));
    info!("graceful shutdown complete");
    Ok(())
}

async fn spawn_sip_udp(
    bind: SocketAddr,
    bus: EventBus,
    cancel: CancellationToken,
    media_fabric: Arc<dyn MediaFabric>,
) -> anyhow::Result<Vec<JoinHandle<()>>> {
    let transport = UdpTransport::bind(bind)
        .await
        .with_context(|| format!("binding UDP on {bind}"))?;
    let local = transport.local_addr()?;
    let transport = Arc::new(transport);

    let (tx, rx) = mpsc::channel(1024);
    let reader = transport.spawn_reader(tx, cancel.clone());

    // Negotiator is per-bind so it can publish the correct local IP in
    // `o=` / `c=`. A single-bind deployment has one negotiator; a
    // multi-bind deployment has one per listener.
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, media_fabric, negotiator)
        .with_context(|| format!("building UAS on {local}"))?;
    let server_handle = tokio::spawn(server.run(rx, cancel));
    info!(%local, "SIP UDP listening");
    Ok(vec![reader, server_handle])
}

fn init_tracing(level: &str, format: LogFormat, mcp_stdio: bool) -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .or_else(|_| EnvFilter::try_new("info"))
        .context("constructing tracing EnvFilter")?;

    let registry = tracing_subscriber::registry().with(filter);
    // In MCP stdio mode, stdout is the JSON-RPC wire; divert logs to
    // stderr regardless of the configured format.
    if mcp_stdio {
        registry
            .with(fmt::layer().with_writer(std::io::stderr))
            .init();
        return Ok(());
    }
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
