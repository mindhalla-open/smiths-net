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
    /// MCP control-plane server.
    pub mcp: McpConfig,
    /// A2A HTTP adapter.
    pub a2a: A2aConfig,
    /// Plugin loader settings.
    pub plugins: PluginsConfig,
    /// Auth / subscriber-DB configuration (P8, slice 2.1).
    pub auth: AuthConfig,
    /// Pluggable storage configuration (P23, slice 2.3). CDR + KV
    /// backends share this section; auth has its own `[auth]`
    /// because its lifetime + security story differs.
    pub storage: StorageConfig,
}

/// `[storage]` TOML block — CDR + KV backend selection.
///
/// ```toml
/// [storage]
/// backend = "sqlite"        # "none" | "sqlite"
///
/// [storage.sqlite]
/// path = "/var/lib/smiths-net/storage.db"
/// ```
///
/// When operators point `[auth.sqlite]` and `[storage.sqlite]` at
/// the same file the `SQLite` auth store serves both surfaces
/// (credentials + registrations + CDR + KV) from one DB — that's
/// the default the runbook recommends.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Which backend to wire up for `CdrStore` + `KvStore`.
    pub backend: StorageBackend,
    /// SQLite-specific settings. Ignored when `backend != "sqlite"`.
    pub sqlite: SqliteStorageConfig,
}

/// Storage backend selector.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// No CDR / KV persistence. Dialog terminates produce no CDR
    /// rows; `list_cdr` returns an empty page. Default so a
    /// fresh `config.toml` stays silent until operators opt in.
    #[default]
    None,
    /// Embedded `SQLite` store — shares schema with `[auth]` when
    /// paths match (recommended).
    Sqlite,
}

/// `[storage.sqlite]` settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SqliteStorageConfig {
    /// Filesystem path to the `SQLite` database. Auto-created.
    /// Point this at the same path as `[auth.sqlite] path` to share
    /// one DB file across both traits.
    pub path: std::path::PathBuf,
}

impl Default for SqliteStorageConfig {
    fn default() -> Self {
        Self {
            path: std::path::PathBuf::from("smiths-storage.db"),
        }
    }
}

/// `[auth]` TOML block — subscriber-DB backend selection and realm.
///
/// ```toml
/// [auth]
/// backend = "sqlite"        # "none" | "sqlite"
/// realm   = "smiths.local"
///
/// [auth.sqlite]
/// path = "/var/lib/smiths-net/auth.db"
/// ```
///
/// `backend = "none"` (the default today) keeps the pre-v0.33.0 dev
/// behaviour: REGISTER is accepted blindly, INVITE isn't challenged.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Which subscriber-DB implementation to wire up.
    pub backend: AuthBackend,
    /// Digest-auth realm the engine advertises in `WWW-Authenticate`.
    /// Must match the realm stored against each account; mismatched
    /// realms surface to UAs as `401 Unauthorized` with the engine's
    /// value.
    pub realm: String,
    /// SQLite-specific settings. Ignored when `backend != "sqlite"`.
    pub sqlite: SqliteAuthConfig,
    /// HTTP-webhook settings. Ignored when `backend != "http"`.
    pub http: HttpAuthConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            backend: AuthBackend::None,
            realm: "smiths.local".to_owned(),
            sqlite: SqliteAuthConfig::default(),
            http: HttpAuthConfig::default(),
        }
    }
}

/// Subscriber-DB backend selector. Extend by adding a variant +
/// wiring the corresponding `smiths-sip::auth::*_store` impl.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthBackend {
    /// No credential store: REGISTER + INVITE accepted without auth.
    /// Same as pre-v0.33.0 behaviour. Default so existing
    /// `config.toml` files keep working.
    #[default]
    None,
    /// Embedded `SQLite` store. Path configured via
    /// [`AuthConfig::sqlite`].
    Sqlite,
    /// HTTP webhook — engine posts a challenge to the operator's
    /// endpoint and expects a pre-computed HA1 or deny verdict back.
    /// Config in [`AuthConfig::http`].
    Http,
}

/// `[auth.sqlite]` settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SqliteAuthConfig {
    /// Filesystem path to the `SQLite` database. Opened with
    /// auto-create; the enclosing directory must already exist.
    pub path: std::path::PathBuf,
}

impl Default for SqliteAuthConfig {
    fn default() -> Self {
        Self {
            path: std::path::PathBuf::from("smiths-auth.db"),
        }
    }
}

/// `[auth.http]` settings — webhook endpoint + breaker tuning.
///
/// Mirror of `smiths-sip::auth::http_store::HttpAuthConfig`. Kept in
/// `smiths-core` so operators configure auth without the CLI having
/// to reach sideways into `smiths-sip`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpAuthConfig {
    /// Full endpoint URL the engine POSTs challenges to. Required
    /// when `backend = "http"`; empty on `"none"` / `"sqlite"`.
    pub endpoint: String,
    /// Per-request timeout, milliseconds. Default 2000.
    pub timeout_ms: u64,
    /// Retry count per lookup (on top of the first attempt).
    /// Default 1.
    pub retries: u8,
    /// `Authorization: Bearer <token>` sent with every webhook
    /// request, so the backend can authenticate the engine itself.
    /// `None` = no bearer.
    pub bearer_token: Option<String>,
    /// Consecutive failures before the circuit breaker trips Open.
    /// Default 5.
    pub breaker_threshold: u32,
    /// Cooldown (seconds) after the breaker trips Open before the
    /// next probe is attempted. Default 30.
    pub breaker_cooldown_secs: u64,
    /// What to do while the breaker is Open. `"fail_closed"`
    /// (default) treats every lookup as deny; `"fail_open"` returns
    /// `UnknownUser` without touching the breaker — only appropriate
    /// when auth is optional.
    pub failure_mode: HttpFailureMode,
}

impl Default for HttpAuthConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            timeout_ms: 2_000,
            retries: 1,
            bearer_token: None,
            breaker_threshold: 5,
            breaker_cooldown_secs: 30,
            failure_mode: HttpFailureMode::FailClosed,
        }
    }
}

/// Wire form of `smiths-sip::auth::http_store::FailureMode`.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HttpFailureMode {
    /// Every lookup returns deny while the breaker is Open. Default.
    #[default]
    FailClosed,
    /// Every lookup returns `UnknownUser` (no breaker increment).
    FailOpen,
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
    /// Per-call packet-capture directory. `None` disables the pcap
    /// tap entirely. When set, each call's RTP + RTCP stream is
    /// written to `<pcap_dir>/<call-id>.pcap`; the feature is
    /// gated behind the `pcap` Cargo feature on `smiths-media`
    /// because dependency size is non-trivial.
    pub pcap_dir: Option<std::path::PathBuf>,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
            log_format: LogFormat::Json,
            health_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            pcap_dir: None,
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
    /// Filesystem path to the PEM-encoded TLS server certificate.
    /// Required when `transports` contains `tls`. Ignored otherwise.
    pub tls_cert_path: Option<std::path::PathBuf>,
    /// Filesystem path to the PEM-encoded TLS private key that pairs
    /// with `tls_cert_path`.
    pub tls_key_path: Option<std::path::PathBuf>,
    /// Per-source-IP rate limit on inbound SIP datagrams.
    pub rate_limit: SipRateLimit,
}

/// Token-bucket rate limit applied per source IP at UAS ingress.
///
/// `per_sec == 0` disables the limiter entirely (default, dev-friendly).
/// `per_sec > 0` rate-limits new datagrams to the configured rate with
/// a bucket depth of `burst` (falling back to `per_sec` when `burst == 0`).
/// Datagrams from over-limit sources are dropped silently — this is
/// anti-flood, not a protocol-level response, so we don't burn a
/// `503 Service Unavailable` generation on every dropped packet.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SipRateLimit {
    /// Sustained datagrams/sec allowed per source IP. `0` disables.
    pub per_sec: u32,
    /// Maximum bucket depth. `0` falls back to `per_sec`.
    pub burst: u32,
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
            tls_cert_path: None,
            tls_key_path: None,
            rate_limit: SipRateLimit::default(),
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

/// MCP (Model Context Protocol) server settings.
///
/// `enabled_http` is off by default because the stdio variant is the
/// canonical MCP entry point for LLM agents spawning the engine as a
/// subprocess. HTTP is useful for long-running daemons.
///
/// `rate_limit` applies to **every** tool dispatcher — both MCP
/// (stdio/HTTP) and A2A share the same token buckets, since the
/// protection target is the engine, not the adapter.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Serve MCP over HTTP JSON-RPC when `true`. stdio is always
    /// available via the `--mcp` CLI flag regardless of this setting.
    pub enabled_http: bool,
    /// HTTP bind for MCP.
    pub http_bind: SocketAddr,
    /// Token-bucket rate limit applied to tool invocations.
    pub rate_limit: RateLimitConfig,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled_http: false,
            http_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7878),
            rate_limit: RateLimitConfig::default(),
        }
    }
}

/// Token-bucket rate limit config for tool dispatch.
///
/// `per_sec == 0` disables the limiter entirely (the default —
/// operators opt in when they start hosting external traffic).
/// `burst == 0` falls back to `per_sec` so a bare `per_sec` override
/// still works without an explicit burst.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sustained tokens / second refill rate. `0` = disabled.
    pub per_sec: u32,
    /// Maximum bucket depth (burst allowance). `0` = fall back to
    /// `per_sec`.
    pub burst: u32,
}

/// A2A (agent-to-agent) HTTP adapter settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct A2aConfig {
    /// Serve the A2A HTTP API when `true`.
    pub enabled: bool,
    /// HTTP bind for A2A.
    pub bind: SocketAddr,
    /// Optional bearer token. When set, every HTTP request must carry
    /// a matching `Authorization: Bearer <token>` header or the server
    /// returns `401 Unauthorized`. `None` disables auth — fine for
    /// local development, never for public deployments.
    pub bearer_token: Option<String>,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7879),
            bearer_token: None,
        }
    }
}

/// Plugin loader settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    /// Directory the loader scans at startup. Each subdirectory is one
    /// plugin. Missing directory → no plugins loaded, no error.
    pub dir: std::path::PathBuf,
    /// Resource-limit sandbox applied to every sidecar subprocess.
    /// Default is permissive (no limits, no `no_new_privs`) so tests
    /// and dev runs aren't surprised; production deployments should
    /// set conservative caps per the operator runbook.
    pub sandbox: SandboxConfig,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            dir: std::path::PathBuf::from("plugins"),
            sandbox: SandboxConfig::default(),
        }
    }
}

/// Per-sidecar sandbox knobs applied right before the child `exec`s.
///
/// Every field is optional — `None` means "don't touch the default
/// (usually inherited from the engine process)". Limits that are
/// POSIX-standard (`RLIMIT_*`) apply on Linux + macOS; Linux-only
/// toggles (`no_new_privs`) are no-ops elsewhere with a debug log.
///
/// Full seccomp-BPF filtering and user-namespace isolation are NOT
/// in this struct — they warrant their own slice and config surface
/// because their correctness is deeply bound to the guest's syscall
/// set (tokio + the plugin's runtime). This struct is the MVP
/// sandboxing item 8 called for: FD / memory / CPU / process caps
/// plus the privilege-escalation gate.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// `RLIMIT_NOFILE` soft + hard cap. Caps the number of file
    /// descriptors the plugin can hold. Prevents FD-exhaustion
    /// denial-of-service against the host.
    pub max_fds: Option<u64>,
    /// `RLIMIT_AS` cap in bytes — the process's max virtual
    /// address space. Approximates a memory ceiling portably;
    /// cgroups + OOM scoring are a separate story.
    pub max_memory_bytes: Option<u64>,
    /// `RLIMIT_CPU` soft cap in seconds. Kernel sends `SIGXCPU` when
    /// the plugin exceeds it; by default that terminates the child.
    pub max_cpu_seconds: Option<u64>,
    /// `RLIMIT_NPROC` cap — how many additional processes this user
    /// can spawn. Set `Some(0)` to forbid `fork()` / `exec()` from
    /// the plugin entirely (it can't spawn helpers, launch shells,
    /// etc.).
    pub max_processes: Option<u64>,
    /// Apply `prctl(PR_SET_NO_NEW_PRIVS, 1)` before exec. Prevents
    /// the plugin from gaining privileges via setuid / file caps.
    /// Linux-only; silently skipped elsewhere.
    pub no_new_privs: bool,
    /// Seccomp-BPF syscall filter policy (Linux only). `Off` skips
    /// filtering entirely; `Allowlist` installs a curated
    /// allowlist + denies everything else with `ERRNO(EPERM)`.
    /// Silently ignored on non-Linux targets.
    #[serde(default)]
    pub seccomp: SeccompPolicy,
    /// Additional syscall names to allow **on top of** the
    /// [`SeccompPolicy::Allowlist`] baseline. Lets operators permit
    /// plugin-specific syscalls (`io_uring_setup`, `statx`, etc.)
    /// without the engine re-auditing its default list. Empty by
    /// default; ignored when `seccomp = Off`.
    #[serde(default)]
    pub seccomp_extra_allow: Vec<String>,
}

/// Seccomp-BPF policy selector. Tiny on-wire form so `[plugins.sandbox]
/// seccomp = "allowlist"` reads naturally in TOML.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeccompPolicy {
    /// No seccomp filter. Plugins can make any syscall the kernel
    /// permits for the running UID. Default.
    #[default]
    Off,
    /// Install a curated allowlist covering tokio's runtime + the
    /// syscalls typical Rust / Python / Node plugins need. Deny
    /// everything else with `ERRNO(EPERM)` so failures surface as
    /// ordinary "operation not permitted" errors rather than kernel
    /// kills (easier to debug).
    Allowlist,
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
