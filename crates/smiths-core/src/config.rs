//! Layered configuration: defaults → TOML file → `SMITHS__*` env vars.
//!
//! Keep the shape small and flat until concrete subsystems need
//! something. As new sections land (sip, media, plugins, mcp) they add
//! their own struct here and plug into [`Config`].

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

use crate::Error;

/// Root configuration loaded at startup.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Runtime-wide tuning knobs (thread pools, etc.).
    pub core: CoreConfig,
    /// Logging, health endpoint, metrics bind (metrics added later).
    pub observability: ObservabilityConfig,
    /// SIP signaling configuration.
    pub sip: SipConfig,
}

/// Core runtime tuning.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    /// Tokio worker threads. `0` means auto (number of CPUs).
    pub worker_threads: usize,
}

/// Observability config — logging and the health endpoint.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// `tracing-subscriber` env-filter directive (e.g. `info`, `debug,smiths_sip=trace`).
    pub log_level: String,
    /// Log output formatter.
    pub log_format: LogFormat,
    /// HTTP bind address for the `/health` endpoint.
    pub health_bind: SocketAddr,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
            log_format: LogFormat::Json,
            health_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
        }
    }
}

/// SIP signaling configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SipConfig {
    /// Socket addresses to bind for SIP signaling.
    pub bind: Vec<SocketAddr>,
    /// Enabled transports. Only `udp` is wired in Phase 1.
    pub transports: Vec<SipTransport>,
    /// Grace period to finish in-flight transactions on shutdown.
    pub drain_timeout_secs: u64,
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            bind: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5060)],
            transports: vec![SipTransport::Udp],
            drain_timeout_secs: 10,
        }
    }
}

/// Transport protocols enabled for SIP signaling.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SipTransport {
    /// RFC 3261 SIP over UDP.
    #[default]
    Udp,
    /// RFC 3261 SIP over TCP. Not yet wired in Phase 1.
    Tcp,
    /// RFC 5630 SIP over TLS. Not yet wired in Phase 1.
    Tls,
}

/// Format for `tracing-subscriber` output.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Machine-readable JSON — recommended in production.
    #[default]
    Json,
    /// Human-readable multi-line output — recommended for local dev.
    Pretty,
}

impl Config {
    /// Load config from `path`, layering in `SMITHS__*` env overrides.
    ///
    /// Missing files are tolerated — the returned config falls back to
    /// defaults plus env. Unknown keys in the TOML are rejected.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let fig = Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("SMITHS__").split("__"));

        fig.extract().map_err(|e| Error::Config(e.to_string()))
    }

    /// Build a config solely from defaults + env (no file).
    pub fn from_env() -> Result<Self, Error> {
        Figment::from(Serialized::defaults(Self::default()))
            .merge(Env::prefixed("SMITHS__").split("__"))
            .extract()
            .map_err(|e| Error::Config(e.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Error is >200 B; irrelevant in tests
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.core.worker_threads, 0);
        assert_eq!(c.observability.log_level, "info");
        assert_eq!(c.observability.log_format, LogFormat::Json);
        assert_eq!(c.observability.health_bind.port(), 8080);
    }

    #[test]
    fn env_overrides_defaults() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("SMITHS__OBSERVABILITY__LOG_LEVEL", "debug");
            jail.set_env("SMITHS__OBSERVABILITY__LOG_FORMAT", "pretty");
            jail.set_env("SMITHS__CORE__WORKER_THREADS", "4");

            let c = Config::from_env().unwrap();
            assert_eq!(c.observability.log_level, "debug");
            assert_eq!(c.observability.log_format, LogFormat::Pretty);
            assert_eq!(c.core.worker_threads, 4);
            Ok(())
        });
    }

    #[test]
    fn toml_file_is_merged() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [observability]
                log_level = "warn"
                health_bind = "0.0.0.0:9999"
                "#,
            )?;
            let c = Config::load(Path::new("config.toml")).unwrap();
            assert_eq!(c.observability.log_level, "warn");
            assert_eq!(c.observability.health_bind.port(), 9999);
            Ok(())
        });
    }
}
