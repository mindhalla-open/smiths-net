//! smiths-net binary entry point.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::{Json, Router, routing::get};
use clap::{Parser, ValueEnum};
use prometheus_client::registry::Registry;
use smiths_core::call::CallOriginator;
use smiths_core::{
    AiRegistry, Config, Event, EventBus, LogFormat, MediaFabric, Metrics, SdpNegotiator, Shutdown,
    SipTransport, SystemEvent,
};
use smiths_mcp::{ControlState, ToolContext};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{
    ResponseRouter, TcpTransport, TlsTransport, Transport as _, UacClient, UasServer, UdpTransport,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
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

/// Build a protocol / feature advertisement for `--version` output
/// (slice 4.3 / P17 / Small 2). Operators can run `smiths-net
/// --version` and tell at a glance what transports + features the
/// binary was compiled with. Assembled via `#[cfg]`-toggled `const`
/// fragments + `concat!` so there's zero runtime cost and the
/// string lives as `'static` for clap's API.
#[cfg(feature = "wireguard")]
const WG_HINT: &str = " + wireguard (scaffold)";
#[cfg(not(feature = "wireguard"))]
const WG_HINT: &str = "";

#[cfg(feature = "sip-quic")]
const QUIC_HINT: &str = ", quic (scaffold)";
#[cfg(not(feature = "sip-quic"))]
const QUIC_HINT: &str = "";

#[cfg(feature = "mcp-http3")]
const H3_HINT: &str = ", h3 (scaffold)";
#[cfg(not(feature = "mcp-http3"))]
const H3_HINT: &str = "";

const LONG_VERSION: &str = const_format::concatcp!(
    env!("CARGO_PKG_VERSION"),
    "\n",
    "sip transports: udp, tcp, tls",
    WG_HINT,
    ", proxy: socks5 + http-connect",
    QUIC_HINT,
    "\n",
    "mcp adapters: stdio, http/1.1, http/2",
    H3_HINT,
    "\n",
    "storage: cdr+kv (sqlite), vector (memory), recording (fs), auth (sqlite+http)",
    "\n",
    "ai: dispatcher, ollama/openai/anthropic refs, whisper.cpp ref, piper ref",
    "\n",
    "plugin tiers: sidecar, wasm, script (rhai)"
);

/// CLI flags.
#[derive(Debug, Parser)]
#[command(
    name = "smiths-net",
    version,
    long_version = LONG_VERSION,
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

    /// HA snapshot file path (slice 6.1). When set, the engine
    /// reads this file at startup and restores every dialog
    /// record in it; on graceful shutdown, the live dialog table
    /// is serialized back to the same path. `None` = no HA
    /// persistence (cold boot every time).
    #[arg(long, env = "SMITHS_SNAPSHOT")]
    snapshot_path: Option<PathBuf>,
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

    // Graceful-drain flag — UAS reads it on every INVITE to decide
    // whether to admit new dialogs. Flipped by the shutdown driver
    // before the cancel token fires.
    let drain = smiths_core::Drain::new();

    // Per-source-IP rate limiter for SIP ingress. Shared across all
    // listeners (UDP / TCP / TLS) so a single noisy peer can't bypass
    // the limit by hopping transports.
    let sip_rate_limit = smiths_sip::SipRateLimiter::new(config.sip.rate_limit);

    // Optional env-driven credential seed. `SMITHS_TEST_CREDS=
    // user:realm:pass[,user:realm:pass...]` populates an in-memory
    // registrar so sipp / dev traffic can exercise the digest auth
    // round-trip. Disabled by default — production credential stores
    // land via the CredentialStore trait (database, LDAP, …) rather
    // than through this env knob.
    let registrar = build_test_registrar();

    // One Prometheus registry, shared between the /metrics endpoint
    // and every subsystem that increments counters.
    let metrics_registry = Arc::new(Mutex::new(Registry::default()));
    let metrics = {
        let mut guard = metrics_registry.lock().await;
        Metrics::register(&mut guard)
    };

    // Control-plane state is always spawned so MCP/A2A can serve tools
    // with live data. It's cheap and draining is cooperative.
    let (control_state, control_task) = ControlState::spawn(&bus, shutdown.token());

    // Build the shared media fabric before the WASM engine so guest
    // `send_rtp` can push packets through it.
    let media_fabric: Arc<dyn MediaFabric> =
        Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(&metrics)));

    // Load plugins from the configured directory. Failures are per-
    // plugin and logged; they don't block startup.
    let ai_registry = smiths_plugin::AiRegistry::new();
    // Build a shared WASM engine so `type = "wasm"` manifests can load.
    // Failing this shouldn't block sidecar plugins — log and proceed.
    let wasm_engine = match smiths_plugin::wasm::WasmEngine::new() {
        Ok(e) => Some(
            e.with_bus(bus.clone())
                .with_media(Arc::new(control_state.clone()), Arc::clone(&media_fabric)),
        ),
        Err(err) => {
            warn!(
                ?err,
                "wasm engine init failed; wasm plugins will be skipped"
            );
            None
        }
    };
    let loader_opts = smiths_plugin::LoaderOpts {
        bus: Some(bus.clone()),
        wasm_engine,
        metrics: Some(Arc::clone(&metrics)),
        sandbox: config.plugins.sandbox.clone(),
    };
    let mut plugins_loaded: Vec<String> = Vec::new();
    let mut plugins_failed: Vec<(String, String)> = Vec::new();
    match smiths_plugin::load_plugins(&config.plugins.dir, &ai_registry, loader_opts).await {
        Ok(report) => {
            if !report.loaded.is_empty() {
                info!(loaded = ?report.loaded, "plugins ready");
            }
            if !report.failed.is_empty() {
                for (dir, err) in &report.failed {
                    warn!(%dir, %err, "plugin load failed");
                }
            }
            plugins_loaded = report.loaded;
            plugins_failed = report.failed;
        }
        Err(e) => warn!(?e, "plugin scan failed"),
    }

    let registry = Arc::new(smiths_mcp::tools::builtin_registry());
    let resources = Arc::new(smiths_mcp::builtin_resources());
    let rate_limiter = Arc::new(smiths_mcp::RateLimiter::new(&config.mcp.rate_limit));
    let ai_registry_dyn: Arc<dyn AiRegistry> = Arc::new(ai_registry.clone());
    let config_snapshot = Arc::new(config.clone());

    // Shared response correlator. The UAS forwards responses to it;
    // the UAC subscribes by branch.
    let response_router = Arc::new(ResponseRouter::new());

    // ---- SIP subsystem ----
    let mut sip_handles: Vec<JoinHandle<()>> = Vec::new();
    // Collected for `/health`. We push the intended `proto://addr`
    // string for each spawn attempt; the health payload reflects the
    // operator's intent even when a particular bind fails (the failure
    // is already in the startup warn! log).
    let mut sip_binds_report: Vec<String> = Vec::new();
    // Slice 6.1: HA dialog snapshot. Read the file once before
    // any SIP bind so the first UAS restores from it; the handle
    // we capture is used on shutdown to serialize back.
    let (mut initial_dialogs, snapshot_sink): (
        Vec<smiths_core::DialogRecord>,
        Option<std::path::PathBuf>,
    ) = if let Some(path) = cli.snapshot_path.clone() {
        match smiths_sip::read_snapshot(&path) {
            Ok(Some(records)) => {
                info!(
                    path = %path.display(),
                    count = records.len(),
                    "HA snapshot loaded; dialogs will be replayed on first UDP bind"
                );
                (records, Some(path))
            }
            Ok(None) => {
                info!(path = %path.display(), "HA snapshot file absent; cold boot");
                (Vec::new(), Some(path))
            }
            Err(e) => {
                warn!(path = %path.display(), ?e, "HA snapshot unreadable; cold boot");
                (Vec::new(), Some(path))
            }
        }
    } else {
        (Vec::new(), None)
    };
    let mut sip_dialogs_for_snapshot: Option<
        Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    > = None;
    let udp_enabled = config.sip.transports.contains(&SipTransport::Udp);
    let tcp_enabled = config.sip.transports.contains(&SipTransport::Tcp);
    let tls_enabled = config.sip.transports.contains(&SipTransport::Tls);
    let quic_requested = config.sip.transports.contains(&SipTransport::Quic);
    if !udp_enabled && !tcp_enabled && !tls_enabled {
        warn!("no SIP transports configured; signaling disabled");
    }
    if tls_enabled && (config.sip.tls_cert_path.is_none() || config.sip.tls_key_path.is_none()) {
        warn!(
            "sip.transports includes `tls` but tls_cert_path/tls_key_path are unset; disabling TLS"
        );
    }
    // Slice 4.3 / P17: SIP-over-QUIC. Config surface + feature
    // flag land now; runtime listener is a dedicated follow-on.
    // Honest behaviour: warn loudly when the operator opts in so
    // "nothing happens" is never mistaken for "it just works".
    if quic_requested {
        if cfg!(feature = "sip-quic") {
            warn!(
                "sip.transports includes `quic`: runtime listener not yet wired. \
                 Config accepted; no QUIC port will open in 0.45.0."
            );
        } else {
            warn!(
                "sip.transports includes `quic` but binary built without \
                 --features sip-quic; ignoring."
            );
        }
    }

    // Build the UAC from the first configured UDP bind. The UAC shares
    // that bind's UdpTransport + ResponseRouter with the UAS, so
    // outbound INVITEs / BYEs get their responses on the same socket.
    let mut originator: Option<Arc<dyn CallOriginator>> = None;
    if udp_enabled && let Some(bind) = config.sip.bind.first() {
        let addr = bind.socket_addr();
        sip_binds_report.push(format!("udp://{addr}"));
        match spawn_sip_udp(
            addr,
            bus.clone(),
            shutdown.token(),
            Arc::clone(&media_fabric),
            Arc::clone(&metrics),
            Arc::clone(&response_router),
            drain.clone(),
            sip_rate_limit.clone(),
            registrar.clone(),
            /* build_uac */ true,
            std::mem::take(&mut initial_dialogs),
        )
        .await
        {
            Ok(SpawnedSipUdp {
                handles,
                uac,
                dialogs,
            }) => {
                sip_handles.extend(handles);
                originator = uac.map(|u| u as Arc<dyn CallOriginator>);
                // The first UDP bind owns the snapshot — multi-bind
                // deployments still get one coherent file.
                if sip_dialogs_for_snapshot.is_none() {
                    sip_dialogs_for_snapshot = Some(dialogs);
                }
            }
            Err(e) => warn!(%bind, ?e, "failed to start SIP/UDP on first bind; continuing"),
        }
    }

    let mut tool_ctx = ToolContext::new(
        control_state,
        ai_registry_dyn,
        config_snapshot.clone(),
        Arc::clone(&media_fabric),
    )
    .with_metrics(Arc::clone(&metrics));
    if let Some(o) = originator.clone() {
        tool_ctx = tool_ctx.with_originator(o);
    }

    // Slice 3.4 storage wiring. Today the CLI supports `memory` for
    // vectors and `fs` for recordings natively; `sidecar` variants
    // resolve at a follow-on slice when the adapter lands.
    match config_snapshot.storage.vector.backend {
        smiths_core::VectorBackend::Memory => {
            let store: Arc<dyn smiths_core::VectorStore> =
                Arc::new(smiths_core::MemoryVectorStore::new());
            tool_ctx = tool_ctx.with_vector(store);
            tracing::info!("vector store: in-memory");
        }
        smiths_core::VectorBackend::None => {}
        smiths_core::VectorBackend::Sidecar => {
            tracing::warn!(
                plugin = ?config_snapshot.storage.vector.plugin,
                "storage.vector = sidecar: adapter not yet wired; \
                 search_calls_semantic will return NotFound"
            );
        }
    }
    match config_snapshot.storage.recording.backend {
        smiths_core::RecordingBackend::Fs => {
            let root = &config_snapshot.storage.recording.fs.root;
            match smiths_core::FsRecordingStore::new(root) {
                Ok(store) => {
                    let handle: Arc<dyn smiths_core::RecordingStore> = Arc::new(store);
                    tool_ctx = tool_ctx.with_recording(Arc::clone(&handle));
                    tracing::info!(root = %root.display(), "recording store: filesystem");
                    spawn_recording_retention_sweeper(
                        Arc::clone(&handle),
                        config_snapshot.storage.recording.retention_days,
                        shutdown.token(),
                    );
                }
                Err(e) => tracing::warn!(
                    root = %root.display(), ?e,
                    "recording store: failed to initialize; continuing without"
                ),
            }
        }
        smiths_core::RecordingBackend::None => {}
        smiths_core::RecordingBackend::Sidecar => {
            tracing::warn!(
                plugin = ?config_snapshot.storage.recording.plugin,
                "storage.recording = sidecar: adapter not yet wired; \
                 pipeline tools still accept inline `audio_base64`"
            );
        }
    }

    // Slice 4.2: IVR prompt library. When the operator sets a
    // non-empty root, wire the library so `record_prompt` writes
    // there + caches decoded WAVs.
    if !config_snapshot.media.prompts.root.is_empty() {
        let mut library =
            smiths_media::PromptLibrary::with_root(&config_snapshot.media.prompts.root);
        if config_snapshot.media.prompts.capacity > 0 {
            library = library.with_capacity(config_snapshot.media.prompts.capacity);
        }
        tool_ctx = tool_ctx.with_prompts(library);
        tracing::info!(
            root = %config_snapshot.media.prompts.root,
            "IVR prompt library wired"
        );
    }

    // Slice 3.5: embedded WireGuard lives behind the `wireguard`
    // Cargo feature. 0.42.0 ships the config surface only; the
    // runtime device (boringtun + tun/tap) lands in a follow-on.
    // Warn clearly when operators opt in so the "nothing happens"
    // isn't mistaken for "everything works".
    match config_snapshot.sip.vpn.mode {
        smiths_core::VpnMode::None => {}
        smiths_core::VpnMode::Wireguard => {
            if cfg!(feature = "wireguard") {
                tracing::warn!(
                    "sip.vpn.mode = wireguard: runtime device not yet wired; \
                     config accepted but no tunnel will come up. \
                     See docs/deployment/vpn.md for the host-sidecar alternative."
                );
            } else {
                tracing::warn!(
                    "sip.vpn.mode = wireguard but binary built without \
                     --features wireguard; falling back to mode=none."
                );
            }
        }
    }

    // MCP stdio is now additive: it runs alongside SIP / health / A2A
    // rather than replacing them, so agents can receive push
    // notifications about calls the engine is serving.
    let mcp_stdio_task: Option<JoinHandle<()>> = if cli.mcp == Some(McpMode::Stdio) {
        let reg = Arc::clone(&registry);
        let res = Arc::clone(&resources);
        let rl = Arc::clone(&rate_limiter);
        let met = Arc::clone(&metrics);
        let ctx = tool_ctx.clone();
        let bus = bus.clone();
        let cancel = shutdown.token();
        Some(tokio::spawn(async move {
            if let Err(e) = smiths_mcp::mcp::run_stdio(reg, res, rl, met, ctx, bus, cancel).await {
                warn!(?e, "MCP stdio server error");
            }
        }))
    } else {
        None
    };

    // Additional SIP binds. The first UDP bind (when UDP is enabled)
    // was already consumed above to stand up the UAC; other binds
    // come online here as UAS-only listeners.
    for (idx, bind) in config.sip.bind.iter().enumerate() {
        let addr = bind.socket_addr();
        if udp_enabled && !(idx == 0 && originator.is_some()) {
            sip_binds_report.push(format!("udp://{addr}"));
            match spawn_sip_udp(
                addr,
                bus.clone(),
                shutdown.token(),
                Arc::clone(&media_fabric),
                Arc::clone(&metrics),
                Arc::clone(&response_router),
                drain.clone(),
                sip_rate_limit.clone(),
                registrar.clone(),
                /* build_uac */ false,
                std::mem::take(&mut initial_dialogs),
            )
            .await
            {
                Ok(SpawnedSipUdp {
                    handles, dialogs, ..
                }) => {
                    sip_handles.extend(handles);
                    if sip_dialogs_for_snapshot.is_none() {
                        sip_dialogs_for_snapshot = Some(dialogs);
                    }
                }
                Err(e) => warn!(%bind, ?e, "failed to start SIP/UDP on bind; continuing"),
            }
        }
        if tcp_enabled {
            sip_binds_report.push(format!("tcp://{addr}"));
            match spawn_sip_tcp(
                addr,
                bus.clone(),
                shutdown.token(),
                Arc::clone(&media_fabric),
                Arc::clone(&metrics),
                drain.clone(),
                sip_rate_limit.clone(),
                registrar.clone(),
                &config_snapshot.sip.proxy,
            )
            .await
            {
                Ok(handles) => sip_handles.extend(handles),
                Err(e) => warn!(%bind, ?e, "failed to start SIP/TCP on bind; continuing"),
            }
        }
        if tls_enabled
            && let (Some(cert), Some(key)) = (&config.sip.tls_cert_path, &config.sip.tls_key_path)
        {
            sip_binds_report.push(format!("tls://{addr}"));
            match spawn_sip_tls(
                addr,
                cert,
                key,
                bus.clone(),
                shutdown.token(),
                Arc::clone(&media_fabric),
                Arc::clone(&metrics),
                drain.clone(),
                sip_rate_limit.clone(),
                registrar.clone(),
            )
            .await
            {
                Ok(handles) => sip_handles.extend(handles),
                Err(e) => warn!(%bind, ?e, "failed to start SIP/TLS on bind; continuing"),
            }
        }
    }

    // ---- health + metrics HTTP endpoint ----
    // Spawned after SIP bind collection so /health's snapshot is
    // complete on the first request.
    let health_state = HealthState {
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        sip_binds: sip_binds_report,
        plugins_loaded,
        plugins_failed,
        metrics: Arc::clone(&metrics),
        drain: drain.clone(),
    };
    let health = tokio::spawn(serve_health(
        config.observability.health_bind,
        shutdown.token(),
        Arc::clone(&metrics_registry),
        health_state,
    ));

    // ---- A2A HTTP adapter (optional) ----
    let mut adapter_handles: Vec<JoinHandle<()>> = Vec::new();
    if config.a2a.enabled {
        let bind = config.a2a.bind;
        let reg = Arc::clone(&registry);
        let res = Arc::clone(&resources);
        let rl = Arc::clone(&rate_limiter);
        let met = Arc::clone(&metrics);
        let bearer = config.a2a.bearer_token.clone();
        let ctx = tool_ctx.clone();
        let cancel = shutdown.token();
        adapter_handles.push(tokio::spawn(async move {
            if let Err(e) =
                smiths_mcp::a2a::serve_http(bind, reg, res, rl, met, bearer, ctx, cancel).await
            {
                warn!(%bind, ?e, "A2A HTTP server error");
            }
        }));
    }

    // ---- MCP HTTP + SSE adapter (optional) ----
    if config.mcp.enabled_http {
        let bind = config.mcp.http_bind;
        let reg = Arc::clone(&registry);
        let res = Arc::clone(&resources);
        let rl = Arc::clone(&rate_limiter);
        let met = Arc::clone(&metrics);
        let ctx = tool_ctx.clone();
        let bus_clone = bus.clone();
        let cancel = shutdown.token();
        adapter_handles.push(tokio::spawn(async move {
            if let Err(e) =
                smiths_mcp::mcp_http::serve_http(bind, reg, res, rl, met, ctx, bus_clone, cancel)
                    .await
            {
                warn!(%bind, ?e, "MCP HTTP server error");
            }
        }));
        info!("MCP HTTP adapter: http/1.1 + http/2 negotiated via ALPN when TLS-terminated");
    }

    // Slice 4.3 / P17: mcp-http3 scaffold — feature gate + config
    // accepted, runtime listener deferred. Warn clearly so a mis-
    // set `enabled = true` never looks like success.
    if config.mcp.http3.enabled {
        if cfg!(feature = "mcp-http3") {
            warn!(
                bind = %config.mcp.http3.bind,
                "mcp.http3.enabled = true: runtime quinn+h3 listener not yet wired in 0.45.0. \
                 Config accepted; no QUIC port will open. \
                 See docs/architecture/07-http3.md for the rollout plan."
            );
        } else {
            warn!(
                "mcp.http3.enabled = true but binary built without \
                 --features mcp-http3; ignoring."
            );
        }
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
    // Graceful drain: flip the UAS flag first, sleep the drain
    // window so live dialogs can reach BYE on their own, then fire
    // the hard cancel. `SMITHS_DRAIN_SECS` (default 5) overrides;
    // setting it to `0` keeps the pre-v0.13 instant-cancel behaviour
    // for tight test loops.
    drain.start();
    info!("shutdown signal received; draining new INVITEs");
    let _ = bus.publish(Event::System(SystemEvent::ShutdownRequested));
    let drain_secs: u64 = std::env::var("SMITHS_DRAIN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    if drain_secs > 0 {
        info!(drain_secs, "holding new INVITEs with 503 during drain");
        tokio::time::sleep(std::time::Duration::from_secs(drain_secs)).await;
    }
    shutdown.trigger();

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

    // Slice 6.1: write the HA snapshot if the operator wired a
    // path. Happens after every SIP / adapter / health task has
    // joined, so the dialog table is quiescent — no races with
    // UAS writes. The sink / dialogs handles are independent so
    // either can be `None` (MCP-only mode, no UDP bind, etc.)
    // without breaking the other.
    if let (Some(path), Some(dialogs)) = (snapshot_sink, sip_dialogs_for_snapshot) {
        match smiths_sip::write_snapshot(&path, &dialogs) {
            Ok(n) => info!(path = %path.display(), count = n, "HA snapshot written"),
            Err(e) => warn!(path = %path.display(), ?e, "HA snapshot write failed"),
        }
    }

    let _ = bus.publish(Event::System(SystemEvent::ShutdownComplete));
    info!("graceful shutdown complete");
    Ok(())
}

struct SpawnedSipUdp {
    handles: Vec<JoinHandle<()>>,
    /// Populated only on the bind we designate as the outbound-call
    /// origin. `None` for every other UDP listener.
    uac: Option<Arc<UacClient<UdpTransport>>>,
    /// Shared handle on the UAS's dialog table (slice 6.1). Cloned
    /// out before `run()` is spawned so the shutdown path can
    /// serialize every live dialog to disk without reaching into
    /// the private `UasServer` state.
    dialogs: Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
}

/// Fire-and-forget retention sweep for the filesystem recording
/// store (slice 3.4). Runs once at boot then every hour until
/// shutdown fires. `retention_days = 0` disables the sweep entirely
/// so operators who want indefinite retention opt in by leaving
/// the field at its default.
fn spawn_recording_retention_sweeper(
    store: Arc<dyn smiths_core::RecordingStore>,
    retention_days: u32,
    cancel: CancellationToken,
) {
    if retention_days == 0 {
        return;
    }
    let max_age = std::time::Duration::from_secs(u64::from(retention_days) * 86_400);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    match store.prune_older_than(max_age) {
                        Ok(n) if n > 0 => {
                            tracing::info!(removed = n, retention_days,
                                "recording retention sweep removed expired blobs");
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(?e, "recording retention sweep failed");
                        }
                    }
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn spawn_sip_udp(
    bind: SocketAddr,
    bus: EventBus,
    cancel: CancellationToken,
    media_fabric: Arc<dyn MediaFabric>,
    metrics: Arc<Metrics>,
    router: Arc<ResponseRouter>,
    drain: smiths_core::Drain,
    rate_limit: smiths_sip::SipRateLimiter,
    registrar: Option<smiths_sip::auth::digest::Registrar>,
    build_uac: bool,
    restore_dialogs: Vec<smiths_core::DialogRecord>,
) -> anyhow::Result<SpawnedSipUdp> {
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
    let mut server = UasServer::new(
        Arc::clone(&transport),
        bus.clone(),
        Arc::clone(&media_fabric),
        Arc::clone(&negotiator),
    )
    .with_context(|| format!("building UAS on {local}"))?
    .with_metrics(Arc::clone(&metrics))
    .with_response_router(Arc::clone(&router))
    .with_drain(drain.clone())
    .with_rate_limit(rate_limit.clone());
    if let Some(reg) = registrar {
        server = server.with_registrar(reg);
    }
    // Slice 6.1: replay any pre-shutdown snapshot before `run`
    // takes ownership of the server. Also clone out the dialogs
    // handle so the shutdown path can serialize live state.
    let restored = server.restore_dialogs(restore_dialogs);
    if restored > 0 {
        metrics.snapshot_replay_dialogs.inc_by(restored as u64);
        info!(
            restored,
            "HA snapshot replay: {restored} dialog records restored"
        );
    }
    let dialogs = server.dialogs_handle();
    let server_handle = tokio::spawn(server.run(rx, cancel));
    info!(%local, "SIP UDP listening");

    let uac = if build_uac {
        let uac = Arc::new(UacClient::new(
            Arc::clone(&transport),
            bus,
            media_fabric,
            negotiator,
            router,
            local,
            metrics,
        ));
        info!(%local, "SIP UDP UAC ready");
        Some(uac)
    } else {
        None
    };

    Ok(SpawnedSipUdp {
        handles: vec![reader, server_handle],
        uac,
        dialogs,
    })
}

#[allow(clippy::too_many_arguments)]
async fn spawn_sip_tls(
    bind: SocketAddr,
    cert: &std::path::Path,
    key: &std::path::Path,
    bus: EventBus,
    cancel: CancellationToken,
    media_fabric: Arc<dyn MediaFabric>,
    metrics: Arc<Metrics>,
    drain: smiths_core::Drain,
    rate_limit: smiths_sip::SipRateLimiter,
    registrar: Option<smiths_sip::auth::digest::Registrar>,
) -> anyhow::Result<Vec<JoinHandle<()>>> {
    let transport = TlsTransport::bind(bind, cert, key)
        .await
        .with_context(|| format!("binding TLS on {bind}"))?;
    let local = transport.local_addr()?;
    let transport = Arc::new(transport);

    let (tx, rx) = mpsc::channel(1024);
    let reader = transport.spawn_reader(tx, cancel.clone());

    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let mut server = UasServer::new(Arc::clone(&transport), bus, media_fabric, negotiator)
        .with_context(|| format!("building UAS on {local}"))?
        .with_metrics(metrics)
        .with_drain(drain)
        .with_rate_limit(rate_limit);
    if let Some(reg) = registrar {
        server = server.with_registrar(reg);
    }
    let server_handle = tokio::spawn(server.run(rx, cancel));
    info!(%local, "SIP TLS listening");
    Ok(vec![reader, server_handle])
}

#[allow(clippy::too_many_arguments)]
async fn spawn_sip_tcp(
    bind: SocketAddr,
    bus: EventBus,
    cancel: CancellationToken,
    media_fabric: Arc<dyn MediaFabric>,
    metrics: Arc<Metrics>,
    drain: smiths_core::Drain,
    rate_limit: smiths_sip::SipRateLimiter,
    registrar: Option<smiths_sip::auth::digest::Registrar>,
    proxy_cfg: &smiths_core::SipProxyConfig,
) -> anyhow::Result<Vec<JoinHandle<()>>> {
    let mut transport = TcpTransport::bind(bind)
        .await
        .with_context(|| format!("binding TCP on {bind}"))?;
    // Slice 3.5: wrap outbound connects in an operator-configured
    // proxy. Inbound accepts are untouched — only the `send` path
    // changes shape.
    let connector = smiths_sip::transport::proxy::connector_from_config(proxy_cfg)
        .with_context(|| "building sip.proxy connector")?;
    let label = connector.label();
    transport = transport.with_proxy(connector);
    if label != "direct" {
        info!(mode = label, ?proxy_cfg.address, "sip outbound proxy engaged");
    }
    let local = transport.local_addr()?;
    let transport = Arc::new(transport);

    let (tx, rx) = mpsc::channel(1024);
    let reader = transport.spawn_reader(tx, cancel.clone());

    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let mut server = UasServer::new(Arc::clone(&transport), bus, media_fabric, negotiator)
        .with_context(|| format!("building UAS on {local}"))?
        .with_metrics(metrics)
        .with_drain(drain)
        .with_rate_limit(rate_limit);
    if let Some(reg) = registrar {
        server = server.with_registrar(reg);
    }
    let server_handle = tokio::spawn(server.run(rx, cancel));
    info!(%local, "SIP TCP listening");
    Ok(vec![reader, server_handle])
}

/// Build an `Arc<Registrar>` from `SMITHS_TEST_CREDS` if set.
/// Format: `user:realm:pass[,user:realm:pass…]`. All credentials
/// share the first realm — the realm is the Registrar's challenge
/// scope, and SIP auth only validates accounts within it. Returns
/// `None` when the env var is unset or malformed (logged).
fn build_test_registrar() -> Option<smiths_sip::auth::digest::Registrar> {
    use smiths_sip::auth::digest::Registrar;
    use smiths_sip::auth::{Credentials, InMemoryCredentialStore};
    use std::sync::Arc;
    let raw = std::env::var("SMITHS_TEST_CREDS").ok()?;
    let store = Arc::new(InMemoryCredentialStore::new());
    let mut realm: Option<String> = None;
    for entry in raw.split(',').filter(|s| !s.is_empty()) {
        let mut parts = entry.splitn(3, ':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(u), Some(r), Some(p)) if !u.is_empty() && !r.is_empty() => {
                if realm.is_none() {
                    realm = Some(r.to_owned());
                }
                store.insert(Credentials::new(u, r, p));
            }
            _ => {
                warn!(
                    entry,
                    "SMITHS_TEST_CREDS entry ignored (expected user:realm:pass)"
                );
            }
        }
    }
    let realm = realm?;
    info!(
        realm = %realm,
        accounts = store.len(),
        "test credential store seeded from SMITHS_TEST_CREDS"
    );
    Some(Registrar::new(&realm, store))
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

/// Snapshot of startup state + live handles that the `/health`
/// endpoint reports on. Cloneable; the handler holds it behind axum's
/// `State` extractor.
#[derive(Clone)]
struct HealthState {
    started_at: u64,
    sip_binds: Vec<String>,
    plugins_loaded: Vec<String>,
    plugins_failed: Vec<(String, String)>,
    metrics: Arc<smiths_core::Metrics>,
    drain: smiths_core::Drain,
}

/// Combined state the axum router carries: the Prometheus registry
/// for `/metrics` and the rich `HealthState` for `/health`.
#[derive(Clone)]
struct HttpState {
    registry: Arc<Mutex<Registry>>,
    health: HealthState,
}

async fn serve_health(
    bind: SocketAddr,
    cancel: CancellationToken,
    registry: Arc<Mutex<Registry>>,
    health: HealthState,
) -> anyhow::Result<()> {
    let state = HttpState { registry, health };
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(state);
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding health endpoint on {bind}"))?;
    info!(%bind, "health + metrics endpoint listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .context("health server")?;
    info!("health endpoint stopped");
    Ok(())
}

/// Detailed `/health` payload. Fields are stable — consumers (k8s
/// liveness / load balancer health probes) rely on this shape.
async fn health_handler(
    axum::extract::State(state): axum::extract::State<HttpState>,
) -> Json<serde_json::Value> {
    let hs = &state.health;
    let uptime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(hs.started_at);
    let status = if hs.drain.is_draining() {
        "draining"
    } else {
        "ok"
    };
    let failed: Vec<serde_json::Value> = hs
        .plugins_failed
        .iter()
        .map(|(dir, err)| serde_json::json!({ "dir": dir, "error": err }))
        .collect();
    Json(serde_json::json!({
        "status": status,
        "draining": hs.drain.is_draining(),
        "uptime_secs": uptime,
        "sip": { "binds": hs.sip_binds },
        "plugins": {
            "loaded": hs.plugins_loaded,
            "failed": failed,
        },
        "dialogs_active": hs.metrics.dialogs_active.get(),
        "bridges_active": hs.metrics.bridges_active.get(),
    }))
}

async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<HttpState>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    let mut out = String::new();
    let guard = state.registry.lock().await;
    if let Err(e) = prometheus_client::encoding::text::encode(&mut out, &guard) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("encode: {e}")).into_response();
    }
    (
        [(
            CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        out,
    )
        .into_response()
}
