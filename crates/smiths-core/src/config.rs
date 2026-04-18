//! Layered configuration: defaults → TOML file → `SMITHS__*` env vars.
//!
//! Keep the shape small and flat until concrete subsystems need
//! something. As new sections land (sip, media, plugins, mcp) they add
//! their own struct here and plug into [`Config`].

use std::fmt;
use std::net::{AddrParseError, IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::str::FromStr;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

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
    /// Addresses to bind for SIP signaling.
    pub bind: Vec<BindSpec>,
    /// Enabled transports. Only `udp` is wired in Phase 1.
    pub transports: Vec<SipTransport>,
    /// Grace period to finish in-flight transactions on shutdown.
    pub drain_timeout_secs: u64,
}

impl Default for SipConfig {
    fn default() -> Self {
        Self {
            bind: vec![BindSpec::Addr(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                5060,
            ))],
            transports: vec![SipTransport::Udp],
            drain_timeout_secs: 10,
        }
    }
}

/// A SIP bind target — today a resolved `SocketAddr`, tomorrow may
/// carry an interface name (`"eth0:5060"`, `"wg0:5060"`) resolved at
/// runtime. Keeping this as an open newtype — not a bare `SocketAddr`
/// — is the MVP guardrail for proxy/VPN transports (see
/// `docs/architecture/04-post-mvp-scope.md §11`).
///
/// Accepts any string that parses as `SocketAddr` today. Interface
/// syntax is reserved and returns a descriptive error pointing at the
/// post-MVP work item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindSpec {
    /// A concrete IP+port already resolved at config-load time.
    Addr(SocketAddr),
}

impl BindSpec {
    /// Resolve this spec to a concrete socket address for `bind()`.
    ///
    /// Infallible today — the `Addr` variant is the only one. Will
    /// grow an async resolver once interface-name support lands.
    #[must_use]
    pub const fn socket_addr(&self) -> SocketAddr {
        match self {
            Self::Addr(a) => *a,
        }
    }
}

impl fmt::Display for BindSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Addr(a) => fmt::Display::fmt(a, f),
        }
    }
}

impl FromStr for BindSpec {
    type Err = BindSpecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Today: only the `ip:port` form. Interface form (e.g. `eth0:5060`,
        // `wg0:5060`) is reserved; fail with a clear message until the
        // post-MVP work lands. Heuristic: if the left side of the last `:`
        // contains characters that cannot appear in an IP literal, assume
        // it's an interface name.
        match s.parse::<SocketAddr>() {
            Ok(a) => Ok(Self::Addr(a)),
            Err(parse_err) => {
                if looks_like_iface_spec(s) {
                    Err(BindSpecError::InterfaceUnsupported(s.to_owned()))
                } else {
                    Err(BindSpecError::Parse(parse_err))
                }
            }
        }
    }
}

impl Serialize for BindSpec {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BindSpec {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(de::Error::custom)
    }
}

/// Errors produced when parsing a [`BindSpec`] string.
#[derive(Debug, thiserror::Error)]
pub enum BindSpecError {
    /// The string isn't a valid `ip:port` literal.
    #[error("invalid socket address: {0}")]
    Parse(#[from] AddrParseError),
    /// Interface-name syntax (e.g. `wg0:5060`) is reserved for the
    /// post-MVP proxy/VPN work (see roadmap P16).
    #[error(
        "interface-name bind spec `{0}` is not yet supported \
         (reserved for proxy/VPN work — roadmap P16); \
         use an explicit `ip:port`"
    )]
    InterfaceUnsupported(String),
}

/// Heuristic: does the host part of `s` look like an interface name?
///
/// Interface names contain letters or `-` / `_` in a way that IPv4
/// literals cannot, and that IPv6 literals only inside `[...]`. We
/// split on the last `:` and inspect the host portion.
fn looks_like_iface_spec(s: &str) -> bool {
    let Some((host, _port)) = s.rsplit_once(':') else {
        return false;
    };
    if host.starts_with('[') {
        return false; // IPv6 literal
    }
    host.chars()
        .any(|c| c.is_ascii_alphabetic() || c == '-' || c == '_')
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
    fn bindspec_parses_ip_port() {
        let spec: BindSpec = "127.0.0.1:5060".parse().unwrap();
        assert_eq!(
            spec,
            BindSpec::Addr("127.0.0.1:5060".parse::<SocketAddr>().unwrap())
        );
        assert_eq!(spec.socket_addr().port(), 5060);
    }

    #[test]
    fn bindspec_rejects_interface_form_with_roadmap_hint() {
        let err = "wg0:5060".parse::<BindSpec>().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("wg0:5060"), "{msg}");
        assert!(msg.contains("P16"), "{msg}");
    }

    #[test]
    fn sip_bind_from_toml_string() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [sip]
                bind = ["0.0.0.0:5060", "127.0.0.1:5070"]
                "#,
            )?;
            let c = Config::load(Path::new("config.toml")).unwrap();
            assert_eq!(c.sip.bind.len(), 2);
            assert_eq!(c.sip.bind[1].socket_addr().port(), 5070);
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
