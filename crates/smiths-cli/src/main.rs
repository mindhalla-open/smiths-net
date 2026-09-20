//! smiths-net binary entry point: CLI parsing, config subcommands,
//! and the hand-off to [`engine::run`].

mod engine;
mod ha;
mod http;
mod ice_driver;
mod init;
mod logging;
mod reload_driver;
mod replication_service;
mod retention;
mod shutdown;
mod sip_spawn;
mod transcode;
mod webrtc;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand, ValueEnum};
use smiths_core::Config;
use tracing::info;

/// Which transport to run MCP on. Additive to SIP / health / A2A,
/// which all come from `config` as usual. stdin EOF terminates the
/// process.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
enum McpMode {
    /// Run MCP over stdio alongside any other enabled subsystems.
    /// Logs are routed to stderr so stdout stays on the JSON-RPC wire.
    Stdio,
}

/// Protocol / feature advertisement for `--version`. The
/// `wireguard`, `sip-quic` and `mcp-http3` Cargo features reserve
/// the config surface only; no build ships their runtime.
#[cfg(feature = "wireguard")]
const WG_HINT: &str = " + wireguard (config only)";
#[cfg(not(feature = "wireguard"))]
const WG_HINT: &str = "";

#[cfg(feature = "sip-quic")]
const QUIC_HINT: &str = ", quic (config only)";
#[cfg(not(feature = "sip-quic"))]
const QUIC_HINT: &str = "";

#[cfg(feature = "mcp-http3")]
const H3_HINT: &str = ", h3 (config only)";
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
    "ai: dispatcher, ollama/llamacpp/openai/anthropic refs, whisper.cpp ref, piper ref",
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
    /// Run options shared between "run the engine" and the config
    /// subcommands, so `smiths-net --config foo validate` and plain
    /// `smiths-net --config foo` both accept `--config`.
    #[command(flatten)]
    run: RunArgs,

    /// Config-management subcommands. No subcommand = run the engine.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Shared flags for "run the engine" + every subcommand.
#[derive(Debug, Args)]
struct RunArgs {
    /// Path to the TOML config file. The file must exist: a missing
    /// file is an error, never a fallback to defaults.
    #[arg(long, env = "SMITHS_CONFIG", default_value = "examples/config.toml")]
    config: PathBuf,

    /// Override `observability.log_level` (`RUST_LOG` still takes precedence).
    #[arg(long, env = "SMITHS_LOG")]
    log: Option<String>,

    /// Also serve MCP on the chosen transport. Logs move to stderr so
    /// stdout stays a clean JSON-RPC wire.
    #[arg(long, value_enum)]
    mcp: Option<McpMode>,

    /// HA snapshot file path. When set, every dialog record in it is
    /// restored at startup and the live dialog table is written back
    /// on graceful shutdown. Unset = cold boot every time.
    #[arg(long, env = "SMITHS_SNAPSHOT")]
    snapshot_path: Option<PathBuf>,

    /// Ignore POSIX SIGHUP entirely. `[reload] enabled = false`
    /// achieves the same from the config file.
    #[arg(long, default_value_t = false)]
    no_reload_signal: bool,
}

/// Subcommands. Absent = run the engine.
#[derive(Debug, Subcommand)]
enum Command {
    /// Load + validate the config without starting the engine. Exit
    /// codes: `0` clean, `1` parse / I/O error (including a missing
    /// file), `2` semantic error (a cross-field invariant tripped).
    Validate,
    /// Hot-reload a running engine's config through the same
    /// `Config::load` + `Config::validate` + `ConfigReloader::apply`
    /// path SIGHUP drives. With `--dry-run`, skips the signal and
    /// prints the diff against defaults.
    Reload(ReloadArgs),
    /// Generate a valid config.toml through an interactive wizard or
    /// a preset (`--non-interactive --preset prod` for scripts).
    Init(init::InitArgs),
}

#[derive(Debug, Args)]
struct ReloadArgs {
    /// PID of the running `smiths-net` process to SIGHUP. Read from
    /// `SMITHS_PID` when omitted. Required unless `--dry-run`.
    #[arg(long, env = "SMITHS_PID")]
    pid: Option<i32>,
    /// Print the `ApplyReport` (diff against defaults) before
    /// signalling.
    #[arg(long, default_value_t = false)]
    diff: bool,
    /// Stop after load + validate + diff — no signal, no mutation.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Validate) => {
            run_validate(&cli.run.config);
            return Ok(());
        }
        Some(Command::Reload(ref args)) => return run_reload(&cli.run.config, args),
        Some(Command::Init(ref args)) => return init::run(args),
        None => {}
    }

    let config = Config::load(&cli.run.config)
        .with_context(|| format!("loading config from {}", cli.run.config.display()))?;
    config
        .validate_with(&reload_driver::build_support())
        .map_err(|e| anyhow::anyhow!("config validation failed: {e}"))?;

    let level = cli
        .run
        .log
        .as_deref()
        .unwrap_or(&config.observability.log_level);
    let log_reloader = logging::init_tracing(
        level,
        config.observability.log_format,
        cli.run.mcp.is_some(),
    )?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.run.config.display(),
        "smiths-net starting"
    );

    let opts = engine::RunOptions {
        config_path: cli.run.config,
        mcp_stdio: cli.run.mcp == Some(McpMode::Stdio),
        snapshot_path: cli.run.snapshot_path,
        no_reload_signal: cli.run.no_reload_signal,
    };
    engine::run(opts, config, log_reloader).await
}

/// Validate the config file and exit with the outcome code. Plain
/// sync so the subcommand returns without any subsystem wiring.
#[allow(clippy::print_stdout, clippy::print_stderr)]
fn run_validate(path: &Path) {
    match Config::load(path) {
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        Ok(cfg) => match cfg.validate_with(&reload_driver::build_support()) {
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

/// Trigger a reload on a running engine or pre-flight the file.
/// `--dry-run` does load + validate + `ApplyReport`-against-defaults
/// and exits without signalling. Otherwise a SIGHUP goes to the PID
/// from `--pid` / `SMITHS_PID`; the engine applies through its own
/// reload driver, using its own `[canary] deadline_s`.
#[allow(clippy::print_stdout)]
fn run_reload(path: &Path, args: &ReloadArgs) -> anyhow::Result<()> {
    let candidate = Config::load(path)
        .with_context(|| format!("loading candidate config from {}", path.display()))?;
    candidate
        .validate_with(&reload_driver::build_support())
        .map_err(|e| anyhow::anyhow!("candidate config failed validation: {e}"))?;

    if args.diff || args.dry_run {
        // The subcommand doesn't know the target engine's live
        // config, so it diffs against the shipped defaults; the MCP
        // `put_config(dry_run=true)` tool diffs against the live one.
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
    println!(
        "sent SIGHUP to pid {pid}; target reloads from {}",
        path.display()
    );
    Ok(())
}

/// POSIX `kill(pid, SIGHUP)` through `rustix`.
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
