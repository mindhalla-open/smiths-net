//! Media-plane trait seams.
//!
//! `smiths-sip` drives dialogs but **never** touches sockets, RTP
//! frames, or concrete bridge types. It asks a [`MediaFabric`] for
//! endpoints (abstracted through the [`MediaEndpoint`] trait) and for
//! forwarding sessions between two endpoints. The concrete UDP
//! implementation lives in `smiths-media`; future implementations
//! (ICE-gathering endpoints, SRTP, mixer, transcoder) swap in here
//! without signaling-layer changes.
//!
//! This is the MVP guardrail from `docs/architecture/04-post-mvp-scope.md`
//! — `MediaEndpoint` / `MediaSession` / `MediaFabric` abstractions,
//! realized as token handles ([`EndpointId`] / [`BridgeId`]) so cross-
//! process lookups and dialog snapshots stay serializable.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::sdp::SrtpKeys;

/// Opaque handle to an engine-allocated media endpoint.
///
/// Issued by [`MediaFabric::allocate`]; retained by the Call FSM / UAS
/// and passed back to bridge / release. Serializable so a dialog
/// snapshot can round-trip through the HA path.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EndpointId(pub u64);

/// Opaque handle to a live media session (today: a two-leg forwarder).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BridgeId(pub u64);

/// How an [`MediaEndpoint`] obtained its reachable address.
///
/// MVP only emits [`Self::Host`]. The variants for
/// [`Self::ServerReflexive`] and [`Self::Relayed`] are the guardrail
/// for ICE/STUN/TURN: when that lands, new endpoint types implementing
/// [`MediaEndpoint`] slot in without touching the SIP layer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    /// Local UDP socket bound on a host interface.
    Host,
    /// Public address learned via STUN binding.
    ServerReflexive,
    /// TURN-relay allocation.
    Relayed,
}

/// Capability interface for one engine-side media endpoint.
///
/// A `MediaEndpoint` advertises the data SDP negotiation needs
/// (`local_addr`, `id`, `kind`). Specialisations add their own
/// lifecycle or gathering logic; for today's host-candidate UDP
/// endpoint the default [`Endpoint`] impl is sufficient.
pub trait MediaEndpoint: Send + Sync {
    /// Opaque ID the fabric issued for this endpoint.
    fn id(&self) -> EndpointId;
    /// Address the far peer should direct RTP at.
    fn local_addr(&self) -> SocketAddr;
    /// How the address above was obtained.
    fn kind(&self) -> EndpointKind;
}

/// Default host-candidate endpoint. Returned by MVP UDP fabric.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    /// Stable opaque id the fabric hands out for RTP allocations.
    pub id: EndpointId,
    /// Bound UDP address (the "RTP port") announced in SDP.
    pub local_addr: SocketAddr,
    /// Companion RTCP port (RFC 3550 §11: RTP port is even, RTCP is
    /// RTP+1). `None` when the fabric did not allocate a paired port.
    pub rtcp_addr: Option<SocketAddr>,
}

impl MediaEndpoint for Endpoint {
    fn id(&self) -> EndpointId {
        self.id
    }
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    fn kind(&self) -> EndpointKind {
        EndpointKind::Host
    }
}

/// Lifecycle of one forwarding session between two endpoints.
///
/// Today's implementation is the byte-level SSRC-rewriting bridge
/// in `smiths-media::bridge`. Future non-RTP sessions (T.38 relay,
/// WebTransport bridge, full RTP mixer) implement the same trait so
/// the fabric can multiplex session types without changing the
/// signaling layer.
#[async_trait]
pub trait MediaSession: Send + Sync {
    /// Opaque ID this session was filed under.
    fn id(&self) -> BridgeId;
    /// Halt forwarding. Idempotent. Does not consume `self`, so
    /// callers holding `Arc<dyn MediaSession>` can issue `stop()`
    /// without dismantling other holders' references.
    async fn stop(&self);
}

/// SRTP cipher suites the engine understands on the wire.
///
/// Today we support **`AES_CM_128_HMAC_SHA1_80`** only — the baseline
/// SDES / DTLS-SRTP profile every mainstream UA offers and the one
/// all of our current test tooling emits. Adding further suites is a
/// matter of registering a new variant and wiring the corresponding
/// `webrtc-srtp` `ProtectionProfile` in `smiths-media::srtp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SrtpSuite {
    /// RFC 4568 §6.2.1 — 128-bit AES counter mode with 80-bit HMAC
    /// SHA-1 authentication. 30-byte SDES key material
    /// (16-byte master key + 14-byte master salt).
    AesCm128HmacSha1_80,
}

impl SrtpSuite {
    /// Length of the master key in bytes.
    #[must_use]
    pub const fn key_len(self) -> usize {
        match self {
            Self::AesCm128HmacSha1_80 => 16,
        }
    }

    /// Length of the master salt in bytes.
    #[must_use]
    pub const fn salt_len(self) -> usize {
        match self {
            Self::AesCm128HmacSha1_80 => 14,
        }
    }

    /// Total SDES key-material length (`key_len + salt_len`). This is
    /// what `base64`-decodes out of an `a=crypto:... inline:...`.
    #[must_use]
    pub const fn key_material_len(self) -> usize {
        self.key_len() + self.salt_len()
    }

    /// Canonical wire name used in SDP `a=crypto:` (RFC 4568 §6.2).
    #[must_use]
    pub const fn sdp_name(self) -> &'static str {
        match self {
            Self::AesCm128HmacSha1_80 => "AES_CM_128_HMAC_SHA1_80",
        }
    }

    /// Parse an SDP suite name. Unknown names return `None` so the
    /// SDP negotiator can surface them as a rejected offer.
    #[must_use]
    pub fn from_sdp_name(name: &str) -> Option<Self> {
        match name {
            "AES_CM_128_HMAC_SHA1_80" => Some(Self::AesCm128HmacSha1_80),
            _ => None,
        }
    }
}

/// Errors surfaced by an [`SrtpTransform`].
#[derive(Debug, Error)]
pub enum SrtpError {
    /// Master key/salt length didn't match the suite's expectations.
    /// Usually means an SDES line carried the wrong suite tag.
    #[error("SRTP key material wrong size: expected {expected}, got {got}")]
    KeyLength {
        /// Bytes the suite requires (key + salt).
        expected: usize,
        /// Bytes the caller supplied.
        got: usize,
    },
    /// Packet's HMAC didn't verify against the key — either wrong
    /// keys or tampering.
    #[error("SRTP authentication failed")]
    AuthFailed,
    /// Any other transform-layer failure (encrypt/decrypt returned an
    /// error the backend didn't classify more specifically).
    #[error("SRTP transform error: {0}")]
    Other(String),
}

/// Per-direction SRTP encryption/decryption primitive.
///
/// Exposed as a trait so the engine can swap backends (pure-Rust
/// `webrtc-srtp` today; `libsrtp` FFI or an HSM-backed variant
/// tomorrow) without touching the bridge. **Per-direction**: one
/// transform instance for the local→peer stream, another for
/// peer→local. Sharing a transform across directions mixes SSRC
/// state machines and breaks SRTP's rollover counter accounting.
///
/// `&self` methods with interior mutability: SRTP contexts track
/// per-SSRC state (ROC, replay detector) that mutates per packet, so
/// the bridge needs shared access from multiple tasks. Implementations
/// wrap a `Mutex<Context>` or equivalent.
///
/// **Not a plugin.** Per-packet crypto on a 20 ms RTP frame is the
/// kind of hot path that can't tolerate a plugin hop. The plugin
/// model is reserved for AI / control-plane async RPC.
pub trait SrtpTransform: Send + Sync {
    /// Encrypt an outgoing RTP packet. Returns the ciphertext + auth
    /// tag as a fresh buffer (may be longer than the input).
    fn protect_rtp(&self, plaintext: &[u8]) -> Result<Vec<u8>, SrtpError>;

    /// Decrypt and authenticate an incoming RTP packet. Returns the
    /// plaintext (same shape as the original pre-encryption packet).
    /// Auth-tag failures surface as [`SrtpError::AuthFailed`] — the
    /// bridge drops the packet on that path without touching further
    /// state.
    fn unprotect_rtp(&self, ciphertext: &[u8]) -> Result<Vec<u8>, SrtpError>;
}

/// One side of a bridge request.
///
/// Split out from the previous 4-arg `bridge(a, peer_a, b, peer_b)`
/// signature so the two newly optional pieces — per-leg SRTP keying
/// material today, per-leg codec transcoding later — can be added
/// without inflating the argument list.
#[derive(Clone, Debug)]
pub struct BridgeLeg {
    /// Local endpoint (previously allocated via [`MediaFabric::allocate`]).
    pub endpoint: EndpointId,
    /// Address of the remote peer for this leg (RTP). Derived from the
    /// offer/answer exchange.
    pub peer: SocketAddr,
    /// SRTP keys to apply on this leg. `None` = plain RTP passthrough.
    pub srtp: Option<SrtpKeys>,
}

impl BridgeLeg {
    /// Construct a plain-RTP leg (no SRTP).
    #[must_use]
    pub const fn plain(endpoint: EndpointId, peer: SocketAddr) -> Self {
        Self {
            endpoint,
            peer,
            srtp: None,
        }
    }

    /// Construct an SRTP-protected leg.
    #[must_use]
    pub const fn with_srtp(endpoint: EndpointId, peer: SocketAddr, srtp: SrtpKeys) -> Self {
        Self {
            endpoint,
            peer,
            srtp: Some(srtp),
        }
    }
}

/// Errors surfaced by [`MediaFabric`] operations.
#[derive(Debug, Error)]
pub enum MediaError {
    /// Underlying I/O failure (socket bind, etc.).
    #[error("media I/O: {0}")]
    Io(#[from] std::io::Error),
    /// The caller supplied an [`EndpointId`] the fabric does not know.
    #[error("unknown media endpoint: {0:?}")]
    UnknownEndpoint(EndpointId),
    /// The caller supplied a [`BridgeId`] the fabric does not know.
    /// Idempotent shutdowns treat this as success, so this is for
    /// genuinely programmer-error paths.
    #[error("unknown media bridge: {0:?}")]
    UnknownBridge(BridgeId),
    /// Port allocation exhausted its search window.
    #[error("port allocator exhausted: {0}")]
    PortExhausted(String),
}

/// Media-plane factory + lifecycle. Implemented by `smiths-media`.
///
/// All methods are `async` because binding sockets and awaiting
/// forwarder shutdown are naturally async. `release_*` methods are
/// idempotent and return `()` — observers shouldn't care whether the
/// handle was already gone, only that it is gone now.
#[async_trait]
pub trait MediaFabric: Send + Sync {
    /// Allocate a fresh local media endpoint (today: a UDP socket pair
    /// for RTP + RTCP bound on `bind_ip`). The fabric retains the
    /// underlying resources; callers only see the opaque
    /// [`EndpointId`] wrapped in an [`MediaEndpoint`] capability.
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError>;

    /// Start forwarding bytes between two endpoints. Each [`BridgeLeg`]
    /// carries the local endpoint, the remote RTP address learned from
    /// SDP, and optional SRTP keying material. Returns a [`BridgeId`];
    /// drop via [`MediaFabric::release_bridge`].
    ///
    /// Passing `srtp = Some(_)` on one leg and `None` on the other is
    /// legal — the fabric encrypts/decrypts only where keys are
    /// present (useful for half-SRTP gateway scenarios).
    async fn bridge(&self, a: BridgeLeg, b: BridgeLeg) -> Result<BridgeId, MediaError>;

    /// Stop a bridge and drop its forwarder tasks. No-op if `id` is
    /// unknown (e.g. already released by the paired side).
    async fn release_bridge(&self, id: BridgeId);

    /// Drop an allocated endpoint. No-op if unknown.
    async fn release_endpoint(&self, id: EndpointId);

    /// Send a raw UDP payload out through the endpoint's RTP socket.
    /// The caller provides a complete RTP packet (or any other
    /// datagram). Returns `UnknownEndpoint` when `src` was never
    /// allocated or has been released. This is the primitive behind
    /// engine-side audio injection (`speak`).
    async fn send_packet(
        &self,
        src: EndpointId,
        dest: SocketAddr,
        bytes: &[u8],
    ) -> Result<(), MediaError>;
}

/// Cross-subsystem handle the SIP UAS uses to participate in
/// the WebRTC tag-based rendezvous (slice 5.10-sipjoin).
///
/// A SIP INVITE carrying `X-Smiths-Webrtc-Tag: <tag>` asks the
/// engine to bridge this dialog with a WebRTC leg sharing the
/// same tag. The UAS calls [`Self::pair_sip_leg`] after
/// negotiating its own SDP; the impl (living in the CLI's
/// `CliWebRtcHandler`) either pairs with a pre-parked WebRTC
/// partner (installs a bridge + returns `Some(BridgeId)`) or
/// parks the SIP leg under `tag` awaiting its WebRTC half.
///
/// Lives in `smiths-core` so the UAS doesn't have to depend
/// on `smiths-cli` — both sides see only the trait.
#[async_trait]
pub trait WebRtcRendezvous: Send + Sync {
    /// Pair or park a SIP leg under `tag`.
    ///
    /// - **Partner present** — installs the bridge between
    ///   this SIP leg and the parked WebRTC leg, returns
    ///   `Ok(Some(bridge_id))`. The UAS retains the
    ///   `BridgeId` under its dialog so BYE tears the pair
    ///   down.
    /// - **No partner** — parks the SIP leg with a deadline
    ///   evictor, returns `Ok(None)`. The UAS still holds
    ///   its endpoint; when the WebRTC partner arrives, the
    ///   WebRTC handler reaches back through the same trait
    ///   (different impl method) to retrieve the SIP leg's
    ///   endpoint + install the bridge. Today that
    ///   "WebRTC-finds-SIP" path is implemented inside the
    ///   `CliWebRtcHandler`; SIP-finds-WebRTC (this call) is
    ///   the other half.
    ///
    /// On deadline: the parked leg is evicted + the metric
    /// `smiths_webrtc_sessions_paired_total{partner="none"}`
    /// bumps, same as the WebRTC-parked case. Releasing the
    /// UAS's endpoint is the UAS's job (via the separately
    /// stored [`SipRendezvousTicket`]); the rendezvous trait
    /// doesn't own SIP dialogs.
    ///
    /// # Errors
    /// `String` — fabric allocation failure, unlikely in
    /// practice since the UAS already allocated the endpoint.
    async fn pair_sip_leg(
        &self,
        tag: &str,
        endpoint: EndpointId,
        peer: SocketAddr,
        srtp: Option<crate::SrtpKeys>,
    ) -> Result<Option<BridgeId>, String>;

    /// Release a SIP leg parked under `tag` (e.g., the SIP
    /// dialog sent BYE before the WebRTC partner arrived).
    /// Idempotent — a tag that's not parked is a no-op.
    async fn release_sip_leg(&self, tag: &str);
}
