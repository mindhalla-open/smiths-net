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
    pub id: EndpointId,
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

    /// Start forwarding bytes between two endpoints. Each endpoint's
    /// `peer_*` is the remote RTP address learned from SDP. Returns a
    /// [`BridgeId`]; drop via [`MediaFabric::release_bridge`].
    async fn bridge(
        &self,
        a: EndpointId,
        peer_a: SocketAddr,
        b: EndpointId,
        peer_b: SocketAddr,
    ) -> Result<BridgeId, MediaError>;

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
