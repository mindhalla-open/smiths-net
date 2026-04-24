//! smiths-net binary entry point.

mod replication_service;
mod webrtc;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use axum::{Json, Router, routing::get};
use clap::{Args, Parser, Subcommand, ValueEnum};
use prometheus_client::registry::Registry;
use smiths_core::call::CallOriginator;
use smiths_core::probe::{ErrorRateProbe, ProbeConfig};
use smiths_core::{
    AiRegistry, ChangeReceipt, Config, ConfigReloader, Event, EventBus, LogFormat, MediaFabric,
    Metrics, SdpNegotiator, Shutdown, SipTransport, SystemEvent, hangup_stream,
};
use smiths_mcp::{ControlState, ToolContext};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{
    ResponseRouter, TcpTransport, TlsTransport, Transport as _, UacClient, UasServer, UdpTransport,
};
use smiths_transcode::{CpuBudget, CpuBudgetConfig};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt, prelude::*, reload};

/// Type-erased handle the CLI stashes at `init_tracing` time so
/// the `observability.log_level` read-through adapter can swap
/// the live `EnvFilter` without knowing the tracing subscriber's
/// concrete `Layered<…>` type. `Ok(())` on a successful reload;
/// `Err(msg)` carries a human string for the log line.
type LogReloader = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

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
    /// Run options — shared between the default "run the engine"
    /// invocation and the config subcommands. clap `flatten` so
    /// `smiths-net --config foo validate` and plain
    /// `smiths-net --config foo` both accept `--config`.
    #[command(flatten)]
    run: RunArgs,

    /// Config-management subcommands (slice 5.8-c). Absent
    /// invocation (no subcommand) runs the engine.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Shared flags for "run the engine" + every subcommand. `Args`
/// rather than inline fields so clap renders them once under the
/// top-level command instead of duplicating on every subcommand.
#[derive(Debug, Args)]
struct RunArgs {
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

    /// Slice 5.8-c: opt out of the POSIX SIGHUP reload trigger.
    /// Useful for ops setups that repurpose SIGHUP for something
    /// else (systemd `ReloadSignal=` variants) or test harnesses
    /// that want the engine to ignore stray signals.
    #[arg(long, default_value_t = false)]
    no_reload_signal: bool,
}

/// Subcommands. Absent = run the engine.
#[derive(Debug, Subcommand)]
enum Command {
    /// Load + validate the config without starting the engine.
    /// Exit code carries the outcome so CI / deploy gates can
    /// refuse to ship a broken config (slice 5.8-c Small 1):
    ///
    /// - `0` — clean load + passes `Config::validate`.
    /// - `1` — parse error (file missing / malformed TOML / env
    ///   rejected).
    /// - `2` — semantic error (parse OK, but a cross-field
    ///   invariant tripped, e.g. `sip.transports` includes `tls`
    ///   but `tls_cert_path` is unset).
    Validate,
    /// Hot-reload a running engine's config through the same
    /// `Config::load` + `Config::validate` + `ConfigReloader::apply`
    /// path SIGHUP drives (slice 5.8-c Small 2). Signals the
    /// engine via PID. With `--dry-run`, skips the signal and
    /// prints what the diff would look like — useful for
    /// pre-flight in CI.
    Reload(ReloadArgs),
}

#[derive(Debug, Args)]
struct ReloadArgs {
    /// PID of the running `smiths-net` process to SIGHUP. Read
    /// from `SMITHS_PID` env var when omitted. Required for the
    /// live-apply path; optional for `--dry-run`.
    #[arg(long, env = "SMITHS_PID")]
    pid: Option<i32>,
    /// Print the `ApplyReport` (diff against defaults) before
    /// signalling. Combined with `--dry-run` this is the "show
    /// me what would happen" pre-flight.
    #[arg(long, default_value_t = false)]
    diff: bool,
    /// Stop after the load + validate + diff phase — don't send
    /// SIGHUP, don't mutate anything.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
    /// Seconds the new config has to prove itself before auto-
    /// rollback. Overrides `[canary] deadline_s` on the target
    /// engine — applied on top of the engine's SIGHUP apply path.
    /// `None` = use the target's configured deadline. Today this
    /// is informational only (SIGHUP reads the deadline from the
    /// target's own config); a future slice wires it through an
    /// MCP channel so the overriding value travels with the
    /// signal.
    #[arg(long)]
    canary_secs: Option<u64>,
}

#[tokio::main]
#[allow(clippy::too_many_lines)] // wiring of all subsystems belongs in one place
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Subcommands run a strict load / validate / signal path and
    // exit with a stable code — no engine startup, no metrics
    // registration. The engine proper runs when `command` is
    // absent.
    match cli.command {
        Some(Command::Validate) => {
            run_validate(&cli.run.config);
            return Ok(());
        }
        Some(Command::Reload(ref args)) => return run_reload(&cli.run.config, args),
        None => {}
    }

    let config = Config::load(&cli.run.config)
        .with_context(|| format!("loading config from {}", cli.run.config.display()))?;
    // Slice 5.8-c: engine startup runs the same `validate` the
    // `validate` subcommand does — keeps "it loaded" from masking
    // a broken cross-field invariant during `main()` wiring.
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("config validation failed: {e}"))?;

    // stdio MCP must not pollute stdout with logs or framing garbage.
    // Route everything to stderr and shut off pretty/JSON frames.
    let level = cli
        .run
        .log
        .as_deref()
        .unwrap_or(&config.observability.log_level);
    let log_reloader = init_tracing(
        level,
        config.observability.log_format,
        cli.run.mcp.is_some(),
    )?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.run.config.display(),
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
    ) = if let Some(path) = cli.run.snapshot_path.clone() {
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

    // Slice 5.10-sipjoin: when the WebRTC adapter is enabled
    // we build its `CliWebRtcHandler` early (before SIP binds)
    // so the SIP UAS can receive an `Arc<dyn WebRtcRendezvous>`
    // handle on construction. The WebSocket server itself is
    // spawned later, after all the other adapters; this split
    // keeps the signaling listener spawn next to the rest of
    // the CLI's adapter wiring.
    let (webrtc_handler, webrtc_rendezvous): (
        Option<Arc<webrtc::CliWebRtcHandler>>,
        Option<Arc<dyn smiths_core::WebRtcRendezvous>>,
    ) = if config.webrtc.enabled {
        let handler = build_webrtc_handler(&config, &metrics);
        let rdv: Arc<dyn smiths_core::WebRtcRendezvous> = Arc::clone(&handler) as _;
        (Some(handler), Some(rdv))
    } else {
        (None, None)
    };

    // ---- HA Replication (slice 6.2) ----
    let mut replicator: Arc<dyn smiths_core::Replicator> = Arc::new(smiths_core::NoopReplicator);
    let mut dialogs_shared: Option<
        Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    > = None;

    if config.cluster.mode == smiths_core::ClusterMode::Primary {
        if let Some(peer_addr) = config.cluster.peer_addr {
            let (tx, rx) = mpsc::channel(1024);
            replicator = Arc::new(replication_service::PrimaryReplicator::new(tx));
            let cancel = shutdown.token();
            tokio::spawn(async move {
                tokio::select! {
                    () = cancel.cancelled() => {},
                    res = replication_service::run_primary_service(peer_addr, rx) => {
                        if let Err(e) = res {
                            tracing::error!(?e, "HA Primary replication service failed");
                        }
                    }
                }
            });
        } else {
            warn!("HA mode = primary but cluster.peer_addr is unset; replication disabled");
        }
    } else if config.cluster.mode == smiths_core::ClusterMode::Secondary {
        if let Some(peer_addr) = config.cluster.peer_addr {
            let shared = Arc::new(dashmap::DashMap::new());
            dialogs_shared = Some(Arc::clone(&shared));
            for record in std::mem::take(&mut initial_dialogs) {
                shared.insert(record.key(), record);
            }
            let cancel = shutdown.token();
            let shared_for_listener = Arc::clone(&shared);
            tokio::spawn(async move {
                tokio::select! {
                    () = cancel.cancelled() => {},
                    res = replication_service::run_secondary_service(peer_addr, shared_for_listener) => {
                        if let Err(e) = res {
                            tracing::error!(?e, "HA Secondary replication service failed");
                        }
                    }
                }
            });
        } else {
            warn!("HA mode = secondary but cluster.peer_addr is unset; replication disabled");
        }
    }

    // Build the UAC from the first configured UDP bind. The UAC shares
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
            webrtc_rendezvous.clone(),
            /* build_uac */ true,
            std::mem::take(&mut initial_dialogs),
            Arc::clone(&replicator),
            dialogs_shared.as_ref().map(Arc::clone),
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
    //
    // Hold a handle here so the 5.8-b `media.prompts.capacity`
    // read-through adapter can resize the LRU in place on config
    // reload.
    let prompt_library: Option<smiths_media::PromptLibrary> =
        if config_snapshot.media.prompts.root.is_empty() {
            None
        } else {
            let mut library =
                smiths_media::PromptLibrary::with_root(&config_snapshot.media.prompts.root);
            if config_snapshot.media.prompts.capacity > 0 {
                library = library.with_capacity(config_snapshot.media.prompts.capacity);
            }
            tool_ctx = tool_ctx.with_prompts(library.clone());
            tracing::info!(
                root = %config_snapshot.media.prompts.root,
                "IVR prompt library wired"
            );
            Some(library)
        };

    // Slice 5.3: dormant `CpuBudget` tied to `[media.transcode]`.
    // The UAS admission path consumes this in a dedicated follow-
    // on slice; today the budget exists so the 5.8-b read-through
    // adapter has something non-trivial to update for
    // `media.transcode.max_concurrent_calls`.
    let transcode_metrics = smiths_transcode::TranscodeMetrics::noop();
    let cpu_budget = CpuBudget::new(
        CpuBudgetConfig::from(&config_snapshot.media.transcode),
        transcode_metrics,
    );

    // Slice 5.8-b: build the reloader + wire per-subsystem
    // adapters. Every watcher is cloned once from the reloader's
    // `watch::Receiver`, fires only on actual value changes, and
    // bumps `smiths_config_reloaded_fields_total{field}` so
    // operators see live hot-reload activity.
    let config_reloader = ConfigReloader::new(config.clone());
    let mut reload_adapter_handles: Vec<JoinHandle<()>> = Vec::new();

    // `observability.log_level` → tracing-subscriber reload handle.
    {
        let reloader_fn = log_reloader;
        reload_adapter_handles.push(config_reloader.spawn_read_through(
            "observability.log_level",
            Some(Arc::clone(&metrics)),
            |c: &Config| c.observability.log_level.clone(),
            move |new_level: &String| match reloader_fn(new_level.as_str()) {
                Ok(()) => {
                    tracing::info!(new_level = %new_level, "log filter reloaded");
                }
                Err(e) => {
                    tracing::warn!(?e, new_level = %new_level,
                            "log filter reload rejected; keeping the prior filter");
                }
            },
        ));
    }

    // `sip.rate_limit` → `SipRateLimiter::reconfigure`. The
    // limiter is lock-free, so a swap costs two atomic stores
    // and existing per-IP buckets keep their tokens.
    {
        let limiter = sip_rate_limit.clone();
        reload_adapter_handles.push(config_reloader.spawn_read_through(
            "sip.rate_limit",
            Some(Arc::clone(&metrics)),
            |c: &Config| c.sip.rate_limit,
            move |new_cfg: &smiths_core::SipRateLimit| {
                limiter.reconfigure(*new_cfg);
                tracing::info!(
                    per_sec = new_cfg.per_sec,
                    burst = new_cfg.burst,
                    "sip.rate_limit reconfigured"
                );
            },
        ));
    }

    // `media.transcode.max_concurrent_calls` → `CpuBudget::set_max_concurrent`.
    {
        let budget = cpu_budget.clone();
        reload_adapter_handles.push(config_reloader.spawn_read_through(
            "media.transcode",
            Some(Arc::clone(&metrics)),
            |c: &Config| c.media.transcode.max_concurrent_calls,
            move |new_cap: &usize| {
                budget.set_max_concurrent(*new_cap);
                tracing::info!(
                    max_concurrent_calls = *new_cap,
                    "transcode CpuBudget cap reconfigured"
                );
            },
        ));
    }

    // `media.prompts.capacity` → `PromptLibrary::resize`. Only
    // wires when the library itself is configured (non-empty
    // `root`); otherwise there's nothing to resize.
    if let Some(library) = prompt_library {
        reload_adapter_handles.push(config_reloader.spawn_read_through(
            "media.prompts.capacity",
            Some(Arc::clone(&metrics)),
            |c: &Config| c.media.prompts.capacity,
            move |new_cap: &usize| {
                library.resize(*new_cap);
                tracing::info!(capacity = *new_cap, "prompt library resized");
            },
        ));
    }

    // `ai.*_api_key` → `AiRegistry::set_env` / `clear_env`. Next
    // sidecar respawn picks up the rotated value; already-live
    // sidecars keep their old env until they're reloaded (see
    // `AiRegistry::env_snapshot` comment).
    for (field, env_key) in [
        ("ai.openai_api_key", "OPENAI_API_KEY"),
        ("ai.anthropic_api_key", "ANTHROPIC_API_KEY"),
    ] {
        let reg = ai_registry.clone();
        let extract: fn(&Config) -> Option<String> = match env_key {
            "OPENAI_API_KEY" => |c: &Config| c.ai.openai_api_key.clone(),
            _ => |c: &Config| c.ai.anthropic_api_key.clone(),
        };
        reload_adapter_handles.push(config_reloader.spawn_read_through(
            field,
            Some(Arc::clone(&metrics)),
            extract,
            move |val: &Option<String>| {
                match val {
                    Some(v) => reg.set_env(env_key, v.clone()),
                    None => reg.clear_env(env_key),
                }
                tracing::info!(%field, %env_key,
                    "AI credential snapshot rotated; next sidecar respawn inherits");
            },
        ));
    }

    // Seed the registry's env snapshot from boot config so the
    // invariant "snapshot reflects current config" holds on the
    // very first sidecar spawn, before any reload has fired.
    if let Some(v) = config_snapshot.ai.openai_api_key.as_ref() {
        ai_registry.set_env("OPENAI_API_KEY", v.clone());
    }
    if let Some(v) = config_snapshot.ai.anthropic_api_key.as_ref() {
        ai_registry.set_env("ANTHROPIC_API_KEY", v.clone());
    }

    // Slice 5.8-c: POSIX SIGHUP reloads the config file through
    // the same `Config::load` + `Config::validate` +
    // `ConfigReloader::apply` path the MCP `put_config` tool will
    // use. The deadline comes from `[canary] deadline_s`; the
    // 5.9 error-rate probe watches the same canary window and
    // fires early-rollback if the new config trips a ceiling.
    let reload_driver_handle: Option<JoinHandle<()>> = if cli.run.no_reload_signal {
        info!("--no-reload-signal set; SIGHUP reload disabled");
        None
    } else if let Some(mut hup) = hangup_stream() {
        let path = cli.run.config.clone();
        let reloader_arc = Arc::clone(&config_reloader);
        let metrics_arc = Arc::clone(&metrics);
        let cancel = shutdown.token();
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    sig = hup.recv() => {
                        if sig.is_none() { return; }
                    }
                }
                info!(path = %path.display(), "SIGHUP received; reloading config");
                match Config::load(&path) {
                    Ok(candidate) => {
                        if let Err(e) = candidate.validate() {
                            warn!(%e, "SIGHUP reload: validation failed; prior config kept");
                            continue;
                        }
                        let deadline = candidate.canary.deadline_s;
                        let probe_cfg = ProbeConfig::from(&candidate.canary);
                        match reloader_arc.apply(candidate, deadline).await {
                            Ok(receipt) if receipt.report.is_noop() => {
                                info!(%receipt.id, "SIGHUP reload: no-op");
                            }
                            Ok(receipt) => {
                                info!(
                                    %receipt.id,
                                    reloaded = ?receipt.report.reloaded,
                                    deadline_secs = deadline,
                                    "SIGHUP reload: canary window armed"
                                );
                                spawn_canary_watchdogs(
                                    Arc::clone(&reloader_arc),
                                    Arc::clone(&metrics_arc),
                                    &receipt,
                                    probe_cfg,
                                );
                            }
                            Err(e) => warn!(?e, "SIGHUP reload: apply rejected"),
                        }
                    }
                    Err(e) => warn!(?e, "SIGHUP reload: load failed"),
                }
            }
        }))
    } else {
        info!("SIGHUP reload unavailable on this platform");
        None
    };

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
    let mcp_stdio_task: Option<JoinHandle<()>> = if cli.run.mcp == Some(McpMode::Stdio) {
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
                webrtc_rendezvous.clone(),
                /* build_uac */ false,
                std::mem::take(&mut initial_dialogs),
                Arc::clone(&replicator),
                dialogs_shared.as_ref().map(Arc::clone),
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
                Arc::clone(&replicator),
                dialogs_shared.as_ref().map(Arc::clone),
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
                Arc::clone(&replicator),
                dialogs_shared.as_ref().map(Arc::clone),
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

    // ---- WebRTC signaling adapter (slice 5.10-followup) ----
    // Off by default. When enabled, opens a plain-HTTP WebSocket at
    // [webrtc] ws_bind and routes /smiths/webrtc upgrades through
    // the CLI-side WebRtcSessionHandler → SdpNegotiator chain.
    //
    // Slice 5.10-dtls + 5.10-bridge: the handler owns a
    // fresh DTLS-SRTP cert minted at boot + the shared media
    // fabric, so DTLS-SRTP offers complete a real handshake and
    // paired sessions (via their `tag`) get a live `MediaFabric::bridge`.
    // ICE / NAT traversal is still a follow-on — peer address
    // comes from the offer's `c=` line.
    if let Some(handler) = webrtc_handler.clone() {
        let bind = config.webrtc.ws_bind;
        // Slice 5.11-privacy: hot-reload `[webrtc.privacy]` via
        // the generic 5.8-b read-through adapter. Mode flips +
        // redaction-key rotation take effect on the next offer
        // without restarting the engine.
        let privacy_handle = handler.privacy_handle();
        reload_adapter_handles.push(config_reloader.spawn_read_through(
            "webrtc.privacy",
            Some(Arc::clone(&metrics)),
            |c: &Config| c.webrtc.privacy.clone(),
            move |new_cfg: &smiths_core::WebRtcPrivacyConfig| {
                let mut guard = privacy_handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = new_cfg.clone();
                tracing::info!(mode = ?new_cfg.mode, "webrtc.privacy reloaded");
            },
        ));

        let cancel = shutdown.token();
        adapter_handles.push(tokio::spawn(async move {
            if let Err(e) = webrtc::serve_webrtc(bind, handler, cancel).await {
                warn!(%bind, ?e, "WebRTC WebSocket server error");
            }
        }));
        if !config.webrtc.tls_cert.is_empty() || !config.webrtc.tls_key.is_empty() {
            warn!(
                "webrtc.tls_cert/tls_key are set but this adapter binds plaintext; \
                 front the engine with a TLS terminator (nginx / Caddy / Envoy) for wss://. \
                 See docs/deployment/webrtc.md."
            );
        }
    }

    // ---- Embedded TURN server (slice 5.11-turn) ----
    // Off by default. When `[webrtc.turn] external_url` is set,
    // the embedded server is skipped in favour of handing
    // clients the external URL.
    if config.webrtc.turn.enabled && config.webrtc.turn.external_url.is_empty() {
        let turn_cfg = smiths_ice::TurnServerConfig {
            bind: config.webrtc.turn.bind,
            realm: if config.webrtc.turn.realm.is_empty() {
                "smiths-turn".to_owned()
            } else {
                config.webrtc.turn.realm.clone()
            },
            relay_ip: config
                .webrtc
                .turn
                .relay_ip
                .unwrap_or_else(|| config.webrtc.turn.bind.ip()),
            allocation_lifetime: std::time::Duration::from_secs(u64::from(
                config.webrtc.turn.allocation_lifetime_s,
            )),
            credentials: config
                .webrtc
                .turn
                .credentials
                .iter()
                .map(|c| {
                    smiths_ice::LongTermCredential::new(
                        &c.username,
                        if config.webrtc.turn.realm.is_empty() {
                            "smiths-turn"
                        } else {
                            &config.webrtc.turn.realm
                        },
                        &c.password,
                    )
                })
                .collect(),
        };
        if turn_cfg.credentials.is_empty() {
            warn!(
                bind = %turn_cfg.bind,
                "webrtc.turn.enabled but credentials list is empty; every Allocate will 401. \
                 Add `[[webrtc.turn.credentials]]` entries or disable the server."
            );
        }
        let server =
            Arc::new(smiths_ice::TurnServer::new(turn_cfg).with_metrics(Arc::clone(&metrics)));
        let cancel = shutdown.token();
        adapter_handles.push(tokio::spawn(async move {
            if let Err(e) = server.run(cancel).await {
                warn!(?e, "TURN server exited with error");
            }
        }));
    } else if config.webrtc.turn.enabled && !config.webrtc.turn.external_url.is_empty() {
        info!(
            url = %config.webrtc.turn.external_url,
            "webrtc.turn.external_url set; embedded TURN server skipped"
        );
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
    // Slice 5.8-b: drop the reloader so every watch::Sender
    // closes; the read-through adapter tasks exit their
    // `.changed()` loop and we reap their handles cleanly.
    drop(config_reloader);
    for h in reload_adapter_handles {
        if let Err(err) = h.await {
            warn!(?err, "config read-through adapter panicked during shutdown");
        }
    }
    if let Some(h) = reload_driver_handle
        && let Err(err) = h.await
    {
        warn!(?err, "config reload driver panicked during shutdown");
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

/// Validate the config file and exit with the outcome code
/// (slice 5.8-c Small 1). Kept as a plain sync function so the
/// subcommand returns without spinning up the tokio runtime's
/// full subsystem wiring.
///
/// Exit codes match the spec: `0` = clean, `1` = parse / I/O /
/// env-extraction failure, `2` = a semantic invariant tripped.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn run_validate(path: &Path) {
    match Config::load(path) {
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        Ok(cfg) => match cfg.validate() {
            Ok(()) => {
                println!("{}: ok", path.display());
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        },
    }
}

/// Trigger a reload on a running engine or pre-flight the
/// configured file (slice 5.8-c Small 2). `--dry-run` does the
/// load + validate + `ApplyReport`-against-defaults diff and
/// exits without signalling. Otherwise a `SIGHUP` is sent to
/// the PID from `--pid` / `SMITHS_PID`; the running engine
/// handles the actual apply through its own reload driver.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn run_reload(path: &Path, args: &ReloadArgs) -> anyhow::Result<()> {
    let candidate = Config::load(path)
        .with_context(|| format!("loading candidate config from {}", path.display()))?;
    candidate
        .validate()
        .map_err(|e| anyhow::anyhow!("candidate config failed validation: {e}"))?;

    if args.diff || args.dry_run {
        // The subcommand doesn't know the target engine's live
        // config — it prints the diff against the shipped
        // defaults so operators see every customised field.
        // Live-diff against a running engine is the MCP
        // `put_config(dry_run=true)` path in slice 7.3.
        let report = Config::default().apply_report(&candidate);
        println!("--- candidate vs. defaults ---");
        for field in &report.reloaded {
            println!("reloadable: {field}");
        }
        for field in &report.restart_required {
            println!("restart-required: {field}");
        }
        if report.is_noop() {
            println!("(no differences from defaults)");
        }
    }
    if args.dry_run {
        return Ok(());
    }

    let Some(pid) = args.pid else {
        anyhow::bail!(
            "smiths-net reload needs --pid N (or SMITHS_PID env) to signal the running engine; \
             use --dry-run for a signal-free pre-flight"
        );
    };
    send_hup(pid)?;
    if let Some(secs) = args.canary_secs {
        eprintln!(
            "note: --canary-secs {secs} is informational; the running engine uses \
             its own [canary] deadline_s for the SIGHUP apply"
        );
    }
    println!(
        "sent SIGHUP to pid {pid}; target reloads from {}",
        path.display()
    );
    Ok(())
}

/// POSIX `kill(pid, SIGHUP)`. Wrapped in a thin `rustix`-backed
/// helper so the subcommand stays clean of raw libc.
#[cfg(unix)]
fn send_hup(pid: i32) -> anyhow::Result<()> {
    use rustix::process::{Pid, Signal, kill_process};
    let Some(p) = (if pid <= 0 { None } else { Pid::from_raw(pid) }) else {
        anyhow::bail!("refusing to signal pid {pid}: must be > 0");
    };
    kill_process(p, Signal::HUP).map_err(|e| anyhow::anyhow!("kill(pid={pid}, SIGHUP): {e}"))?;
    Ok(())
}

#[cfg(not(unix))]
fn send_hup(_pid: i32) -> anyhow::Result<()> {
    anyhow::bail!(
        "smiths-net reload: non-Unix platforms have no SIGHUP; use MCP put_config instead"
    );
}

/// Arm the 5.9 canary watchdogs on a freshly-applied receipt:
/// the deadline timer (substrate 5.8-mvp) + the error-rate
/// probe (5.9-followup). A shared `CancellationToken` ensures
/// whichever arm resolves first (operator confirm, deadline,
/// probe trip) ends the other two — no orphaned tasks.
fn spawn_canary_watchdogs(
    reloader: Arc<ConfigReloader>,
    metrics: Arc<Metrics>,
    receipt: &ChangeReceipt,
    probe_cfg: ProbeConfig,
) {
    let cancel = CancellationToken::new();
    // Deadline timer — the reloader already handles the rollback
    // side; we only need to wire cancellation so the probe stops
    // when the timer wins the race.
    let timer_cancel = cancel.clone();
    let deadline_handle = reloader.spawn_auto_rollback(receipt, Some(Arc::clone(&metrics)));
    tokio::spawn(async move {
        let _ = deadline_handle.await;
        timer_cancel.cancel();
    });
    // Error-rate probe. The returned handle is detached — we
    // don't need the verdict directly; rollback metrics +
    // tracing already narrate the outcome. Assign to
    // `_probe_handle` (not `_`) to dodge
    // `clippy::let_underscore_future` without leaking a
    // drop-on-cancel surprise.
    let probe = ErrorRateProbe::new(metrics, probe_cfg);
    let _probe_handle = probe.spawn(reloader, receipt, cancel);
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
        let mut ticker = tokio::time::interval(std::time::Duration::from_hours(1));
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
/// Build the WebRTC signaling handler (slice 5.10-bridge +
/// 5.10-dtls + 5.10-ice + 5.11-privacy) without starting its
/// WebSocket server. The handler is also handed to the SIP
/// UAS as an `Arc<dyn WebRtcRendezvous>` (slice 5.10-sipjoin)
/// so SIP INVITEs carrying `X-Smiths-Webrtc-Tag:` can bridge
/// against a parked WebRTC partner. The server is spawned
/// later next to the other control-plane adapters.
fn build_webrtc_handler(config: &Config, metrics: &Arc<Metrics>) -> Arc<webrtc::CliWebRtcHandler> {
    let bind = config.webrtc.ws_bind;
    // One cert per engine instance, minted at boot.
    let webrtc_dtls_cert = match smiths_core::SelfSignedCert::generate("smiths-net-webrtc") {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            warn!(
                ?e,
                "minting WebRTC DTLS cert failed; DTLS-SRTP offers will be rejected"
            );
            None
        }
    };
    // Dedicated fabric for the WebRTC adapter so its endpoint
    // pool doesn't share with SIP.
    let webrtc_fabric: Arc<UdpMediaFabric> =
        Arc::new(UdpMediaFabric::new().with_metrics(Arc::clone(metrics)));
    let mut negotiator_builder = Negotiator::with_default_codecs(bind.ip());
    if let Some(cert) = webrtc_dtls_cert.as_ref() {
        negotiator_builder = negotiator_builder.with_dtls_cert(Arc::clone(cert));
    }
    negotiator_builder = negotiator_builder.with_ice_enabled(config.webrtc.ice.enabled);
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(negotiator_builder);
    let mut handler =
        webrtc::CliWebRtcHandler::new(negotiator, webrtc_fabric, bind.ip(), 50_000, 1_000)
            .with_metrics(Arc::clone(metrics))
            .with_privacy(config.webrtc.privacy.clone());
    if let Some(cert) = webrtc_dtls_cert.as_ref() {
        handler = handler.with_dtls_cert(Arc::clone(cert));
    }
    Arc::new(handler)
}

#[allow(clippy::too_many_arguments)] // CLI wiring helper: one arg per subsystem the UAS composes
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
    webrtc_rendezvous: Option<Arc<dyn smiths_core::WebRtcRendezvous>>,
    build_uac: bool,
    restore_dialogs: Vec<smiths_core::DialogRecord>,
    replicator: Arc<dyn smiths_core::Replicator>,
    dialogs_shared: Option<
        Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    >,
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
    .with_rate_limit(rate_limit.clone())
    .with_replicator(replicator);
    if let Some(d) = dialogs_shared {
        server = server.with_dialogs(d);
    }
    if let Some(reg) = registrar {
        server = server.with_registrar(reg);
    }
    if let Some(rdv) = webrtc_rendezvous {
        server = server.with_webrtc_rendezvous(rdv);
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
    replicator: Arc<dyn smiths_core::Replicator>,
    dialogs_shared: Option<
        Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    >,
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
        .with_rate_limit(rate_limit)
        .with_replicator(replicator);
    if let Some(d) = dialogs_shared {
        server = server.with_dialogs(d);
    }
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
    replicator: Arc<dyn smiths_core::Replicator>,
    dialogs_shared: Option<
        Arc<dashmap::DashMap<smiths_core::DialogKey, smiths_core::DialogRecord>>,
    >,
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
        .with_rate_limit(rate_limit)
        .with_replicator(replicator);
    if let Some(d) = dialogs_shared {
        server = server.with_dialogs(d);
    }
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

fn init_tracing(level: &str, format: LogFormat, mcp_stdio: bool) -> anyhow::Result<LogReloader> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .or_else(|_| EnvFilter::try_new("info"))
        .context("constructing tracing EnvFilter")?;

    // Slice 5.8-b: wrap the `EnvFilter` in a `reload::Layer` so
    // the `observability.log_level` read-through adapter can
    // live-swap the filter when the config reloader publishes a
    // new value. The handle is plugged into a `Box<dyn Fn>`
    // closure so the CLI can store the reloader without naming
    // the subscriber's full `Layered<…>` type.
    let (filter_layer, filter_handle) = reload::Layer::new(filter);

    let registry = tracing_subscriber::registry().with(filter_layer);
    // In MCP stdio mode, stdout is the JSON-RPC wire; divert logs to
    // stderr regardless of the configured format.
    if mcp_stdio {
        registry
            .with(fmt::layer().with_writer(std::io::stderr))
            .init();
    } else {
        match format {
            LogFormat::Json => registry.with(fmt::layer().json()).init(),
            LogFormat::Pretty => registry.with(fmt::layer()).init(),
        }
    }

    // Swap in a fresh `EnvFilter` on reload. `EnvFilter::try_new`
    // rejects unparseable directives before the swap so a bad
    // config doesn't take out logging.
    let reloader: LogReloader = Box::new(move |new_level: &str| -> Result<(), String> {
        let new_filter = EnvFilter::try_new(new_level).map_err(|e| e.to_string())?;
        filter_handle.reload(new_filter).map_err(|e| e.to_string())
    });
    Ok(reloader)
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
