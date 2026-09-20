//! Engine wiring: builds every subsystem from a validated
//! [`Config`], runs until a shutdown trigger, then tears down in
//! order. `main` only parses the CLI and calls [`run`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use prometheus_client::registry::Registry;
use smiths_core::call::CallOriginator;
use smiths_core::{
    Config, ConfigReloader, Drain, Event, EventBus, MediaFabric, Metrics, Shutdown, SipTransport,
    SystemEvent,
};
use smiths_mcp::{ControlState, ToolContext};
use smiths_media::UdpMediaFabric;
use smiths_sip::ResponseRouter;
use smiths_sip::uas::SessionTimerConfig;
use smiths_transcode::CpuBudget;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::logging::LogReloader;
use crate::reload_driver::{self, AdapterTargets};
use crate::replication_service::DialogTable;
use crate::sip_spawn::{DialogHangup, SipListenerDeps, SipListenerKind, SpawnedSip, spawn_sip};
use crate::transcode::BudgetedTranscodeOrchestrator;
use crate::{ha, http, retention, shutdown, webrtc};

/// Run-mode flags from the command line.
pub(crate) struct RunOptions {
    /// Config file the SIGHUP driver re-reads.
    pub config_path: PathBuf,
    /// Serve MCP over stdio alongside everything else.
    pub mcp_stdio: bool,
    /// HA dialog snapshot file.
    pub snapshot_path: Option<PathBuf>,
    /// Ignore SIGHUP entirely.
    pub no_reload_signal: bool,
}

/// Handles shared by every subsystem.
struct EngineCore {
    shutdown: Shutdown,
    bus: EventBus,
    drain: Drain,
    metrics: Arc<Metrics>,
    metrics_registry: Arc<Mutex<Registry>>,
    udp_media_fabric: Arc<UdpMediaFabric>,
    media_fabric: Arc<dyn MediaFabric>,
    sip_rate_limit: smiths_sip::SipRateLimiter,
    registrar: Option<smiths_sip::auth::digest::Registrar>,
    conference_registry: Arc<dyn smiths_mixer::ConferenceRegistry>,
    conference_orchestrator: Arc<dyn smiths_sip::ConferenceOrchestrator>,
    control_state: ControlState,
    control_task: JoinHandle<()>,
    response_router: Arc<ResponseRouter>,
    cpu_budget: CpuBudget,
    transcode_orchestrator: Arc<dyn smiths_sip::TranscodeOrchestrator>,
}

/// Plugin loader output.
struct PluginRuntime {
    ai_registry: smiths_plugin::AiRegistry,
    ai_registry_dyn: Arc<dyn smiths_core::AiRegistry>,
    wasm_originator_slot: Option<Arc<std::sync::OnceLock<Arc<dyn CallOriginator>>>>,
    loaded: Vec<String>,
    failed: Vec<(String, String)>,
    registry: Arc<smiths_mcp::ToolRegistry>,
    resources: Arc<smiths_mcp::ResourceRegistry>,
    rate_limiter: Arc<smiths_mcp::RateLimiter>,
}

/// SIP listener output.
struct SipRuntime {
    handles: Vec<JoinHandle<()>>,
    originator: Option<Arc<dyn CallOriginator>>,
    dialogs_for_snapshot: Option<Arc<DialogTable>>,
    hangups: Vec<Arc<dyn DialogHangup>>,
    binds_report: Vec<String>,
}

/// Bring the engine up, serve until shutdown, tear down.
pub(crate) async fn run(
    opts: RunOptions,
    config: Config,
    log_reloader: LogReloader,
) -> anyhow::Result<()> {
    let shutdown = Shutdown::new();
    let core = build_core(&config, shutdown.clone()).await;
    let plugins = load_plugins(&config, &core).await;
    let (initial_dialogs, snapshot_sink) = ha::load_snapshot(opts.snapshot_path.as_deref());
    let ha = ha::start(&config.cluster, shutdown.token()).await?;
    let webrtc_handler = config
        .webrtc
        .enabled
        .then(|| webrtc::build_handler(&config, &core.metrics, Arc::clone(&core.udp_media_fabric)));
    let sip =
        spawn_sip_listeners(&config, &core, &ha, webrtc_handler.clone(), initial_dialogs).await;
    if let (Some(slot), Some(o)) = (&plugins.wasm_originator_slot, &sip.originator) {
        let _ = slot.set(Arc::clone(o));
    }

    let (mut tool_ctx, prompt_library) = build_tool_context(&config, &core, &plugins, &ha, &sip);
    let retention_task = wire_storage(&config, &core, &mut tool_ctx);

    let reloader = ConfigReloader::new_with_cancel(config.clone(), shutdown.token());
    let adapter_handles = reload_driver::wire_read_through_adapters(
        &reloader,
        &core.metrics,
        AdapterTargets {
            log_reloader,
            sip_rate_limit: core.sip_rate_limit.clone(),
            cpu_budget: core.cpu_budget.clone(),
            prompt_library,
            ai_registry: plugins.ai_registry.clone(),
            webrtc_privacy: webrtc_handler.as_ref().map(|h| h.privacy_handle()),
            replication: ha.state.clone(),
        },
    );
    let reload_driver = if opts.no_reload_signal {
        info!("--no-reload-signal set; SIGHUP reload disabled");
        None
    } else {
        reload_driver::spawn_sighup_driver(
            opts.config_path.clone(),
            Arc::clone(&reloader),
            Arc::clone(&core.metrics),
            shutdown.token(),
        )
    };
    tool_ctx = tool_ctx
        .with_reloader(Arc::clone(&reloader))
        .with_config_path(opts.config_path.clone());

    let mcp_stdio_task = opts
        .mcp_stdio
        .then(|| spawn_mcp_stdio(&core, &plugins, &tool_ctx));
    let health = spawn_health(&config, &core, &plugins, &ha, &sip);
    let mut control_adapters = spawn_control_adapters(&config, &core, &plugins, &tool_ctx);
    if let Some(handler) = webrtc_handler {
        control_adapters.extend(webrtc::spawn_servers(
            &config,
            &core.metrics,
            handler,
            &shutdown,
        ));
    }

    if let Err(err) = core.bus.publish(Event::System(SystemEvent::Ready)) {
        warn!(?err, "no bus subscribers at startup");
    }
    info!(
        health_bind = %config.observability.health_bind,
        sip_binds = ?config.sip.bind,
        sip_transports = ?config.sip.transports,
        a2a_enabled = config.a2a.enabled,
        "smiths-net ready"
    );
    wait_for_trigger(&shutdown, mcp_stdio_task).await?;

    let tasks = BackgroundTasks {
        control_adapters,
        adapter_handles,
        reload_driver,
        retention_task,
        health,
    };
    let drain_secs = reloader.current().sip.drain_timeout_secs;
    teardown(
        &shutdown,
        core,
        plugins,
        sip,
        ha,
        tasks,
        drain_secs,
        snapshot_sink,
    )
    .await;
    Ok(())
}

/// Every background task `run` has to join before the process exits.
struct BackgroundTasks {
    control_adapters: Vec<JoinHandle<()>>,
    adapter_handles: Vec<JoinHandle<()>>,
    reload_driver: Option<JoinHandle<()>>,
    retention_task: Option<JoinHandle<()>>,
    health: JoinHandle<anyhow::Result<()>>,
}

/// Drain active calls, then join every task and snapshot the dialog
/// table once the SIP loops are quiescent.
#[allow(clippy::too_many_arguments)] // teardown touches every subsystem `run` built
async fn teardown(
    shutdown: &Shutdown,
    core: EngineCore,
    plugins: PluginRuntime,
    sip: SipRuntime,
    ha: ha::HaRuntime,
    tasks: BackgroundTasks,
    drain_timeout_secs: u64,
    snapshot_sink: Option<PathBuf>,
) {
    let drain_targets = shutdown::DrainTargets {
        originator: sip.originator.clone(),
        control: core.control_state.clone(),
        listeners: sip.hangups.clone(),
    };
    let window = shutdown::drain_window(
        std::env::var("SMITHS_DRAIN_SECS").ok().as_deref(),
        drain_timeout_secs,
    );
    core.drain.start();
    info!("shutdown signal received; draining new INVITEs");
    let _ = core
        .bus
        .publish(Event::System(SystemEvent::ShutdownRequested));
    shutdown::drain(&drain_targets, window).await;
    shutdown.trigger();

    for h in sip.handles {
        shutdown::join_bounded("sip", h).await;
    }
    for h in tasks.control_adapters {
        shutdown::join_bounded("control adapter", h).await;
    }
    for h in tasks.adapter_handles {
        shutdown::join_bounded("config read-through adapter", h).await;
    }
    if let Some(h) = tasks.reload_driver {
        shutdown::join_bounded("config reload driver", h).await;
    }
    if let Some(h) = tasks.retention_task {
        shutdown::join_bounded("recording retention sweeper", h).await;
    }
    for h in ha.tasks {
        shutdown::join_bounded("HA replication", h).await;
    }
    shutdown::join_bounded("health server", tasks.health).await;
    shutdown::join_bounded("control state", core.control_task).await;
    tokio::time::timeout(shutdown::JOIN_TIMEOUT, plugins.ai_registry.shutdown_all())
        .await
        .unwrap_or_else(|_| warn!("plugin sidecars did not stop in time"));

    // The dialog table is quiescent now — every SIP task joined.
    ha::write_snapshot(snapshot_sink.as_deref(), sip.dialogs_for_snapshot.as_ref());
    let _ = core
        .bus
        .publish(Event::System(SystemEvent::ShutdownComplete));
    info!("graceful shutdown complete");
}

async fn build_core(config: &Config, shutdown: Shutdown) -> EngineCore {
    let bus = EventBus::new(1024);
    let drain = Drain::new();
    // Shared across all listeners so a peer can't dodge the limit
    // by hopping transports.
    let sip_rate_limit = smiths_sip::SipRateLimiter::new(config.sip.rate_limit);
    let registrar = build_test_registrar();
    let metrics_registry = Arc::new(Mutex::new(Registry::default()));
    let (metrics, cpu_budget) = {
        let mut guard = metrics_registry.lock().await;
        (
            Metrics::register(&mut guard),
            crate::transcode::build_budget(&config.media.transcode, &mut guard),
        )
    };
    let (control_state, control_task) = ControlState::spawn(&bus, shutdown.token());

    let rtp_port_range = config.media.rtp_ports.map(|r| (r.min, r.max));
    let udp_media_fabric = Arc::new(
        UdpMediaFabric::new()
            .with_metrics(Arc::clone(&metrics))
            .with_rtp_port_range(rtp_port_range),
    );
    let media_fabric: Arc<dyn MediaFabric> = Arc::clone(&udp_media_fabric) as Arc<dyn MediaFabric>;

    let conference_registry: Arc<dyn smiths_mixer::ConferenceRegistry> =
        Arc::new(smiths_mixer::InMemoryConferenceRegistry::new());
    let conference_orchestrator: Arc<dyn smiths_sip::ConferenceOrchestrator> =
        Arc::new(smiths_mixer::MixerConferenceOrchestrator::new(
            Arc::clone(&udp_media_fabric),
            Arc::clone(&conference_registry),
        ));
    let transcode_orchestrator: Arc<dyn smiths_sip::TranscodeOrchestrator> = Arc::new(
        BudgetedTranscodeOrchestrator::new(Arc::clone(&udp_media_fabric), cpu_budget.clone()),
    );

    EngineCore {
        shutdown,
        bus,
        drain,
        metrics,
        metrics_registry,
        udp_media_fabric,
        media_fabric,
        sip_rate_limit,
        registrar,
        conference_registry,
        conference_orchestrator,
        control_state,
        control_task,
        response_router: Arc::new(ResponseRouter::new()),
        cpu_budget,
        transcode_orchestrator,
    }
}

async fn load_plugins(config: &Config, core: &EngineCore) -> PluginRuntime {
    let ai_registry = smiths_plugin::AiRegistry::new();
    // WASM engine failure only disables WASM plugins.
    let wasm_engine = match smiths_plugin::wasm::WasmEngine::new() {
        Ok(e) => Some(e.with_bus(core.bus.clone()).with_media(
            Arc::new(core.control_state.clone()),
            Arc::clone(&core.media_fabric),
        )),
        Err(err) => {
            warn!(
                ?err,
                "wasm engine init failed; wasm plugins will be skipped"
            );
            None
        }
    };
    // Write-once originator slot every `WasmProvider` shares; filled
    // once the UAC exists.
    let wasm_originator_slot = wasm_engine
        .as_ref()
        .map(smiths_plugin::wasm::WasmEngine::originator_slot);
    let loader_opts = smiths_plugin::LoaderOpts {
        bus: Some(core.bus.clone()),
        wasm_engine,
        metrics: Some(Arc::clone(&core.metrics)),
        sandbox: config.plugins.sandbox.clone(),
        wasm_memory_limit_bytes: usize::try_from(
            config
                .plugins
                .wasm
                .memory_limit_mb
                .saturating_mul(1024 * 1024),
        )
        .unwrap_or(usize::MAX),
        wasm_invoke_timeout: Duration::from_millis(config.plugins.wasm.invoke_timeout_ms),
        ..smiths_plugin::LoaderOpts::default()
    };
    let (mut loaded, mut failed) = (Vec::new(), Vec::new());
    match smiths_plugin::load_plugins(&config.plugins.dir, &ai_registry, loader_opts).await {
        Ok(report) => {
            if !report.loaded.is_empty() {
                info!(loaded = ?report.loaded, "plugins ready");
            }
            for (dir, err) in &report.failed {
                warn!(%dir, %err, "plugin load failed");
            }
            loaded = report.loaded;
            failed = report.failed;
        }
        Err(e) => warn!(?e, "plugin scan failed"),
    }
    let ai_registry_dyn: Arc<dyn smiths_core::AiRegistry> = Arc::new(ai_registry.clone());
    // Seed the env snapshot from boot config so the very first
    // sidecar spawn sees the configured keys.
    if let Some(v) = config.ai.openai_api_key.as_ref() {
        ai_registry.set_env("OPENAI_API_KEY", v.clone());
    }
    if let Some(v) = config.ai.anthropic_api_key.as_ref() {
        ai_registry.set_env("ANTHROPIC_API_KEY", v.clone());
    }
    // Only spawns a consumer when `call_event_hooks` is non-empty.
    let _hooks = smiths_plugin::spawn_call_event_hooks(
        &core.bus,
        &ai_registry,
        config.plugins.call_event_hooks.clone(),
        core.shutdown.token(),
    );
    PluginRuntime {
        ai_registry,
        ai_registry_dyn,
        wasm_originator_slot,
        loaded,
        failed,
        registry: Arc::new(smiths_mcp::tools::builtin_registry()),
        resources: Arc::new(smiths_mcp::builtin_resources()),
        rate_limiter: Arc::new(smiths_mcp::RateLimiter::new(&config.mcp.rate_limit)),
    }
}

/// Bind every enabled transport on every configured address. The
/// first UDP listener that binds becomes the outbound-call origin;
/// the first listener of any kind owns the HA snapshot table. Bind
/// failures are logged and skipped so one bad address doesn't take
/// the engine down.
async fn spawn_sip_listeners(
    config: &Config,
    core: &EngineCore,
    ha: &ha::HaRuntime,
    webrtc_handler: Option<Arc<webrtc::CliWebRtcHandler>>,
    mut initial_dialogs: Vec<smiths_core::DialogRecord>,
) -> SipRuntime {
    let sip = &config.sip;
    let mut kinds: Vec<SipListenerKind> = Vec::new();
    if sip.transports.contains(&SipTransport::Udp) {
        kinds.push(SipListenerKind::Udp);
    }
    if sip.transports.contains(&SipTransport::Tcp) {
        kinds.push(SipListenerKind::Tcp);
    }
    if sip.transports.contains(&SipTransport::Tls)
        && let (Some(cert), Some(key)) = (&sip.tls_cert_path, &sip.tls_key_path)
    {
        kinds.push(SipListenerKind::Tls {
            cert: cert.clone(),
            key: key.clone(),
        });
    }
    if kinds.is_empty() {
        warn!("no SIP transports configured; signaling disabled");
    }
    let deps = SipListenerDeps {
        bus: core.bus.clone(),
        cancel: core.shutdown.token(),
        media_fabric: Arc::clone(&core.media_fabric),
        metrics: Arc::clone(&core.metrics),
        router: Arc::clone(&core.response_router),
        drain: core.drain.clone(),
        rate_limit: core.sip_rate_limit.clone(),
        registrar: core.registrar.clone(),
        webrtc_rendezvous: webrtc_handler.map(|h| h as Arc<dyn smiths_core::WebRtcRendezvous>),
        replicator: Arc::clone(&ha.replicator),
        dialogs_shared: ha.dialogs.clone(),
        conference_orchestrator: Some(Arc::clone(&core.conference_orchestrator)),
        conference_prefix: sip.conference_prefix.clone(),
        transcode_orchestrator: Some(Arc::clone(&core.transcode_orchestrator)),
        sdp_advertise_ip: sip_advertise_ip(config),
        proxy: sip.proxy.clone(),
        session_timer: SessionTimerConfig {
            enabled: sip.session_timer_enabled,
            default_expires: Duration::from_secs(sip.session_expires_secs),
            min_se: Duration::from_secs(sip.min_se_secs),
        },
        max_call_duration: (sip.max_call_duration_secs > 0)
            .then(|| Duration::from_secs(sip.max_call_duration_secs)),
    };

    let mut rt = SipRuntime {
        handles: Vec::new(),
        originator: None,
        dialogs_for_snapshot: None,
        hangups: Vec::new(),
        binds_report: Vec::new(),
    };
    for bind in &sip.bind {
        let addr = bind.socket_addr();
        for kind in &kinds {
            // `/health` reports intent: the entry stays even when the
            // bind fails (the failure is in the startup log).
            rt.binds_report.push(format!("{}://{addr}", kind.scheme()));
            let build_uac = matches!(kind, SipListenerKind::Udp) && rt.originator.is_none();
            let restore = std::mem::take(&mut initial_dialogs);
            match spawn_sip(kind, addr, &deps, build_uac, restore).await {
                Ok(SpawnedSip {
                    handles,
                    originator,
                    dialogs,
                    hangup,
                }) => {
                    rt.handles.extend(handles);
                    if originator.is_some() {
                        rt.originator = originator;
                    }
                    rt.dialogs_for_snapshot.get_or_insert(dialogs);
                    rt.hangups.push(hangup);
                }
                Err(e) => {
                    warn!(%bind, scheme = kind.scheme(), ?e, "failed to start SIP listener; continuing");
                }
            }
        }
    }
    rt
}

fn sip_advertise_ip(config: &Config) -> Option<std::net::IpAddr> {
    let ip = config
        .media
        .advertise_ip
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok());
    if let Some(ip) = ip {
        info!(%ip, "SDP answers will advertise public media IP");
    }
    ip
}

fn build_tool_context(
    config: &Config,
    core: &EngineCore,
    plugins: &PluginRuntime,
    ha: &ha::HaRuntime,
    sip: &SipRuntime,
) -> (ToolContext, Option<smiths_media::PromptLibrary>) {
    let mut ctx = ToolContext::new(
        core.control_state.clone(),
        Arc::clone(&plugins.ai_registry_dyn),
        Arc::new(config.clone()),
        Arc::clone(&core.media_fabric),
    )
    .with_metrics(Arc::clone(&core.metrics))
    .with_metrics_registry(Arc::clone(&core.metrics_registry))
    .with_conferences(Arc::clone(&core.conference_registry))
    .with_cluster_status(ha::cluster_status_source(ha.state.clone()));
    if let Some(o) = sip.originator.clone() {
        ctx = ctx.with_originator(o);
    }
    let prompt_library = if config.media.prompts.root.is_empty() {
        None
    } else {
        let mut library = smiths_media::PromptLibrary::with_root(&config.media.prompts.root);
        if config.media.prompts.capacity > 0 {
            library = library.with_capacity(config.media.prompts.capacity);
        }
        ctx = ctx.with_prompts(library.clone());
        info!(root = %config.media.prompts.root, "IVR prompt library wired");
        Some(library)
    };
    (ctx, prompt_library)
}

/// Wire `[storage.vector]` / `[storage.recording]`. The sidecar
/// variants are refused by `Config::validate`, so only the in-tree
/// backends appear here.
fn wire_storage(
    config: &Config,
    core: &EngineCore,
    tool_ctx: &mut ToolContext,
) -> Option<JoinHandle<()>> {
    if config.storage.vector.backend == smiths_core::VectorBackend::Memory {
        let store: Arc<dyn smiths_core::VectorStore> =
            Arc::new(smiths_core::MemoryVectorStore::new());
        *tool_ctx = tool_ctx.clone().with_vector(store);
        info!("vector store: in-memory");
    }
    if config.storage.recording.backend != smiths_core::RecordingBackend::Fs {
        return None;
    }
    let root = &config.storage.recording.fs.root;
    match smiths_core::FsRecordingStore::new(root) {
        Ok(store) => {
            let handle: Arc<dyn smiths_core::RecordingStore> = Arc::new(store);
            *tool_ctx = tool_ctx.clone().with_recording(Arc::clone(&handle));
            info!(root = %root.display(), "recording store: filesystem");
            retention::spawn_recording_retention_sweeper(
                handle,
                config.storage.recording.retention_days,
                core.shutdown.token(),
            )
        }
        Err(e) => {
            warn!(root = %root.display(), ?e, "recording store: failed to initialize; continuing without");
            None
        }
    }
}

fn spawn_mcp_stdio(
    core: &EngineCore,
    plugins: &PluginRuntime,
    tool_ctx: &ToolContext,
) -> JoinHandle<()> {
    let reg = Arc::clone(&plugins.registry);
    let res = Arc::clone(&plugins.resources);
    let rl = Arc::clone(&plugins.rate_limiter);
    let met = Arc::clone(&core.metrics);
    let ctx = tool_ctx.clone();
    let bus = core.bus.clone();
    let cancel = core.shutdown.token();
    tokio::spawn(async move {
        if let Err(e) = smiths_mcp::mcp::run_stdio(reg, res, rl, met, ctx, bus, cancel).await {
            warn!(?e, "MCP stdio server error");
        }
    })
}

fn spawn_health(
    config: &Config,
    core: &EngineCore,
    plugins: &PluginRuntime,
    ha: &ha::HaRuntime,
    sip: &SipRuntime,
) -> JoinHandle<anyhow::Result<()>> {
    let health_state = http::HealthState {
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        sip_binds: sip.binds_report.clone(),
        plugins_loaded: plugins.loaded.clone(),
        plugins_failed: plugins.failed.clone(),
        metrics: Arc::clone(&core.metrics),
        drain: core.drain.clone(),
        replication: ha.state.clone(),
    };
    tokio::spawn(http::serve_health(
        config.observability.health_bind,
        core.shutdown.token(),
        Arc::clone(&core.metrics_registry),
        health_state,
    ))
}

/// A2A + MCP HTTP adapters (both optional).
fn spawn_control_adapters(
    config: &Config,
    core: &EngineCore,
    plugins: &PluginRuntime,
    tool_ctx: &ToolContext,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    if config.a2a.enabled {
        let bind = config.a2a.bind;
        let reg = Arc::clone(&plugins.registry);
        let res = Arc::clone(&plugins.resources);
        let rl = Arc::clone(&plugins.rate_limiter);
        let met = Arc::clone(&core.metrics);
        let bearer = config.a2a.bearer_token.clone();
        let ctx = tool_ctx.clone();
        let cancel = core.shutdown.token();
        handles.push(tokio::spawn(async move {
            if let Err(e) =
                smiths_mcp::a2a::serve_http(bind, reg, res, rl, met, bearer, ctx, cancel).await
            {
                warn!(%bind, ?e, "A2A HTTP server error");
            }
        }));
    }
    if config.mcp.enabled_http {
        let bind = config.mcp.http_bind;
        let server = smiths_mcp::mcp_http::McpHttpServer::new(
            Arc::clone(&plugins.registry),
            Arc::clone(&plugins.resources),
            Arc::clone(&plugins.rate_limiter),
            Arc::clone(&core.metrics),
            tool_ctx.clone(),
            core.bus.clone(),
        )
        .with_http_bearer(config.mcp.http.bearer_token.clone());
        let cancel = core.shutdown.token();
        handles.push(tokio::spawn(async move {
            if let Err(e) = server.serve(bind, cancel).await {
                warn!(%bind, ?e, "MCP HTTP server error");
            }
        }));
        info!(
            bearer = config.mcp.http.bearer_token.is_some(),
            "MCP HTTP adapter: http/1.1 + http/2 negotiated via ALPN when TLS-terminated"
        );
    }
    handles
}

/// Block until SIGINT / SIGTERM, or until the MCP stdio task exits
/// (stdin EOF — how an LLM host stops its subprocess).
async fn wait_for_trigger(
    shutdown: &Shutdown,
    mcp_stdio_task: Option<JoinHandle<()>>,
) -> anyhow::Result<()> {
    match mcp_stdio_task {
        Some(task) => {
            tokio::select! {
                r = shutdown.wait_for_signal() => r.context("installing signal handlers")?,
                _ = task => info!("MCP stdio closed; shutting down"),
            }
        }
        None => shutdown
            .wait_for_signal()
            .await
            .context("installing signal handlers")?,
    }
    Ok(())
}

/// Build a `Registrar` from `SMITHS_TEST_CREDS` if set. Format:
/// `user:realm:pass[,user:realm:pass…]`; all credentials share the
/// first realm. `None` when unset or malformed (logged).
fn build_test_registrar() -> Option<smiths_sip::auth::digest::Registrar> {
    use smiths_sip::auth::digest::Registrar;
    use smiths_sip::auth::{Credentials, InMemoryCredentialStore};
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
            _ => warn!(
                entry,
                "SMITHS_TEST_CREDS entry ignored (expected user:realm:pass)"
            ),
        }
    }
    let realm = realm?;
    info!(realm = %realm, accounts = store.len(), "test credential store seeded from SMITHS_TEST_CREDS");
    Some(Registrar::new(&realm, store))
}
