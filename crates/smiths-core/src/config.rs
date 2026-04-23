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
    /// Media-plane tunings (DTMF inband detection, later: jitter
    /// buffer depth, comfort-noise on silence).
    pub media: MediaConfig,
    /// AI-provider configuration (P22 / slice 3.2). Keys here are
    /// secrets — the `config://current` resource redacts them on
    /// render. Sidecars read their own API keys from environment
    /// variables; the operator threads them through here for
    /// single-source-of-truth deployments.
    pub ai: AiConfig,
    /// WebTransport signaling listener (slice 5.7 / P19). Off by
    /// default — the runtime is a scaffold today, matching the
    /// `[mcp.http3]` and `[sip] transports = ["quic"]` scaffolds.
    /// Enabling today + building without `--features webtransport`
    /// on `smiths-sip` is a config error that surfaces at boot.
    pub webtransport: WebTransportConfig,
    /// Config hot-reload substrate (slice 5.8 scaffold). Off by
    /// default; the runtime — `ArcSwap<Config>` + `#[reloadable]`
    /// derive macro + SIGHUP handler — lands in a focused
    /// follow-on. The config block exists today so operators can
    /// express their intent in TOML + the future CLI knows how
    /// to read it.
    pub reload: ReloadConfig,
    /// Config canary + auto-rollback (slice 5.9 scaffold).
    /// Builds on `[reload]` — thresholds trip a rollback of the
    /// most recent `apply` when hard-failure probes fire.
    pub canary: CanaryConfig,
    /// WebRTC-native signaling adapter (slice 5.10 scaffold).
    /// Pairs with the 5.7 WebTransport scaffold; shares the
    /// JSON-over-stream message shape. Runtime follow-on.
    pub webrtc: WebRtcConfig,
}

/// `[ai]` TOML block — cloud-provider secrets for the reference
/// sidecars. All fields are optional.
///
/// ```toml
/// [ai]
/// openai_api_key    = "sk-..."
/// anthropic_api_key = "sk-ant-..."
/// ```
///
/// Keys are redacted in `config://current` via the MCP resource
/// layer. Today the engine does **not** forward these to sidecar
/// child processes automatically — operators set the corresponding
/// env vars (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) in the engine's
/// own environment, and the sidecars inherit them. This section
/// exists so the secrets have one canonical home on disk + a
/// redaction-tested surface.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    /// `OpenAI` API key — consumed by `ai-llm-openai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai_api_key: Option<String>,
    /// `Anthropic` API key — consumed by `ai-llm-anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_api_key: Option<String>,
}

/// `[media]` TOML block — per-leg media-plane tunings.
///
/// ```toml
/// [media]
/// inband_dtmf = true    # run the Goertzel detector on every bridge
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// Opt every bridge into the Goertzel inband DTMF detector
    /// (slice 2.5). Off by default — the RFC 4733 telephone-event
    /// path (always on when a DTMF sink is wired) covers most
    /// softphones. Enable when legs that never negotiate 4733
    /// (PSTN gateway crossings) need DTMF too.
    pub inband_dtmf: bool,
    /// IVR prompt library (slice 4.2). Points at a directory of
    /// WAV files (`prompts/welcome.wav`, etc.) that IVR scripts
    /// refer to by relative path. When the path is empty, the
    /// `record_prompt` MCP tool returns `NotFound` — operators
    /// opt in by setting a concrete directory.
    pub prompts: PromptsConfig,
    /// Audio transcoding CPU budget + admission control (slice 5.3).
    /// Governs how many simultaneous calls the engine will accept
    /// that require codec conversion (today: `Opus ↔ G.711`).
    pub transcode: TranscodeConfig,
}

/// `[media.transcode]` TOML block — CPU budget + admission control
/// for audio transcoding (slice 5.3).
///
/// ```toml
/// [media.transcode]
/// max_concurrent_calls  = 40    # 0 or unset = built-in default
/// cpu_budget_ms_per_call = 50   # advisory; SLO not hard cap
/// ```
///
/// Defaults target a 4-core box with ~2.5 % CPU per Opus call —
/// raise `max_concurrent_calls` after observing the
/// `smiths_transcode_cpu_ms_total` counter under real traffic.
/// See `docs/architecture/11-transcoding.md` for sizing guidance.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscodeConfig {
    /// Hard cap on simultaneous transcoded calls. INVITEs past this
    /// cap receive a `488 Not Acceptable Here` with
    /// `Warning: 370 transcode budget exhausted`. Default: 40.
    pub max_concurrent_calls: usize,
    /// Advisory per-call CPU-ms budget. Drives the
    /// `smiths_transcode_cpu_ms_total` alerting threshold but is
    /// *not* enforced per-frame — the bridge doesn't hard-preempt a
    /// live transcoder. Default: 50.
    pub cpu_budget_ms_per_call: u64,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            max_concurrent_calls: 40,
            cpu_budget_ms_per_call: 50,
        }
    }
}

/// `[media.prompts]` — IVR prompt-library settings.
///
/// ```toml
/// [media.prompts]
/// root     = "/var/lib/smiths-net/prompts"
/// capacity = 128             # LRU size (default 64)
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptsConfig {
    /// Root directory the library resolves relative paths against.
    /// Empty string disables the library — `record_prompt` then
    /// surfaces a clean "not wired" error instead of writing
    /// somewhere surprising.
    pub root: String,
    /// Maximum number of decoded prompts kept hot in the LRU.
    /// `0` falls back to the library's built-in default.
    #[serde(default)]
    pub capacity: usize,
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
    /// Embedding-indexed search surface (slice 3.4). Off by default
    /// — `search_calls_semantic` returns a clean `NotFound` when
    /// this is `none`.
    pub vector: VectorStoreConfig,
    /// Per-call audio retention (slice 3.4). Off by default; the
    /// filesystem backend makes `transcribe_call` / `summarize_call`
    /// self-resolve audio from a bare `call_id`.
    pub recording: RecordingStoreConfig,
}

/// `[storage.vector]` TOML block — vector-index backend selection.
///
/// ```toml
/// [storage.vector]
/// backend = "memory"        # "none" | "memory" | "sidecar"
///
/// # When backend = "sidecar":
/// # plugin = "store-qdrant"
/// ```
///
/// `memory` is the in-process [`crate::MemoryVectorStore`] —
/// good for tests and single-node deployments without durability.
/// `sidecar` delegates to a loaded plugin that advertises the
/// `storage.vector` capability (today: `store-qdrant`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VectorStoreConfig {
    /// Which backend to wire up.
    pub backend: VectorBackend,
    /// When `backend = "sidecar"`, the plugin name that serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// Vector-store backend selector.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VectorBackend {
    /// No vector store wired. `search_calls_semantic` returns
    /// `NotFound`.
    #[default]
    None,
    /// In-process [`crate::MemoryVectorStore`]. Loses every record
    /// on restart; appropriate for tests + ephemeral dev loops.
    Memory,
    /// Delegates to a loaded plugin via the `storage.vector`
    /// capability seam. The plugin name is taken from
    /// [`VectorStoreConfig::plugin`].
    Sidecar,
}

/// `[storage.recording]` TOML block — per-call audio retention.
///
/// ```toml
/// [storage.recording]
/// backend        = "fs"          # "none" | "fs" | "sidecar"
/// retention_days = 30            # 0 disables retention sweeps
///
/// [storage.recording.fs]
/// root = "/var/lib/smiths-net/recordings"
/// ```
///
/// The filesystem backend writes `<hex(call_id)>.wav` +
/// `<hex(call_id)>.cid` sidecar files. Sidecar backends (S3,
/// Azure Blob, GCS) slot in the same way the vector sidecar does
/// — by advertising the `storage.recording` capability.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingStoreConfig {
    /// Which backend to wire up.
    pub backend: RecordingBackend,
    /// Filesystem-specific settings. Ignored unless `backend = "fs"`.
    pub fs: FsRecordingConfig,
    /// Retention in days. `0` disables the periodic sweeper; the
    /// engine then only prunes when an operator calls `truncate`
    /// equivalents manually. Non-zero values start a once-per-hour
    /// sweep that deletes blobs whose on-disk mtime is older.
    pub retention_days: u32,
    /// When `backend = "sidecar"`, the plugin name that serves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// Recording backend selector.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecordingBackend {
    /// No audio retention. Post-processing tools that need audio
    /// (`transcribe_call`, `summarize_call`) still work via the
    /// `audio_base64` override but can't self-resolve from a
    /// bare `call_id`.
    #[default]
    None,
    /// [`crate::FsRecordingStore`] at [`FsRecordingConfig::root`].
    Fs,
    /// Delegates to a loaded plugin via the `storage.recording`
    /// capability seam (today: `store-s3-recording`).
    Sidecar,
}

/// `[storage.recording.fs]` settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsRecordingConfig {
    /// Root directory for stored recordings. Auto-created on
    /// first write; operators should point this at a persistent
    /// volume in production.
    pub root: std::path::PathBuf,
}

impl Default for FsRecordingConfig {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("smiths-recordings"),
        }
    }
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
    /// Outbound proxy / VPN shim (slice 3.5). Applies to the
    /// TCP-based SIP transports (TCP, TLS inner TCP) — SOCKS5 and
    /// HTTP-CONNECT are stream protocols so UDP can't ride them.
    /// The UDP path ignores this block.
    pub proxy: SipProxyConfig,
    /// Optional embedded-`WireGuard` device (slice 3.5 / feature
    /// `wireguard`). Operators who run `WireGuard` as a host sidecar
    /// leave this `mode = "none"`; operators on appliance-style
    /// hosts enable it to bring up the tunnel in-process.
    pub vpn: SipVpnConfig,
}

/// `[sip.vpn]` — embedded userspace `WireGuard` device. Gated behind
/// the `smiths-cli/wireguard` Cargo feature at runtime. See
/// `docs/deployment/vpn.md` for the full deployment story.
///
/// ```toml
/// [sip.vpn]
/// mode            = "wireguard"       # "none" | "wireguard"
/// private_key     = "..."              # base64 Curve25519 private key
/// peer_public_key = "..."
/// peer_endpoint   = "203.0.113.7:51820"
/// allowed_ips     = ["10.42.0.0/24"]
/// interface_ip    = "10.42.0.5/24"
/// ```
///
/// The runtime plumbing (creating the tun interface, binding SIP
/// against it) is a follow-on — the 0.42.0 release ships only the
/// config surface. An engine built with `features = ["wireguard"]`
/// and `mode = "wireguard"` warns at startup and falls back to
/// `mode = "none"` until the runtime lands.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SipVpnConfig {
    /// Which VPN mode to activate.
    pub mode: VpnMode,
    /// Base64 Curve25519 private key for this engine's WG device.
    /// Redacted in `config://current`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// Base64 Curve25519 peer public key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_public_key: Option<String>,
    /// Remote peer endpoint (`host:port`) the local device dials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_endpoint: Option<String>,
    /// CIDR ranges routed into the tunnel.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_ips: Vec<String>,
    /// Local IP (with prefix) to assign to the in-process interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface_ip: Option<String>,
}

/// Embedded-VPN mode selector.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VpnMode {
    /// No embedded VPN. Operators either run `WireGuard` as a host
    /// sidecar or don't need a tunnel at all.
    #[default]
    None,
    /// Embedded `boringtun` device. Only honored when the binary
    /// was built with `--features wireguard`; otherwise the engine
    /// falls back to `None` with a warning log.
    Wireguard,
}

/// `[sip.proxy]` — outbound-connection shim for TCP-based SIP
/// transports. Mirrors how curl / aws-cli expose the same knobs
/// operators already know.
///
/// ```toml
/// [sip.proxy]
/// mode = "socks5"                       # "none" | "socks5" | "http-connect"
/// address = "127.0.0.1:9050"            # proxy host:port
/// username = "circuit-a"                # optional for socks5 / http-connect
/// password = "secret"                   # optional; redacted in config://current
/// ```
///
/// The proxy is applied at outbound-connect time (the `send` path
/// opens a fresh TCP connection to a peer). Listener binds are
/// unaffected — operators wanting ingress protection terminate TLS
/// or run a reverse proxy in front of the engine.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SipProxyConfig {
    /// Which proxy protocol to wrap outbound TCP connects in.
    pub mode: ProxyMode,
    /// Proxy host:port. Required when `mode != "none"`; ignored
    /// otherwise. Parsed as `SocketAddr` at config-load time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<SocketAddr>,
    /// Optional username for `socks5` (RFC 1929) or HTTP-CONNECT
    /// `Proxy-Authorization: Basic` auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Optional password paired with `username`. Redacted in the
    /// `config://current` MCP resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Outbound-proxy protocol selector.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyMode {
    /// No proxy. Outbound TCP connects go direct.
    #[default]
    None,
    /// RFC 1928 SOCKS5 CONNECT. Supports RFC 1929 user/password auth
    /// when `username` + `password` are set; otherwise the `no-auth`
    /// method is offered.
    Socks5,
    /// HTTP/1.1 CONNECT tunnel (RFC 9110 §9.3.6). `username` +
    /// `password` flow as `Proxy-Authorization: Basic ...`.
    HttpConnect,
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
            proxy: SipProxyConfig::default(),
            vpn: SipVpnConfig::default(),
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
    /// SIP-over-QUIC per `draft-ietf-sipcore-sip-quic` (slice 4.3 /
    /// P17). Requires the `smiths-sip/sip-quic` Cargo feature; the
    /// runtime listener is a dedicated follow-on. Selecting this
    /// transport today with the feature off is a config error;
    /// selecting it with the feature on logs a clear "not yet
    /// wired" warning at bind.
    Quic,
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
    /// HTTP/3 (QUIC) bind for MCP (slice 4.3 / P17). Requires the
    /// `smiths-mcp/mcp-http3` Cargo feature; off by default. The
    /// runtime listener is a dedicated follow-on — 0.45.0 accepts
    /// the config + advertises `h3` in `--version` so operators
    /// aren't surprised later.
    pub http3: McpHttp3Config,
}

/// `[mcp.http3]` — HTTP/3 bind for the MCP adapter.
///
/// ```toml
/// [mcp.http3]
/// enabled = true                  # requires --features mcp-http3
/// bind    = "127.0.0.1:7879"
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpHttp3Config {
    /// Enable the h3 listener. Ignored when the binary was built
    /// without `--features mcp-http3`; the CLI logs a clear warning
    /// in that case.
    pub enabled: bool,
    /// UDP bind for the QUIC listener. Distinct port from the TCP
    /// `http_bind` so operators can front only h3 with a public
    /// load-balancer while keeping h1/h2 on loopback.
    pub bind: SocketAddr,
}

impl Default for McpHttp3Config {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7879),
        }
    }
}

/// `[webtransport]` TOML block — browser-native signaling listener
/// (slice 5.7 / P19). Today a scaffold: flipping `enabled = true`
/// with a binary built without `--features webtransport` is a config
/// error that surfaces at boot; flipping it on *with* the feature
/// logs a "scaffold-only" warning and refuses to bind until the
/// runtime follow-on slice lands.
///
/// ```toml
/// [webtransport]
/// enabled   = true                # requires --features webtransport
/// bind      = "0.0.0.0:7880"      # UDP (QUIC)
/// cert_path = "/etc/smiths/wt.crt"
/// key_path  = "/etc/smiths/wt.key"
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebTransportConfig {
    /// Enable the WebTransport listener. Ignored when the binary
    /// was built without `--features webtransport` on `smiths-sip`;
    /// the CLI logs a clear warning in that case.
    pub enabled: bool,
    /// UDP bind for the QUIC listener. Default picks a loopback
    /// port so accidentally flipping `enabled = true` can't
    /// surprise-expose anything.
    pub bind: SocketAddr,
    /// Path to the TLS certificate (PEM) the listener serves.
    /// Empty = unconfigured; the runtime rejects bind until the
    /// operator points at a real cert.
    pub cert_path: String,
    /// Path to the matching private key (PEM).
    pub key_path: String,
}

impl Default for WebTransportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7880),
            cert_path: String::new(),
            key_path: String::new(),
        }
    }
}

/// `[reload]` TOML block — config hot-reload substrate (slice
/// 5.8 scaffold). The runtime — `ArcSwap<Config>`,
/// `#[derive(Reloadable)]`, `Config::apply` returning
/// `ApplyReport` — lands in a focused follow-on. The block
/// exists today so the CLI can accept `--reload` on the command
/// line without the build changing; the current behaviour is
/// "refuse with `RestartRequired` for every field" until the
/// derive macro lands.
///
/// ```toml
/// [reload]
/// enabled         = true     # accept SIGHUP + CLI `reload`
/// signal          = "SIGHUP" # POSIX default; Windows uses a named event
/// max_frequency_s = 10       # reject reloads arriving faster than this
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReloadConfig {
    /// Enable SIGHUP-triggered + CLI-triggered config reload.
    /// Scaffold only today: flipping this on logs a loud
    /// "runtime not yet wired" warning at startup.
    pub enabled: bool,
    /// Minimum seconds between reload attempts; extras are
    /// refused with a clean diagnostic rather than queued.
    pub max_frequency_s: u64,
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_frequency_s: 10,
        }
    }
}

/// `[canary]` TOML block — config-change canary window +
/// auto-rollback thresholds (slice 5.9 scaffold). Depends on
/// `[reload]`'s runtime. Once the derive macro + `Config::apply`
/// land, this block controls:
///
/// - `deadline_s` — how long the new config has to prove itself
///   before auto-rolling back to the prior snapshot.
/// - `plugin_error_rate_ceiling` — hard-failure early rollback
///   if the plugin error rate in a 30 s trailing window trips
///   this.
/// - `sip_parse_errors_per_sec_ceiling` — same, for SIP parse
///   error rate.
///
/// ```toml
/// [canary]
/// deadline_s                        = 300
/// plugin_error_rate_ceiling         = 0.5
/// sip_parse_errors_per_sec_ceiling  = 10
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CanaryConfig {
    /// Seconds the new config gets before auto-rollback.
    pub deadline_s: u64,
    /// Plugin-invocation error-rate ceiling (0.0..=1.0) that
    /// triggers hard-failure rollback. `1.0` disables.
    pub plugin_error_rate_ceiling: f32,
    /// SIP parse-error rate (per second) that triggers
    /// hard-failure rollback. `u64::MAX` disables.
    pub sip_parse_errors_per_sec_ceiling: u64,
}

impl Default for CanaryConfig {
    fn default() -> Self {
        Self {
            deadline_s: 300,
            plugin_error_rate_ceiling: 0.5,
            sip_parse_errors_per_sec_ceiling: 10,
        }
    }
}

/// `[webrtc]` TOML block — WebRTC-native signaling + privacy
/// (slices 5.10 + 5.11 scaffold). The runtime adapter and the
/// privacy layers land in dedicated follow-on slices; the block
/// exists today so operators can express their intent. Pairs
/// with the 5.7 `[webtransport]` block: the JSON message shape
/// is shared, 5.7 is the QUIC transport substrate, 5.10 is the
/// WebSocket baseline.
///
/// ```toml
/// [webrtc]
/// enabled  = true
/// ws_bind  = "0.0.0.0:7881"
/// tls_cert = "/etc/smiths/wt.crt"
/// tls_key  = "/etc/smiths/wt.key"
///
/// [webrtc.privacy]
/// mode           = "open"         # "open" | "relay_only" | "strict"
/// redaction_key  = ""             # required for `strict`
/// ```
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcConfig {
    /// Enable the WebRTC-native signaling adapter. Scaffold
    /// today — binds nothing, logs "runtime not yet wired".
    pub enabled: bool,
    /// WebSocket bind for the signaling adapter.
    pub ws_bind: SocketAddr,
    /// Path to the TLS cert the adapter serves. Empty =
    /// plaintext (disallowed in privacy `strict` mode).
    pub tls_cert: String,
    /// Matching private key.
    pub tls_key: String,
    /// Privacy hardening knobs (slice 5.11 scaffold).
    pub privacy: WebRtcPrivacyConfig,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ws_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7881),
            tls_cert: String::new(),
            tls_key: String::new(),
            privacy: WebRtcPrivacyConfig::default(),
        }
    }
}

/// `[webrtc.privacy]` — privacy hardening modes (slice 5.11
/// scaffold). Three modes that compose additively:
///
/// - `open` (default) — today's behavior, no hardening.
/// - `relay_only` — reject offers carrying `host` / `srflx`
///   candidates; strip `host` candidates from answers; hint
///   the client to `iceTransportPolicy = "relay"`.
/// - `strict` — `relay_only` + keyed-hash redaction of every
///   peer IP in audit/CDR/tracing + require TLS-only signaling.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcPrivacyConfig {
    /// Privacy mode. Scaffold only — runtime enforcement is a
    /// follow-on slice.
    pub mode: WebRtcPrivacyMode,
    /// `blake3` key for source-IP redaction when
    /// `mode = "strict"`. Rotate via
    /// `[reload]` / `confirm_config`. Empty in `open` /
    /// `relay_only`.
    pub redaction_key: String,
}

impl Default for WebRtcPrivacyConfig {
    fn default() -> Self {
        Self {
            mode: WebRtcPrivacyMode::Open,
            redaction_key: String::new(),
        }
    }
}

/// Privacy mode selector for `[webrtc.privacy]`.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WebRtcPrivacyMode {
    /// Default — no privacy hardening. Matches pre-5.11 behavior.
    #[default]
    Open,
    /// Reject `host` / `srflx` candidates; force relay-only
    /// ICE. Operators paying for TURN land here.
    RelayOnly,
    /// `relay_only` + IP redaction + TLS-only signaling.
    Strict,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled_http: false,
            http_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7878),
            rate_limit: RateLimitConfig::default(),
            http3: McpHttp3Config::default(),
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
