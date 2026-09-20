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
use crate::reloader::Reloadable;

/// Root configuration loaded at startup.
///
/// Field-level attributes on nested sub-configs drive the
/// [`crate::reloader::ApplyReport`] surface: `#[nested]` recurses,
/// leaf fields classify with `#[reloadable]` /
/// `#[restart_required]`. Every field must carry one of the three
/// — the derive rejects an unmarked field at compile time, so a
/// new knob is always either hot-reloadable or restart-required
/// and never silently ignored by `apply`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Runtime-wide tuning knobs (thread pools, etc.).
    #[nested]
    pub core: CoreConfig,
    /// Logging, health endpoint, metrics bind (metrics added later).
    #[nested]
    pub observability: ObservabilityConfig,
    /// SIP signaling configuration.
    #[nested]
    pub sip: SipConfig,
    /// MCP control-plane server.
    #[nested]
    pub mcp: McpConfig,
    /// A2A HTTP adapter.
    #[nested]
    pub a2a: A2aConfig,
    /// Plugin loader settings.
    #[nested]
    pub plugins: PluginsConfig,
    /// Auth / subscriber-DB configuration (P8, ).
    #[nested]
    pub auth: AuthConfig,
    /// Pluggable storage configuration (P23, ). CDR + KV
    /// backends share this section; auth has its own `[auth]`
    /// because its lifetime + security story differs.
    #[nested]
    pub storage: StorageConfig,
    /// Media-plane tunings (DTMF inband detection, later: jitter
    /// buffer depth, comfort-noise on silence).
    #[nested]
    pub media: MediaConfig,
    /// AI-provider configuration (P22 / ). Keys here are
    /// secrets — the `config://current` resource redacts them on
    /// render. Sidecars read their own API keys from environment
    /// variables; the operator threads them through here for
    /// single-source-of-truth deployments.
    #[nested]
    pub ai: AiConfig,
    /// WebTransport signaling listener. Off by default. No build
    /// of the engine ships a WebTransport listener yet, so
    /// `enabled = true` is rejected by [`Config::validate`] rather
    /// than accepted and ignored.
    #[nested]
    pub webtransport: WebTransportConfig,
    /// Config hot-reload driver: SIGHUP / `smiths-net reload` /
    /// MCP `put_config` all funnel through
    /// [`crate::ConfigReloader::apply`]; this block gates the
    /// signal path and throttles reload frequency.
    #[nested]
    pub reload: ReloadConfig,
    /// Config canary + auto-rollback. Every `apply` arms a
    /// deadline timer and an error-rate probe from these
    /// thresholds; an unconfirmed change rolls back when either
    /// fires.
    #[nested]
    pub canary: CanaryConfig,
    /// WebRTC-native signaling adapter (WebSocket + DTLS-SRTP +
    /// ICE-lite + embedded TURN). Off by default.
    #[nested]
    pub webrtc: WebRtcConfig,
    /// HA cluster configuration.
    #[nested]
    pub cluster: ClusterConfig,
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct AiConfig {
    /// `OpenAI` API key — consumed by `ai-llm-openai`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[reloadable]
    pub openai_api_key: Option<String>,
    /// `Anthropic` API key — consumed by `ai-llm-anthropic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[reloadable]
    pub anthropic_api_key: Option<String>,
}

/// `[media]` TOML block — per-leg media-plane tunings.
///
/// ```toml
/// [media]
/// inband_dtmf = true    # run the Goertzel detector on every bridge
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// Opt every bridge into the Goertzel inband DTMF detector
    ///. Off by default — the RFC 4733 telephone-event
    /// path (always on when a DTMF sink is wired) covers most
    /// softphones. Enable when legs that never negotiate 4733
    /// (PSTN gateway crossings) need DTMF too.
    #[restart_required]
    pub inband_dtmf: bool,
    /// IVR prompt library. Points at a directory of
    /// WAV files (`prompts/welcome.wav`, etc.) that IVR scripts
    /// refer to by relative path. When the path is empty, the
    /// `record_prompt` MCP tool returns `NotFound` — operators
    /// opt in by setting a concrete directory.
    #[nested]
    pub prompts: PromptsConfig,
    /// Audio transcoding CPU budget + admission control.
    /// Governs how many simultaneous calls the engine will accept
    /// that require codec conversion (today: `Opus ↔ G.711`).
    #[reloadable(path = "media.transcode")]
    pub transcode: TranscodeConfig,
    /// Restrict RTP/RTCP media ports to a fixed range so operators can
    /// open exactly these UDP ports in a firewall. Unset (default) =
    /// ephemeral OS-assigned ports. RTP binds even ports, RTCP the
    /// odd `port + 1`.
    ///
    /// ```toml
    /// [media.rtp_ports]
    /// min = 16384
    /// max = 16484
    /// ```
    #[restart_required(group = "media rtp port range")]
    pub rtp_ports: Option<RtpPortRange>,
    /// Public IPv4/IPv6 published in SDP `c=` / `o=` for peers that
    /// cannot reach a private LAN address (home NAT / DMZ setups).
    /// Unset = auto-detect via routing table (often wrong behind NAT).
    #[restart_required]
    pub advertise_ip: Option<String>,
}

/// `[media.rtp_ports]` — inclusive UDP port window for RTP/RTCP.
///
/// Each call consumes one even RTP port and the adjacent odd RTCP
/// port, so a range of `N` ports supports up to `N / 2` concurrent
/// media legs. Pick a window large enough for peak concurrency and
/// open it in the host firewall (`udp/min-max`).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RtpPortRange {
    /// Lowest port the allocator may use (rounded up to even).
    pub min: u16,
    /// Highest port the allocator may use (inclusive).
    pub max: u16,
}

/// `[media.transcode]` TOML block — CPU budget + admission control
/// for audio transcoding.
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
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct PromptsConfig {
    /// Root directory the library resolves relative paths against.
    /// Empty string disables the library — `record_prompt` then
    /// surfaces a clean "not wired" error instead of writing
    /// somewhere surprising.
    #[restart_required]
    pub root: String,
    /// Maximum number of decoded prompts kept hot in the LRU.
    /// `0` falls back to the library's built-in default.
    #[serde(default)]
    #[reloadable]
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Which backend to wire up for `CdrStore` + `KvStore`.
    #[restart_required(group = "storage backend")]
    pub backend: StorageBackend,
    /// SQLite-specific settings. Ignored when `backend != "sqlite"`.
    #[restart_required(group = "storage backend")]
    pub sqlite: SqliteStorageConfig,
    /// Embedding-indexed search surface. Off by default
    /// — `search_calls_semantic` returns a clean `NotFound` when
    /// this is `none`.
    #[restart_required(group = "storage backend")]
    pub vector: VectorStoreConfig,
    /// Per-call audio retention. Off by default; the
    /// filesystem backend makes `transcribe_call` / `summarize_call`
    /// self-resolve audio from a bare `call_id`.
    #[restart_required(group = "storage backend")]
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// [`VectorStoreConfig::plugin`]. The engine has no sidecar
    /// adapter yet, so [`Config::validate`] rejects this value.
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// capability seam. The engine has no sidecar adapter yet, so
    /// [`Config::validate`] rejects this value.
    Sidecar,
}

/// `[storage.recording.fs]` settings.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Which subscriber-DB implementation to wire up.
    #[restart_required(group = "auth backend")]
    pub backend: AuthBackend,
    /// Digest-auth realm the engine advertises in `WWW-Authenticate`.
    /// Must match the realm stored against each account; mismatched
    /// realms surface to UAs as `401 Unauthorized` with the engine's
    /// value.
    #[restart_required(group = "auth backend")]
    pub realm: String,
    /// SQLite-specific settings. Ignored when `backend != "sqlite"`.
    #[restart_required(group = "auth backend")]
    pub sqlite: SqliteAuthConfig,
    /// HTTP-webhook settings. Ignored when `backend != "http"`.
    #[restart_required(group = "auth backend")]
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    /// Tokio worker threads. `0` means auto (number of CPUs).
    #[restart_required]
    pub worker_threads: usize,
}

/// Observability config — logging and the health endpoint.
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct ObservabilityConfig {
    /// `tracing-subscriber` env-filter directive (e.g. `info`, `debug,smiths_sip=trace`).
    #[reloadable]
    pub log_level: String,
    /// Log output formatter.
    #[restart_required(group = "observability bind / log format")]
    pub log_format: LogFormat,
    /// HTTP bind address for the `/health` endpoint.
    #[restart_required(group = "observability bind / log format")]
    pub health_bind: SocketAddr,
    /// Per-call packet-capture directory. `None` disables the pcap
    /// tap entirely. When set, each call's RTP + RTCP stream is
    /// written to `<pcap_dir>/<call-id>.pcap`; the feature is
    /// gated behind the `pcap` Cargo feature on `smiths-media`
    /// because dependency size is non-trivial.
    #[restart_required]
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
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct SipConfig {
    /// Addresses to bind for SIP signaling.
    #[restart_required(group = "sip bind / transports / tls paths")]
    pub bind: Vec<BindSpec>,
    /// Enabled transports. Only `udp` is wired in Phase 1.
    #[restart_required(group = "sip bind / transports / tls paths")]
    pub transports: Vec<SipTransport>,
    /// Grace period (seconds) the shutdown driver gives live
    /// dialogs after it has hung them up before it hard-cancels
    /// the SIP tasks. `0` skips the wait. The `SMITHS_DRAIN_SECS`
    /// environment variable overrides it at runtime.
    #[reloadable]
    pub drain_timeout_secs: u64,
    /// Filesystem path to the PEM-encoded TLS server certificate.
    /// Required when `transports` contains `tls`. Ignored otherwise.
    #[restart_required(group = "sip bind / transports / tls paths")]
    pub tls_cert_path: Option<std::path::PathBuf>,
    /// Filesystem path to the PEM-encoded TLS private key that pairs
    /// with `tls_cert_path`.
    #[restart_required(group = "sip bind / transports / tls paths")]
    pub tls_key_path: Option<std::path::PathBuf>,
    /// Per-source-IP rate limit on inbound SIP datagrams.
    #[reloadable(path = "sip.rate_limit")]
    pub rate_limit: SipRateLimit,
    /// Outbound proxy / VPN shim. Applies to the
    /// TCP-based SIP transports (TCP, TLS inner TCP) — SOCKS5 and
    /// HTTP-CONNECT are stream protocols so UDP can't ride them.
    /// The UDP path ignores this block.
    #[restart_required]
    pub proxy: SipProxyConfig,
    /// Optional embedded-`WireGuard` device ( / feature
    /// `wireguard`). Operators who run `WireGuard` as a host sidecar
    /// leave this `mode = "none"`; operators on appliance-style
    /// hosts enable it to bring up the tunnel in-process.
    #[restart_required]
    pub vpn: SipVpnConfig,
    /// Request-URI user-part prefix that marks *conference rooms*
    ///. When set, an INVITE to
    /// `sip:<prefix>…@engine` joins an N-party audio mixer (one
    /// participant per INVITE) instead of the classic 2-peer bridge.
    /// `None` (the default) = every room bridges as before.
    #[restart_required]
    pub conference_prefix: Option<String>,
    /// RFC 4028 session timers: when `true` every accepted INVITE
    /// negotiates a `Session-Expires` refresh interval and the
    /// engine tears down dialogs whose refresh never arrives.
    #[restart_required(group = "sip session timer")]
    pub session_timer_enabled: bool,
    /// Default `Session-Expires` value (seconds) offered when the
    /// peer doesn't ask for one.
    #[restart_required(group = "sip session timer")]
    pub session_expires_secs: u64,
    /// Minimum session interval (`Min-SE`, seconds) the engine
    /// accepts; shorter peer requests get `422 Session Interval Too
    /// Small`.
    #[restart_required(group = "sip session timer")]
    pub min_se_secs: u64,
    /// Hard ceiling on a dialog's lifetime (seconds). The engine
    /// BYEs any call that lasts longer. `0` = unlimited.
    #[restart_required]
    pub max_call_duration_secs: u64,
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
/// No build of the engine brings the tunnel up in-process yet, so
/// `mode = "wireguard"` is rejected by [`Config::validate`] instead
/// of being accepted and ignored; run `WireGuard` as a host sidecar.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// Embedded `boringtun` device. Rejected by
    /// [`Config::validate`] until a build ships the runtime device.
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
            conference_prefix: None,
            session_timer_enabled: true,
            session_expires_secs: 1800,
            min_se_secs: 90,
            max_call_duration_secs: 0,
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
    /// Resolve this spec to a concrete socket address for `bind`.
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
    /// SIP-over-QUIC per `draft-ietf-sipcore-sip-quic`. No build
    /// ships the QUIC listener yet, so selecting this transport is
    /// rejected by [`Config::validate`].
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
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Serve MCP over HTTP JSON-RPC when `true`. stdio is always
    /// available via the `--mcp` CLI flag regardless of this setting.
    #[restart_required(group = "mcp binds")]
    pub enabled_http: bool,
    /// HTTP bind for MCP.
    #[restart_required(group = "mcp binds")]
    pub http_bind: SocketAddr,
    /// Token-bucket rate limit applied to tool invocations. The
    /// limiter is built once at boot.
    #[restart_required]
    pub rate_limit: RateLimitConfig,
    /// HTTP/3 (QUIC) bind for MCP. No build ships the h3 listener
    /// yet, so `enabled = true` is rejected by [`Config::validate`].
    #[restart_required(group = "mcp binds")]
    pub http3: McpHttp3Config,
    /// HTTP/1.1 + HTTP/2 adapter settings (`[mcp.http]`).
    #[restart_required(group = "mcp binds")]
    pub http: McpHttpConfig,
}

/// `[mcp.http]` — settings for the plain-HTTP MCP adapter.
///
/// ```toml
/// [mcp.http]
/// bearer_token = "s3cret" # optional; unset = no auth
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct McpHttpConfig {
    /// When set, every MCP HTTP request must carry a matching
    /// `Authorization: Bearer <token>` header or the adapter
    /// returns `401 Unauthorized`. `None` disables auth — fine on
    /// loopback, never on a public bind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
}

/// `[mcp.http3]` — HTTP/3 bind for the MCP adapter.
///
/// ```toml
/// [mcp.http3]
/// enabled = true                  # requires --features mcp-http3
/// bind    = "127.0.0.1:7879"
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct McpHttp3Config {
    /// Enable the h3 listener. Rejected by [`Config::validate`]
    /// until a build ships the listener.
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

/// `[webtransport]` TOML block — browser-native signaling listener.
/// No build ships the QUIC listener yet: `enabled = true` fails
/// [`Config::validate`] so it can never be silently ignored.
///
/// ```toml
/// [webtransport]
/// enabled   = true                # requires --features webtransport
/// bind      = "0.0.0.0:7880"      # UDP (QUIC)
/// cert_path = "/etc/smiths/wt.crt"
/// key_path  = "/etc/smiths/wt.key"
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct WebTransportConfig {
    /// Enable the WebTransport listener. Rejected by
    /// [`Config::validate`] until a build ships the listener.
    #[restart_required(group = "webtransport listener")]
    pub enabled: bool,
    /// UDP bind for the QUIC listener. Default picks a loopback
    /// port so accidentally flipping `enabled = true` can't
    /// surprise-expose anything.
    #[restart_required(group = "webtransport listener")]
    pub bind: SocketAddr,
    /// Path to the TLS certificate (PEM) the listener serves.
    /// Empty = unconfigured.
    #[restart_required(group = "webtransport listener")]
    pub cert_path: String,
    /// Path to the matching private key (PEM).
    #[restart_required(group = "webtransport listener")]
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

/// `[reload]` TOML block — gates the SIGHUP / `smiths-net reload`
/// hot-reload path. The engine wraps its live config in an
/// `ArcSwap`, diffs candidates with `#[derive(Reloadable)]`, and
/// swaps only reloadable fields through
/// [`crate::ConfigReloader::apply`]. Both knobs are themselves
/// hot-reloadable: the signal driver reads the live values on
/// every SIGHUP.
///
/// ```toml
/// [reload]
/// enabled         = true     # accept SIGHUP + CLI `reload`
/// max_frequency_s = 10       # refuse reloads arriving faster than this
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct ReloadConfig {
    /// Accept SIGHUP-triggered reloads (which is also what the
    /// `smiths-net reload` subcommand sends). `false` makes the
    /// engine log and ignore the signal; MCP `put_config` is
    /// unaffected.
    #[reloadable]
    pub enabled: bool,
    /// Minimum seconds between SIGHUP reload attempts; a signal
    /// arriving sooner is refused with a warning rather than
    /// queued. `0` disables the throttle.
    #[reloadable]
    pub max_frequency_s: u64,
}

impl Default for ReloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_frequency_s: 10,
        }
    }
}

/// `[canary]` TOML block — config-change canary window +
/// auto-rollback thresholds. Every successful `apply` arms a
/// deadline timer and an error-rate probe from the *candidate's*
/// values, so the block is hot-reloadable by construction:
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
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct CanaryConfig {
    /// Seconds the new config gets before auto-rollback.
    #[reloadable]
    pub deadline_s: u64,
    /// Plugin-invocation error-rate ceiling (0.0..=1.0) that
    /// triggers hard-failure rollback. `1.0` disables.
    #[reloadable]
    pub plugin_error_rate_ceiling: f32,
    /// SIP parse-error rate (per second) that triggers
    /// hard-failure rollback. `u64::MAX` disables.
    #[reloadable]
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

/// `[webrtc]` TOML block — WebRTC-native signaling adapter: a
/// plain-HTTP WebSocket at `ws_bind` carrying JSON offers, answered
/// through the shared SDP negotiator with DTLS-SRTP, ICE-lite
/// candidates, tag-based rendezvous against SIP legs, and the
/// `[webrtc.privacy]` candidate filter. The adapter does not
/// terminate TLS itself: `wss://` comes from a reverse proxy in
/// front of it, so `tls_cert` / `tls_key` are rejected by
/// [`Config::validate`] rather than silently ignored.
///
/// ```toml
/// [webrtc]
/// enabled  = true
/// ws_bind  = "0.0.0.0:7881"
///
/// [webrtc.privacy]
/// mode           = "open"         # "open" | "relay_only" | "strict"
/// redaction_key  = ""             # required for `strict`
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcConfig {
    /// Enable the WebRTC-native signaling adapter.
    #[restart_required(group = "webrtc bind / tls")]
    pub enabled: bool,
    /// WebSocket bind for the signaling adapter.
    #[restart_required(group = "webrtc bind / tls")]
    pub ws_bind: SocketAddr,
    /// Path to a TLS cert. Must stay empty: the adapter binds
    /// plaintext and expects a TLS-terminating proxy for `wss://`.
    #[restart_required(group = "webrtc bind / tls")]
    pub tls_cert: String,
    /// Matching private key. Must stay empty (see `tls_cert`).
    #[restart_required(group = "webrtc bind / tls")]
    pub tls_key: String,
    /// Privacy hardening knobs. Hot-reloadable: mode flips and
    /// key rotation take effect on the next offer.
    #[reloadable(path = "webrtc.privacy")]
    pub privacy: WebRtcPrivacyConfig,
    /// ICE surface. Off by default — deployments without NATs
    /// keep the direct-peer-address shape.
    #[restart_required]
    pub ice: WebRtcIceConfig,
    /// TURN server / client surface. Off by default.
    /// `external_url` overrides the embedded server + redirects
    /// clients through an operator's existing `coturn`.
    #[restart_required]
    pub turn: WebRtcTurnConfig,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ws_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7881),
            tls_cert: String::new(),
            tls_key: String::new(),
            privacy: WebRtcPrivacyConfig::default(),
            ice: WebRtcIceConfig::default(),
            turn: WebRtcTurnConfig::default(),
        }
    }
}

/// `[webrtc.ice]` — ICE candidate gathering + connectivity
/// checks. When enabled, the WebRTC adapter
/// emits `a=ice-ufrag` / `a=ice-pwd` / `a=candidate:` lines
/// on every answer + runs a `binding_ping` per bridge install
/// to verify the pair before audio flows. ICE-Lite posture:
/// the engine doesn't swap roles, it just serves the peer's
/// candidate list.
///
/// ```toml
/// [webrtc.ice]
/// enabled      = true
/// host_binds   = ["0.0.0.0:50000"]          # empty = derive from ws_bind.ip
/// stun_servers = ["stun.example.net:3478"]  # srflx candidates via STUN Binding
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcIceConfig {
    /// Master switch. `false` keeps the pre-5.10-ice
    /// "peer address lives in `c=` / `m=`" shape so deployments
    /// without NATs don't pay for a STUN/TURN round trip.
    pub enabled: bool,
    /// Extra host-candidate binds the gatherer advertises. When
    /// empty, the gatherer derives one host candidate per
    /// allocated media endpoint (the default production shape).
    pub host_binds: Vec<SocketAddr>,
    /// STUN servers the engine queries (one `Binding` request
    /// each, 1 s timeout) for server-reflexive candidates on every
    /// WebRTC answer. Empty = host-only gathering.
    pub stun_servers: Vec<SocketAddr>,
}

/// `[webrtc.turn]` — embedded RFC 8656 TURN server + optional
/// external relay fallback. Off by default.
///
/// ```toml
/// [webrtc.turn]
/// enabled        = true
/// bind           = "0.0.0.0:3478"       # standard TURN port
/// realm          = "turn.example.com"
/// relay_ip       = "203.0.113.1"        # public IP to hand back
/// allocation_lifetime_s = 600
/// # One credential per operator account. Changing the list
/// # requires a restart.
/// credentials    = [
///   { username = "alice", password = "hunter2" },
/// ]
/// # When set, the server is disabled and the adapter hands
/// # clients this URL instead (typical coturn front-end).
/// external_url   = ""
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcTurnConfig {
    /// Enable the embedded TURN server.
    pub enabled: bool,
    /// UDP bind for the embedded TURN server.
    pub bind: SocketAddr,
    /// RFC 7616 long-term credential realm. Echoed in 401
    /// challenges; clients hash this into the
    /// `MESSAGE-INTEGRITY` key per RFC 8489 §14.
    pub realm: String,
    /// IP the server hands clients in `XOR-RELAYED-ADDRESS`.
    /// Defaults to the bind's IP; override to publish a
    /// routable public IP when the server runs behind a NAT.
    pub relay_ip: Option<IpAddr>,
    /// How long an allocation lives (seconds) between
    /// `REFRESH` requests. RFC 8656 §3.2 caps at 3600 s; we
    /// cap at 600 s by default so stale allocations drain
    /// faster.
    pub allocation_lifetime_s: u32,
    /// Static credentials served by the long-term
    /// credential mechanism. The server hashes them once at
    /// boot; changing the list requires a restart.
    pub credentials: Vec<WebRtcTurnCredential>,
    /// External TURN URL to hand clients instead of
    /// spawning the embedded server. When set, `enabled`
    /// is ignored + clients receive this URL verbatim on
    /// the signaling channel.
    pub external_url: String,
}

impl Default for WebRtcTurnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 3478),
            realm: String::new(),
            relay_ip: None,
            allocation_lifetime_s: 600,
            credentials: Vec::new(),
            external_url: String::new(),
        }
    }
}

/// One long-term credential entry for the embedded TURN
/// server. Passwords are held in-memory only; never logged.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcTurnCredential {
    /// `USERNAME` attribute value clients must present.
    pub username: String,
    /// Raw password. Hashed via RFC 8489's long-term key
    /// derivation (`MD5(user:realm:pass)`) on load; the
    /// plaintext doesn't live past config parse.
    pub password: String,
}

/// `[webrtc.privacy]` — privacy hardening modes. Three modes that
/// compose additively:
///
/// - `open` (default) — today's behavior, no hardening.
/// - `relay_only` — reject offers carrying `host` / `srflx`
///   candidates; strip `host` candidates from answers; hint
///   the client to `iceTransportPolicy = "relay"`.
/// - `strict` — `relay_only` + keyed-hash redaction of every
///   peer IP in audit/CDR/tracing + require TLS-only signaling.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebRtcPrivacyConfig {
    /// Privacy mode. Enforced by the WebRTC adapter on every
    /// offer (candidate filter) and every log line (redaction).
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
            http: McpHttpConfig::default(),
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
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct A2aConfig {
    /// Serve the A2A HTTP API when `true`.
    #[restart_required(group = "a2a bind")]
    pub enabled: bool,
    /// HTTP bind for A2A.
    #[restart_required(group = "a2a bind")]
    pub bind: SocketAddr,
    /// Optional bearer token. When set, every HTTP request must carry
    /// a matching `Authorization: Bearer <token>` header or the server
    /// returns `401 Unauthorized`. `None` disables auth — fine for
    /// local development, never for public deployments.
    #[restart_required(group = "a2a bind")]
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
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    /// Directory the loader scans at startup. Each subdirectory is one
    /// plugin. Missing directory → no plugins loaded, no error.
    #[restart_required]
    pub dir: std::path::PathBuf,
    /// Resource-limit sandbox applied to every sidecar subprocess.
    /// Default is permissive (no limits, no `no_new_privs`) so tests
    /// and dev runs aren't surprised; production deployments should
    /// set conservative caps per the operator runbook.
    #[restart_required]
    pub sandbox: SandboxConfig,
    /// Plugin names to notify when a SIP dialog goes live. The engine
    /// invokes each one's `on_dialog_created` method (params
    /// `{call_id, remote_rtp}`) so a call-control plugin can react to
    /// inbound calls without an explicit MCP request. Empty (default)
    /// = no call-event consumer is spawned.
    #[restart_required]
    pub call_event_hooks: Vec<String>,
    /// Resource limits for WASM-tier plugins (`[plugins.wasm]`).
    #[restart_required]
    pub wasm: PluginsWasmConfig,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            dir: std::path::PathBuf::from("plugins"),
            sandbox: SandboxConfig::default(),
            call_event_hooks: Vec::new(),
            wasm: PluginsWasmConfig::default(),
        }
    }
}

/// `[plugins.wasm]` — limits applied to every WASM-tier plugin
/// instance.
///
/// ```toml
/// [plugins.wasm]
/// memory_limit_mb = 64 # linear-memory ceiling per instance
/// invoke_timeout_ms = 5000 # per-call wall-clock budget
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsWasmConfig {
    /// Linear-memory ceiling per WASM instance, in MiB. A guest
    /// that grows past it traps instead of taking the host down.
    pub memory_limit_mb: u64,
    /// Wall-clock budget per guest invocation, in milliseconds.
    /// The host interrupts the guest when it elapses.
    pub invoke_timeout_ms: u64,
}

impl Default for PluginsWasmConfig {
    fn default() -> Self {
        Self {
            memory_limit_mb: 64,
            invoke_timeout_ms: 5_000,
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
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
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
    /// can spawn. Set `Some(0)` to forbid `fork` / `exec` from
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
    /// A missing or unreadable file is an error: a mistyped
    /// `--config` must never boot an engine on the defaults
    /// (`0.0.0.0:5060`, no auth). Unknown keys in the TOML are
    /// rejected.
    pub fn load(path: &Path) -> Result<Self, Error> {
        if !path.is_file() {
            return Err(Error::Config(format!(
                "config file not found: {}",
                path.display()
            )));
        }
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

    /// Validate cross-field invariants against the baseline
    /// [`BuildSupport`] (no optional runtime available). Same as
    /// [`Self::validate_with`] with `BuildSupport::default`.
    ///
    /// # Errors
    /// The first [`ConfigValidationError`] found.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        self.validate_with(&BuildSupport::default())
    }

    /// Validate cross-field invariants that pass TOML parsing but
    /// would fail — or, worse, be silently ignored — at boot.
    ///
    /// `support` says which optional runtimes this build actually
    /// ships. A toggle that enables a runtime the build lacks is a
    /// hard [`ConfigValidationError::Unsupported`] so an operator
    /// never reads "config accepted" as "feature active".
    ///
    /// # Errors
    /// The first [`ConfigValidationError`] found; the engine
    /// refuses to boot (or to hot-apply) on any of them.
    #[allow(clippy::too_many_lines)] // one linear checklist; splitting it hides the order
    pub fn validate_with(&self, support: &BuildSupport) -> Result<(), ConfigValidationError> {
        use ConfigValidationError as E;

        if self.sip.rate_limit.per_sec > 0 && self.sip.rate_limit.burst == 0 {
            return Err(E::RateLimitBurstZero {
                per_sec: self.sip.rate_limit.per_sec,
            });
        }
        if self.sip.transports.contains(&SipTransport::Tls)
            && (self.sip.tls_cert_path.is_none() || self.sip.tls_key_path.is_none())
        {
            return Err(E::TlsPathsMissing);
        }
        if self.sip.transports.contains(&SipTransport::Quic) && !support.sip_quic {
            return Err(E::Unsupported {
                field: "sip.transports",
                reason: "`quic` is selected but this build has no SIP-over-QUIC listener",
            });
        }
        if self.sip.vpn.mode == VpnMode::Wireguard && !support.wireguard {
            return Err(E::Unsupported {
                field: "sip.vpn.mode",
                reason: "`wireguard` is set but this build cannot bring up an embedded tunnel; \
                         run WireGuard as a host sidecar (docs/deployment/vpn.md)",
            });
        }
        if self.sip.session_timer_enabled {
            if self.sip.min_se_secs == 0 || self.sip.session_expires_secs == 0 {
                return Err(E::SessionTimerInterval {
                    session_expires_secs: self.sip.session_expires_secs,
                    min_se_secs: self.sip.min_se_secs,
                });
            }
            if self.sip.session_expires_secs < self.sip.min_se_secs {
                return Err(E::SessionTimerInterval {
                    session_expires_secs: self.sip.session_expires_secs,
                    min_se_secs: self.sip.min_se_secs,
                });
            }
        }
        if let Some(range) = self.media.rtp_ports
            && range.min >= range.max
        {
            return Err(E::RtpPortRange {
                min: range.min,
                max: range.max,
            });
        }
        if matches!(
            self.cluster.mode,
            ClusterMode::Primary | ClusterMode::Secondary
        ) && self.cluster.peer_addr.is_none()
        {
            return Err(E::ClusterPeerAddrMissing {
                mode: self.cluster.mode,
            });
        }
        if self.cluster.mode == ClusterMode::Raft && self.cluster.raft_addr.is_none() {
            return Err(E::ClusterRaftAddrMissing);
        }
        if self.auth.backend == AuthBackend::Http && self.auth.http.endpoint.trim().is_empty() {
            return Err(E::AuthHttpEndpointMissing);
        }
        if self.mcp.http3.enabled && !support.mcp_http3 {
            return Err(E::Unsupported {
                field: "mcp.http3.enabled",
                reason: "this build has no HTTP/3 listener",
            });
        }
        if self.webtransport.enabled && !support.webtransport {
            return Err(E::Unsupported {
                field: "webtransport.enabled",
                reason: "this build has no WebTransport listener",
            });
        }
        if self.storage.vector.backend == VectorBackend::Sidecar && !support.vector_sidecar {
            return Err(E::Unsupported {
                field: "storage.vector.backend",
                reason: "`sidecar` is set but this build has no vector-store sidecar adapter",
            });
        }
        if self.storage.recording.backend == RecordingBackend::Sidecar && !support.recording_sidecar
        {
            return Err(E::Unsupported {
                field: "storage.recording.backend",
                reason: "`sidecar` is set but this build has no recording-store sidecar adapter",
            });
        }
        let webrtc_tls_configured =
            !self.webrtc.tls_cert.is_empty() || !self.webrtc.tls_key.is_empty();
        if webrtc_tls_configured && !support.webrtc_tls {
            return Err(E::Unsupported {
                field: "webrtc.tls_cert",
                reason: "the WebRTC signaling adapter binds plaintext; terminate TLS in a \
                         reverse proxy (docs/deployment/webrtc.md) and leave tls_cert / \
                         tls_key empty",
            });
        }
        if self.webrtc.privacy.mode == WebRtcPrivacyMode::Strict {
            if self.webrtc.privacy.redaction_key.is_empty() {
                return Err(E::StrictPrivacyRedactionKeyMissing);
            }
            if !support.webrtc_tls {
                return Err(E::Unsupported {
                    field: "webrtc.privacy.mode",
                    reason: "`strict` requires TLS-only signaling, which this build's adapter \
                             cannot terminate; use `relay_only` behind a TLS proxy",
                });
            }
            if self.webrtc.tls_cert.is_empty() || self.webrtc.tls_key.is_empty() {
                return Err(E::StrictPrivacyTlsMissing);
            }
        }
        if !(0.0..=1.0).contains(&self.canary.plugin_error_rate_ceiling) {
            return Err(E::CanaryCeilingOutOfRange {
                value: self.canary.plugin_error_rate_ceiling,
            });
        }
        Ok(())
    }
}

/// Which optional runtimes the running binary actually ships.
/// [`Config::validate_with`] rejects any toggle that enables a
/// runtime the build lacks. Every field defaults to `false`; a
/// build flips a field to `true` only once the listener / adapter
/// behind it exists and is wired at boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
// One independent flag per optional runtime; they are read
// individually by `validate_with`, so grouping them into an enum or
// bitfield would only obscure which toggle failed.
#[allow(clippy::struct_excessive_bools)]
pub struct BuildSupport {
    /// `sip.transports = ["quic"]` can be served.
    pub sip_quic: bool,
    /// `mcp.http3.enabled = true` can be served.
    pub mcp_http3: bool,
    /// `webtransport.enabled = true` can be served.
    pub webtransport: bool,
    /// `sip.vpn.mode = "wireguard"` brings up an embedded tunnel.
    pub wireguard: bool,
    /// `storage.vector.backend = "sidecar"` has an adapter.
    pub vector_sidecar: bool,
    /// `storage.recording.backend = "sidecar"` has an adapter.
    pub recording_sidecar: bool,
    /// The WebRTC signaling adapter terminates TLS itself.
    pub webrtc_tls: bool,
}

/// Semantic config errors caught by [`Config::validate_with`].
/// Each variant names the offending field(s) so the operator can
/// fix the file without reading engine source.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigValidationError {
    /// `sip.rate_limit.per_sec > 0` with `burst = 0` — the bucket
    /// would never admit a datagram.
    #[error(
        "sip.rate_limit: per_sec = {per_sec} but burst = 0 — the bucket would never admit traffic"
    )]
    RateLimitBurstZero {
        /// Configured sustained rate.
        per_sec: u32,
    },
    /// `sip.transports` includes `tls` but a PEM path is unset.
    #[error("sip.transports includes `tls` but tls_cert_path / tls_key_path are unset")]
    TlsPathsMissing,
    /// `media.rtp_ports` window is empty or inverted.
    #[error("media.rtp_ports: min ({min}) must be lower than max ({max})")]
    RtpPortRange {
        /// Configured lower bound.
        min: u16,
        /// Configured upper bound.
        max: u16,
    },
    /// `cluster.mode = "raft"` without an inter-node RPC bind.
    #[error("cluster.mode = \"raft\" requires cluster.raft_addr")]
    ClusterRaftAddrMissing,
    /// HA mode set without a peer to talk to.
    #[error("cluster.mode = {mode:?} requires cluster.peer_addr")]
    ClusterPeerAddrMissing {
        /// Configured mode.
        mode: ClusterMode,
    },
    /// `auth.backend = "http"` without a webhook URL.
    #[error("auth.backend = http requires a non-empty auth.http.endpoint")]
    AuthHttpEndpointMissing,
    /// `webrtc.privacy.mode = "strict"` without a redaction key.
    #[error("webrtc.privacy.mode = strict requires a non-empty webrtc.privacy.redaction_key")]
    StrictPrivacyRedactionKeyMissing,
    /// `webrtc.privacy.mode = "strict"` without TLS material.
    #[error("webrtc.privacy.mode = strict requires webrtc.tls_cert and webrtc.tls_key")]
    StrictPrivacyTlsMissing,
    /// RFC 4028 intervals are inconsistent.
    #[error(
        "sip session timer: session_expires_secs ({session_expires_secs}) must be >= \
         min_se_secs ({min_se_secs}) and both must be non-zero"
    )]
    SessionTimerInterval {
        /// Configured `Session-Expires`.
        session_expires_secs: u64,
        /// Configured `Min-SE`.
        min_se_secs: u64,
    },
    /// `canary.plugin_error_rate_ceiling` outside `0.0..=1.0`.
    #[error("canary.plugin_error_rate_ceiling = {value} must be within 0.0..=1.0")]
    CanaryCeilingOutOfRange {
        /// Configured value.
        value: f32,
    },
    /// A toggle enables a runtime this build does not ship.
    #[error("{field}: {reason}")]
    Unsupported {
        /// Dotted path of the offending field.
        field: &'static str,
        /// Why the build cannot honor it.
        reason: &'static str,
    },
}

/// `[cluster]` TOML block — HA dialog replication.
///
/// ```toml
/// [cluster]
/// mode                    = "primary"             # "standalone" | "primary" | "secondary"
/// peer_addr               = "10.42.0.10:8000"     # primary: bind; secondary: primary's address
/// heartbeat_interval_secs = 5
/// ```
#[derive(Clone, Debug, Deserialize, Serialize, Reloadable)]
#[serde(default, deny_unknown_fields)]
pub struct ClusterConfig {
    /// HA role: Standalone (no replication), Primary (sends deltas),
    /// Secondary (replays deltas).
    #[restart_required]
    pub mode: ClusterMode,
    /// Replication endpoint (TCP). Required outside `standalone`.
    /// `primary` binds its replication listener here; `secondary`
    /// dials this address (the primary's listener).
    #[restart_required]
    pub peer_addr: Option<SocketAddr>,
    /// Seconds between heartbeat frames on the replication link.
    /// The primary sends one per interval while idle; the
    /// secondary redials after three silent intervals.
    #[reloadable]
    pub heartbeat_interval_secs: u32,
    /// Directory to store Raft `SQLite` logs.
    #[restart_required]
    pub raft_dir: std::path::PathBuf,
    /// Unique Raft node identifier.
    #[restart_required]
    pub node_id: u64,
    /// Address to bind for inter-node Raft RPC traffic.
    #[restart_required]
    pub raft_addr: Option<SocketAddr>,
    /// Initial cluster peers for bootstrap, format: `"node_id@host:port"`.
    #[restart_required]
    pub initial_peers: Vec<String>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            mode: ClusterMode::Standalone,
            peer_addr: None,
            heartbeat_interval_secs: 5,
            raft_dir: std::path::PathBuf::from("raft_data"),
            node_id: 1,
            raft_addr: None,
            initial_peers: Vec::new(),
        }
    }
}

/// HA role selector.
#[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClusterMode {
    /// Single node, no replication.
    #[default]
    Standalone,
    /// Primary node: publishes dialog deltas to the secondary.
    Primary,
    /// Secondary node: subscribes to deltas from the primary and
    /// replays them into its local table.
    Secondary,
    /// Raft node: the dialog table is a replicated state machine.
    /// Unlike `primary`/`secondary` this survives the loss of any
    /// single node, and writes are only acknowledged once a quorum
    /// has them. Needs `raft_addr`, `node_id` and `raft_dir`;
    /// `peer_addr` is unused.
    Raft,
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

    #[test]
    fn load_missing_file_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let err = Config::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("config file not found"), "{msg}");
        assert!(msg.contains("does-not-exist.toml"), "{msg}");
    }

    #[test]
    fn load_directory_path_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load(dir.path()).is_err());
    }

    #[test]
    fn reload_is_enabled_by_default() {
        let c = Config::default();
        assert!(c.reload.enabled);
        assert_eq!(c.reload.max_frequency_s, 10);
    }

    #[test]
    fn new_sections_have_documented_defaults() {
        let c = Config::default();
        assert!(c.sip.session_timer_enabled);
        assert_eq!(c.sip.session_expires_secs, 1800);
        assert_eq!(c.sip.min_se_secs, 90);
        assert_eq!(c.sip.max_call_duration_secs, 0);
        assert_eq!(c.mcp.http.bearer_token, None);
        assert_eq!(c.plugins.wasm.memory_limit_mb, 64);
        assert_eq!(c.plugins.wasm.invoke_timeout_ms, 5_000);
        assert!(c.validate().is_ok());
    }

    // -----------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------

    fn expect_err(cfg: &Config) -> ConfigValidationError {
        cfg.validate().unwrap_err()
    }

    #[test]
    fn validate_catches_inconsistent_rate_limit() {
        let mut cfg = Config::default();
        cfg.sip.rate_limit.per_sec = 10;
        cfg.sip.rate_limit.burst = 0;
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::RateLimitBurstZero { per_sec: 10 }
        ));
    }

    #[test]
    fn validate_requires_tls_paths_for_tls_transport() {
        let mut cfg = Config::default();
        cfg.sip.transports = vec![SipTransport::Tls];
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::TlsPathsMissing
        ));
        cfg.sip.tls_cert_path = Some("/c.pem".into());
        cfg.sip.tls_key_path = Some("/k.pem".into());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_inverted_rtp_port_range() {
        let mut cfg = Config::default();
        cfg.media.rtp_ports = Some(RtpPortRange {
            min: 20_000,
            max: 20_000,
        });
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::RtpPortRange {
                min: 20_000,
                max: 20_000
            }
        ));
        cfg.media.rtp_ports = Some(RtpPortRange {
            min: 20_000,
            max: 20_100,
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_requires_raft_addr_in_raft_mode() {
        let mut cfg = Config::default();
        cfg.cluster.mode = ClusterMode::Raft;
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::ClusterRaftAddrMissing
        ));
        // Raft does not use the primary/secondary replication link.
        cfg.cluster.raft_addr = Some("127.0.0.1:9100".parse().unwrap());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_requires_peer_addr_outside_standalone() {
        for mode in [ClusterMode::Primary, ClusterMode::Secondary] {
            let mut cfg = Config::default();
            cfg.cluster.mode = mode;
            assert!(matches!(
                expect_err(&cfg),
                ConfigValidationError::ClusterPeerAddrMissing { mode: m } if m == mode
            ));
            cfg.cluster.peer_addr = Some("127.0.0.1:9000".parse().unwrap());
            assert!(cfg.validate().is_ok());
        }
    }

    #[test]
    fn validate_requires_http_auth_endpoint() {
        let mut cfg = Config::default();
        cfg.auth.backend = AuthBackend::Http;
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::AuthHttpEndpointMissing
        ));
        cfg.auth.http.endpoint = "https://iam.example/sip-auth".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_strict_privacy_needs_key_and_tls() {
        let mut cfg = Config::default();
        cfg.webrtc.privacy.mode = WebRtcPrivacyMode::Strict;
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::StrictPrivacyRedactionKeyMissing
        ));
        cfg.webrtc.privacy.redaction_key = "k".into();
        // Baseline build cannot terminate TLS, so strict is refused.
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::Unsupported {
                field: "webrtc.privacy.mode",
                ..
            }
        ));
        // A build that terminates TLS still needs the PEM paths.
        let support = BuildSupport {
            webrtc_tls: true,
            ..BuildSupport::default()
        };
        assert!(matches!(
            cfg.validate_with(&support).unwrap_err(),
            ConfigValidationError::StrictPrivacyTlsMissing
        ));
        cfg.webrtc.tls_cert = "/c.pem".into();
        cfg.webrtc.tls_key = "/k.pem".into();
        assert!(cfg.validate_with(&support).is_ok());
    }

    #[test]
    fn validate_rejects_session_timer_interval_below_min_se() {
        let mut cfg = Config::default();
        cfg.sip.session_expires_secs = 30;
        cfg.sip.min_se_secs = 90;
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::SessionTimerInterval { .. }
        ));
        cfg.sip.session_timer_enabled = false;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_canary_ceiling_out_of_range() {
        let mut cfg = Config::default();
        cfg.canary.plugin_error_rate_ceiling = 1.5;
        assert!(matches!(
            expect_err(&cfg),
            ConfigValidationError::CanaryCeilingOutOfRange { .. }
        ));
    }

    /// Every toggle that enables a runtime the baseline build lacks
    /// is a hard validation error — never a warn-and-ignore.
    #[test]
    fn validate_rejects_unsupported_toggles_in_baseline_build() {
        type Mutator = fn(&mut Config);
        let cases: &[(&str, Mutator)] = &[
            ("sip.transports", |c| {
                c.sip.transports.push(SipTransport::Quic);
            }),
            ("sip.vpn.mode", |c| c.sip.vpn.mode = VpnMode::Wireguard),
            ("mcp.http3.enabled", |c| c.mcp.http3.enabled = true),
            ("webtransport.enabled", |c| c.webtransport.enabled = true),
            ("storage.vector.backend", |c| {
                c.storage.vector.backend = VectorBackend::Sidecar;
            }),
            ("storage.recording.backend", |c| {
                c.storage.recording.backend = RecordingBackend::Sidecar;
            }),
            ("webrtc.tls_cert", |c| c.webrtc.tls_cert = "/c.pem".into()),
            ("webrtc.tls_cert", |c| c.webrtc.tls_key = "/k.pem".into()),
        ];
        for (field, mutate) in cases {
            let mut cfg = Config::default();
            mutate(&mut cfg);
            match cfg.validate() {
                Err(ConfigValidationError::Unsupported { field: got, .. }) => {
                    assert_eq!(got, *field, "wrong field reported");
                }
                other => panic!("{field}: expected Unsupported, got {other:?}"),
            }
        }
        // The same toggles pass once the build advertises support.
        let all = BuildSupport {
            sip_quic: true,
            mcp_http3: true,
            webtransport: true,
            wireguard: true,
            vector_sidecar: true,
            recording_sidecar: true,
            webrtc_tls: true,
        };
        for (field, mutate) in cases {
            let mut cfg = Config::default();
            mutate(&mut cfg);
            assert!(
                cfg.validate_with(&all).is_ok(),
                "{field} should pass with full BuildSupport"
            );
        }
    }

    // -----------------------------------------------------------------
    // Hot-reload classification completeness
    // -----------------------------------------------------------------

    /// Collect every leaf path of a serialized config (`a.b.c`).
    /// Arrays and scalars are leaves; objects recurse.
    fn leaf_paths(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let path = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    leaf_paths(child, &path, out);
                }
            }
            _ => out.push(prefix.to_owned()),
        }
    }

    type FieldMutator = fn(&mut Config);

    /// One mutator per config leaf. `every_config_field_changes_the_apply_report`
    /// walks the serialized default config and fails when a field
    /// appears there without an entry here, so adding a knob forces
    /// the author to prove `apply_report` sees it.
    #[allow(clippy::too_many_lines)] // one row per config field, by design
    fn field_mutators() -> Vec<(&'static str, FieldMutator)> {
        vec![
            ("core.worker_threads", |c| c.core.worker_threads = 7),
            ("observability.log_level", |c| {
                c.observability.log_level = "trace".into();
            }),
            ("observability.log_format", |c| {
                c.observability.log_format = LogFormat::Pretty;
            }),
            ("observability.health_bind", |c| {
                c.observability.health_bind.set_port(1);
            }),
            ("observability.pcap_dir", |c| {
                c.observability.pcap_dir = Some("/p".into());
            }),
            ("sip.bind", |c| {
                c.sip
                    .bind
                    .push(BindSpec::Addr("127.0.0.1:1".parse().unwrap()));
            }),
            ("sip.transports", |c| {
                c.sip.transports.push(SipTransport::Tcp);
            }),
            ("sip.drain_timeout_secs", |c| c.sip.drain_timeout_secs += 1),
            ("sip.tls_cert_path", |c| {
                c.sip.tls_cert_path = Some("/c".into());
            }),
            ("sip.tls_key_path", |c| {
                c.sip.tls_key_path = Some("/k".into());
            }),
            ("sip.rate_limit.per_sec", |c| c.sip.rate_limit.per_sec = 5),
            ("sip.rate_limit.burst", |c| c.sip.rate_limit.burst = 5),
            ("sip.proxy.mode", |c| c.sip.proxy.mode = ProxyMode::Socks5),
            ("sip.proxy.address", |c| {
                c.sip.proxy.address = Some("127.0.0.1:1".parse().unwrap());
            }),
            ("sip.proxy.username", |c| {
                c.sip.proxy.username = Some("u".into());
            }),
            ("sip.proxy.password", |c| {
                c.sip.proxy.password = Some("p".into());
            }),
            ("sip.vpn.mode", |c| c.sip.vpn.mode = VpnMode::Wireguard),
            ("sip.vpn.private_key", |c| {
                c.sip.vpn.private_key = Some("k".into());
            }),
            ("sip.vpn.peer_public_key", |c| {
                c.sip.vpn.peer_public_key = Some("k".into());
            }),
            ("sip.vpn.peer_endpoint", |c| {
                c.sip.vpn.peer_endpoint = Some("e".into());
            }),
            ("sip.vpn.allowed_ips", |c| {
                c.sip.vpn.allowed_ips.push("10.0.0.0/8".into());
            }),
            ("sip.vpn.interface_ip", |c| {
                c.sip.vpn.interface_ip = Some("i".into());
            }),
            ("sip.conference_prefix", |c| {
                c.sip.conference_prefix = Some("conf".into());
            }),
            ("sip.session_timer_enabled", |c| {
                c.sip.session_timer_enabled = false;
            }),
            ("sip.session_expires_secs", |c| {
                c.sip.session_expires_secs += 1;
            }),
            ("sip.min_se_secs", |c| c.sip.min_se_secs += 1),
            ("sip.max_call_duration_secs", |c| {
                c.sip.max_call_duration_secs = 1;
            }),
            ("mcp.enabled_http", |c| c.mcp.enabled_http = true),
            ("mcp.http_bind", |c| c.mcp.http_bind.set_port(1)),
            ("mcp.rate_limit.per_sec", |c| c.mcp.rate_limit.per_sec = 5),
            ("mcp.rate_limit.burst", |c| c.mcp.rate_limit.burst = 5),
            ("mcp.http3.enabled", |c| c.mcp.http3.enabled = true),
            ("mcp.http3.bind", |c| c.mcp.http3.bind.set_port(1)),
            ("mcp.http.bearer_token", |c| {
                c.mcp.http.bearer_token = Some("t".into());
            }),
            ("a2a.enabled", |c| c.a2a.enabled = true),
            ("a2a.bind", |c| c.a2a.bind.set_port(1)),
            ("a2a.bearer_token", |c| {
                c.a2a.bearer_token = Some("t".into());
            }),
            ("plugins.dir", |c| c.plugins.dir = "/other".into()),
            ("plugins.sandbox.max_fds", |c| {
                c.plugins.sandbox.max_fds = Some(1);
            }),
            ("plugins.sandbox.max_memory_bytes", |c| {
                c.plugins.sandbox.max_memory_bytes = Some(1);
            }),
            ("plugins.sandbox.max_cpu_seconds", |c| {
                c.plugins.sandbox.max_cpu_seconds = Some(1);
            }),
            ("plugins.sandbox.max_processes", |c| {
                c.plugins.sandbox.max_processes = Some(1);
            }),
            ("plugins.sandbox.no_new_privs", |c| {
                c.plugins.sandbox.no_new_privs = true;
            }),
            ("plugins.sandbox.seccomp", |c| {
                c.plugins.sandbox.seccomp = SeccompPolicy::Allowlist;
            }),
            ("plugins.sandbox.seccomp_extra_allow", |c| {
                c.plugins.sandbox.seccomp_extra_allow.push("statx".into());
            }),
            ("plugins.call_event_hooks", |c| {
                c.plugins.call_event_hooks.push("x".into());
            }),
            ("plugins.wasm.memory_limit_mb", |c| {
                c.plugins.wasm.memory_limit_mb += 1;
            }),
            ("plugins.wasm.invoke_timeout_ms", |c| {
                c.plugins.wasm.invoke_timeout_ms += 1;
            }),
            ("auth.backend", |c| c.auth.backend = AuthBackend::Sqlite),
            ("auth.realm", |c| c.auth.realm = "other".into()),
            ("auth.sqlite.path", |c| c.auth.sqlite.path = "/db".into()),
            ("auth.http.endpoint", |c| {
                c.auth.http.endpoint = "http://x".into();
            }),
            ("auth.http.timeout_ms", |c| c.auth.http.timeout_ms += 1),
            ("auth.http.retries", |c| c.auth.http.retries += 1),
            ("auth.http.bearer_token", |c| {
                c.auth.http.bearer_token = Some("t".into());
            }),
            ("auth.http.breaker_threshold", |c| {
                c.auth.http.breaker_threshold += 1;
            }),
            ("auth.http.breaker_cooldown_secs", |c| {
                c.auth.http.breaker_cooldown_secs += 1;
            }),
            ("auth.http.failure_mode", |c| {
                c.auth.http.failure_mode = HttpFailureMode::FailOpen;
            }),
            ("storage.backend", |c| {
                c.storage.backend = StorageBackend::Sqlite;
            }),
            ("storage.sqlite.path", |c| {
                c.storage.sqlite.path = "/db".into();
            }),
            ("storage.vector.backend", |c| {
                c.storage.vector.backend = VectorBackend::Memory;
            }),
            ("storage.vector.plugin", |c| {
                c.storage.vector.plugin = Some("p".into());
            }),
            ("storage.recording.backend", |c| {
                c.storage.recording.backend = RecordingBackend::Fs;
            }),
            ("storage.recording.fs.root", |c| {
                c.storage.recording.fs.root = "/rec".into();
            }),
            ("storage.recording.retention_days", |c| {
                c.storage.recording.retention_days += 1;
            }),
            ("storage.recording.plugin", |c| {
                c.storage.recording.plugin = Some("p".into());
            }),
            ("media.inband_dtmf", |c| c.media.inband_dtmf = true),
            ("media.prompts.root", |c| {
                c.media.prompts.root = "/prompts".into();
            }),
            ("media.prompts.capacity", |c| c.media.prompts.capacity += 1),
            ("media.transcode.max_concurrent_calls", |c| {
                c.media.transcode.max_concurrent_calls += 1;
            }),
            ("media.transcode.cpu_budget_ms_per_call", |c| {
                c.media.transcode.cpu_budget_ms_per_call += 1;
            }),
            ("media.rtp_ports", |c| {
                c.media.rtp_ports = Some(RtpPortRange { min: 1, max: 9 });
            }),
            ("media.advertise_ip", |c| {
                c.media.advertise_ip = Some("1.2.3.4".into());
            }),
            ("ai.openai_api_key", |c| {
                c.ai.openai_api_key = Some("k".into());
            }),
            ("ai.anthropic_api_key", |c| {
                c.ai.anthropic_api_key = Some("k".into());
            }),
            ("webtransport.enabled", |c| c.webtransport.enabled = true),
            ("webtransport.bind", |c| c.webtransport.bind.set_port(1)),
            ("webtransport.cert_path", |c| {
                c.webtransport.cert_path = "/c".into();
            }),
            ("webtransport.key_path", |c| {
                c.webtransport.key_path = "/k".into();
            }),
            ("reload.enabled", |c| c.reload.enabled = !c.reload.enabled),
            ("reload.max_frequency_s", |c| c.reload.max_frequency_s += 1),
            ("canary.deadline_s", |c| c.canary.deadline_s += 1),
            ("canary.plugin_error_rate_ceiling", |c| {
                c.canary.plugin_error_rate_ceiling = 0.1;
            }),
            ("canary.sip_parse_errors_per_sec_ceiling", |c| {
                c.canary.sip_parse_errors_per_sec_ceiling += 1;
            }),
            ("webrtc.enabled", |c| c.webrtc.enabled = true),
            ("webrtc.ws_bind", |c| c.webrtc.ws_bind.set_port(1)),
            ("webrtc.tls_cert", |c| c.webrtc.tls_cert = "/c".into()),
            ("webrtc.tls_key", |c| c.webrtc.tls_key = "/k".into()),
            ("webrtc.privacy.mode", |c| {
                c.webrtc.privacy.mode = WebRtcPrivacyMode::RelayOnly;
            }),
            ("webrtc.privacy.redaction_key", |c| {
                c.webrtc.privacy.redaction_key = "k".into();
            }),
            ("webrtc.ice.enabled", |c| c.webrtc.ice.enabled = true),
            ("webrtc.ice.host_binds", |c| {
                c.webrtc.ice.host_binds.push("127.0.0.1:1".parse().unwrap());
            }),
            ("webrtc.ice.stun_servers", |c| {
                c.webrtc
                    .ice
                    .stun_servers
                    .push("127.0.0.1:1".parse().unwrap());
            }),
            ("webrtc.turn.enabled", |c| c.webrtc.turn.enabled = true),
            ("webrtc.turn.bind", |c| c.webrtc.turn.bind.set_port(1)),
            ("webrtc.turn.realm", |c| c.webrtc.turn.realm = "r".into()),
            ("webrtc.turn.relay_ip", |c| {
                c.webrtc.turn.relay_ip = Some("1.2.3.4".parse().unwrap());
            }),
            ("webrtc.turn.allocation_lifetime_s", |c| {
                c.webrtc.turn.allocation_lifetime_s += 1;
            }),
            ("webrtc.turn.credentials", |c| {
                c.webrtc.turn.credentials.push(WebRtcTurnCredential {
                    username: "u".into(),
                    password: "p".into(),
                });
            }),
            ("webrtc.turn.external_url", |c| {
                c.webrtc.turn.external_url = "turn:x".into();
            }),
            ("cluster.mode", |c| c.cluster.mode = ClusterMode::Primary),
            ("cluster.peer_addr", |c| {
                c.cluster.peer_addr = Some("127.0.0.1:1".parse().unwrap());
            }),
            ("cluster.heartbeat_interval_secs", |c| {
                c.cluster.heartbeat_interval_secs += 1;
            }),
            ("cluster.raft_dir", |c| c.cluster.raft_dir = "/raft".into()),
            ("cluster.node_id", |c| c.cluster.node_id += 1),
            ("cluster.raft_addr", |c| {
                c.cluster.raft_addr = Some("127.0.0.1:1".parse().unwrap());
            }),
            ("cluster.initial_peers", |c| {
                c.cluster.initial_peers.push("2@h:1".into());
            }),
        ]
    }

    /// Nothing in the config is outside the hot-reload universe:
    /// every serialized leaf has a mutator, and every mutator
    /// lands the field in `reloaded` or `restart_required`.
    #[test]
    fn every_config_field_changes_the_apply_report() {
        let base = Config::default();
        let json = serde_json::to_value(&base).unwrap();
        let mut leaves = Vec::new();
        leaf_paths(&json, "", &mut leaves);
        let mutators = field_mutators();
        for leaf in &leaves {
            assert!(
                mutators.iter().any(|(p, _)| p == leaf),
                "config field `{leaf}` has no mutator in `field_mutators` — \
                 add one so its hot-reload classification is tested"
            );
        }
        for (path, mutate) in mutators {
            let mut candidate = base.clone();
            mutate(&mut candidate);
            let report = base.apply_report(&candidate);
            assert!(
                !report.is_noop(),
                "changing `{path}` produced an empty ApplyReport — \
                 the field would be silently ignored on reload"
            );
        }
    }
}
