//! `smiths-net init` config wizard (slice 7.1).
//!
//! Generates a valid `config.toml` by walking operators through
//! interactive prompts with sensible defaults. Supports:
//!
//! - `--preset dev` / `--preset prod` to pre-fill all values.
//! - `--non-interactive` to skip prompts (uses preset or defaults).
//! - Round-trip validation: the generated TOML is loaded back through
//!   `Config::load` before exit.

#![allow(clippy::print_stdout)] // CLI wizard output is intentionally on stdout.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, ValueEnum};
use smiths_core::Config;
use smiths_core::config::{AuthBackend, LogFormat, SandboxConfig, SeccompPolicy, SipTransport};

/// CLI args for `smiths-net init`.
#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Output path for the generated config file.
    #[arg(long, short, default_value = "config.toml")]
    pub output: PathBuf,

    /// Configuration preset — pre-fills every field with opinionated
    /// defaults so `--non-interactive` produces a usable config
    /// without any prompts.
    #[arg(long, value_enum)]
    pub preset: Option<Preset>,

    /// Skip all interactive prompts and use the preset (or default)
    /// values directly. Required when stdin is not a TTY.
    #[arg(long, default_value_t = false)]
    pub non_interactive: bool,

    /// Overwrite the output file if it already exists.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

/// Pre-baked configuration presets.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub(crate) enum Preset {
    /// Relaxed development defaults: debug logging, pretty format,
    /// no TLS, no auth, no sandbox.
    Dev,
    /// Hardened production defaults: info logging, JSON format,
    /// TLS enabled (paths prompted), sandbox with `no_new_privs`,
    /// seccomp allowlist, conservative rlimits.
    Prod,
}

/// Run the init wizard.
pub(crate) fn run(args: &InitArgs) -> anyhow::Result<()> {
    if args.output.exists() && !args.force {
        anyhow::bail!(
            "output file {} already exists; use --force to overwrite",
            args.output.display()
        );
    }

    let config = if args.non_interactive {
        build_non_interactive(args.preset)
    } else {
        build_interactive(args.preset)?
    };

    // Serialize to TOML.
    let toml_string =
        toml::to_string_pretty(&config).context("failed to serialize config to TOML")?;

    // Write to disk.
    if let Some(parent) = args.output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(&args.output, &toml_string)
        .with_context(|| format!("writing {}", args.output.display()))?;

    // Round-trip: load and validate.
    let loaded = Config::load(&args.output)
        .with_context(|| format!("round-trip load of {}", args.output.display()))?;
    if let Err(e) = loaded.validate() {
        let _ = std::fs::remove_file(&args.output);
        anyhow::bail!("generated config failed validation: {e}");
    }

    println!("✓ Config written to {}", args.output.display());
    println!(
        "  Run `smiths-net --config {}` to start the engine.",
        args.output.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Non-interactive path
// ---------------------------------------------------------------------------

fn build_non_interactive(preset: Option<Preset>) -> Config {
    let mut config = Config::default();
    match preset {
        Some(Preset::Prod) => apply_prod_preset(&mut config),
        Some(Preset::Dev) | None => apply_dev_preset(&mut config),
    }
    config
}

// ---------------------------------------------------------------------------
// Interactive path
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)] // wizard prompt sequence is one logical flow
fn build_interactive(preset: Option<Preset>) -> anyhow::Result<Config> {
    let mut config = Config::default();

    // Seed from preset if provided.
    match preset {
        Some(Preset::Dev) => apply_dev_preset(&mut config),
        Some(Preset::Prod) => apply_prod_preset(&mut config),
        None => {}
    }

    println!();
    println!("  🛠  smiths-net configuration wizard");
    println!("  ───────────────────────────────────");
    println!("  Press Enter to accept the [default] value.");
    println!();

    prompt_sip(&mut config)?;
    prompt_auth(&mut config)?;
    prompt_observability(&mut config)?;
    prompt_webrtc(&mut config)?;
    prompt_sandbox(&mut config)?;
    prompt_cluster(&mut config)?;

    Ok(config)
}

// ---------------------------------------------------------------------------
// Per-section interactive helpers
// ---------------------------------------------------------------------------

fn prompt_sip(config: &mut Config) -> anyhow::Result<()> {
    use dialoguer::{Input, Select};

    let bind: String = Input::new()
        .with_prompt("SIP bind address")
        .default(
            config
                .sip
                .bind
                .first()
                .map_or_else(|| "0.0.0.0:5060".to_owned(), ToString::to_string),
        )
        .interact_text()?;
    config.sip.bind = vec![bind.parse().context("invalid SIP bind address")?];

    let transport_choices = &["udp", "tcp", "tls", "udp + tcp", "udp + tcp + tls"];
    let default_idx = u8::from(config.sip.transports.contains(&SipTransport::Tls))
        .wrapping_mul(4)
        .max(u8::from(config.sip.transports.contains(&SipTransport::Tcp)).wrapping_mul(3));
    let transport_sel = Select::new()
        .with_prompt("SIP transports")
        .items(transport_choices)
        .default(usize::from(default_idx))
        .interact()?;
    config.sip.transports = match transport_sel {
        0 => vec![SipTransport::Udp],
        1 => vec![SipTransport::Tcp],
        2 => vec![SipTransport::Tls],
        3 => vec![SipTransport::Udp, SipTransport::Tcp],
        _ => vec![SipTransport::Udp, SipTransport::Tcp, SipTransport::Tls],
    };

    if config.sip.transports.contains(&SipTransport::Tls) {
        let cert: String = Input::new()
            .with_prompt("TLS certificate path")
            .default(config.sip.tls_cert_path.as_ref().map_or_else(
                || "/etc/smiths/tls/cert.pem".to_owned(),
                |p| p.display().to_string(),
            ))
            .interact_text()?;
        config.sip.tls_cert_path = Some(PathBuf::from(cert));

        let key: String = Input::new()
            .with_prompt("TLS private key path")
            .default(config.sip.tls_key_path.as_ref().map_or_else(
                || "/etc/smiths/tls/key.pem".to_owned(),
                |p| p.display().to_string(),
            ))
            .interact_text()?;
        config.sip.tls_key_path = Some(PathBuf::from(key));
    }
    Ok(())
}

fn prompt_auth(config: &mut Config) -> anyhow::Result<()> {
    use dialoguer::Select;

    let auth_choices = &["none", "sqlite", "http"];
    let auth_default = match config.auth.backend {
        AuthBackend::Sqlite => 1,
        AuthBackend::Http => 2,
        AuthBackend::None => 0,
    };
    let auth_sel = Select::new()
        .with_prompt("Auth backend")
        .items(auth_choices)
        .default(auth_default)
        .interact()?;
    config.auth.backend = match auth_sel {
        1 => AuthBackend::Sqlite,
        2 => AuthBackend::Http,
        _ => AuthBackend::None,
    };
    Ok(())
}

fn prompt_observability(config: &mut Config) -> anyhow::Result<()> {
    use dialoguer::{Input, Select};

    let log_choices = &["info", "debug", "warn", "trace", "error"];
    let log_default = match config.observability.log_level.as_str() {
        "debug" => 1,
        "warn" => 2,
        "trace" => 3,
        "error" => 4,
        _ => 0,
    };
    let log_sel = Select::new()
        .with_prompt("Log level")
        .items(log_choices)
        .default(log_default)
        .interact()?;
    config.observability.log_level = log_choices[log_sel].to_string();

    let fmt_choices = &["json", "pretty"];
    let fmt_default = usize::from(config.observability.log_format == LogFormat::Pretty);
    let fmt_sel = Select::new()
        .with_prompt("Log format")
        .items(fmt_choices)
        .default(fmt_default)
        .interact()?;
    config.observability.log_format = if fmt_sel == 1 {
        LogFormat::Pretty
    } else {
        LogFormat::Json
    };

    let health: String = Input::new()
        .with_prompt("Health endpoint bind")
        .default(config.observability.health_bind.to_string())
        .interact_text()?;
    config.observability.health_bind = health.parse().context("invalid health bind address")?;
    Ok(())
}

fn prompt_webrtc(config: &mut Config) -> anyhow::Result<()> {
    use dialoguer::Confirm;
    config.webrtc.enabled = Confirm::new()
        .with_prompt("Enable WebRTC/ICE?")
        .default(config.webrtc.enabled)
        .interact()?;
    Ok(())
}

fn prompt_sandbox(config: &mut Config) -> anyhow::Result<()> {
    use dialoguer::Select;

    let sandbox_choices = &[
        "off",
        "basic (no_new_privs)",
        "strict (no_new_privs + seccomp)",
    ];
    let sandbox_default = if config.plugins.sandbox.seccomp == SeccompPolicy::Allowlist {
        2
    } else {
        usize::from(config.plugins.sandbox.no_new_privs)
    };
    let sandbox_sel = Select::new()
        .with_prompt("Plugin sandbox")
        .items(sandbox_choices)
        .default(sandbox_default)
        .interact()?;
    match sandbox_sel {
        1 => {
            config.plugins.sandbox.no_new_privs = true;
        }
        2 => {
            apply_strict_sandbox(&mut config.plugins.sandbox);
        }
        _ => {
            config.plugins.sandbox = SandboxConfig::default();
        }
    }
    Ok(())
}

fn prompt_cluster(config: &mut Config) -> anyhow::Result<()> {
    use dialoguer::Select;

    let cluster_choices = &["standalone", "primary", "secondary"];
    let cluster_default = match config.cluster.mode {
        smiths_core::ClusterMode::Primary => 1,
        smiths_core::ClusterMode::Secondary => 2,
        smiths_core::ClusterMode::Standalone => 0,
    };
    let cluster_sel = Select::new()
        .with_prompt("HA cluster mode")
        .items(cluster_choices)
        .default(cluster_default)
        .interact()?;
    config.cluster.mode = match cluster_sel {
        1 => smiths_core::ClusterMode::Primary,
        2 => smiths_core::ClusterMode::Secondary,
        _ => smiths_core::ClusterMode::Standalone,
    };
    Ok(())
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

fn apply_dev_preset(config: &mut Config) {
    "debug".clone_into(&mut config.observability.log_level);
    config.observability.log_format = LogFormat::Pretty;
    config.observability.health_bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
    config.sip.transports = vec![SipTransport::Udp];
    config.auth.backend = AuthBackend::None;
    config.webrtc.enabled = false;
    config.plugins.sandbox = SandboxConfig::default();
    config.cluster.mode = smiths_core::ClusterMode::Standalone;
}

fn apply_prod_preset(config: &mut Config) {
    "info".clone_into(&mut config.observability.log_level);
    config.observability.log_format = LogFormat::Json;
    config.observability.health_bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);
    config.sip.transports = vec![SipTransport::Udp, SipTransport::Tcp, SipTransport::Tls];
    config.sip.tls_cert_path = Some(PathBuf::from("/etc/smiths/tls/cert.pem"));
    config.sip.tls_key_path = Some(PathBuf::from("/etc/smiths/tls/key.pem"));
    config.auth.backend = AuthBackend::None;
    config.webrtc.enabled = false;
    config.plugins.sandbox = SandboxConfig::default();
    apply_strict_sandbox(&mut config.plugins.sandbox);
    config.cluster.mode = smiths_core::ClusterMode::Standalone;
}

fn apply_strict_sandbox(sandbox: &mut SandboxConfig) {
    sandbox.no_new_privs = true;
    sandbox.seccomp = SeccompPolicy::Allowlist;
    sandbox.max_fds = Some(256);
    sandbox.max_memory_bytes = Some(512 * 1024 * 1024); // 512 MiB
    sandbox.max_cpu_seconds = Some(60);
    sandbox.max_processes = Some(0);
}
