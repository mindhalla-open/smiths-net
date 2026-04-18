//! Media-plane trait seam.
//!
//! `smiths-sip` drives dialogs but **never** touches sockets, RTP
//! frames, or concrete bridge types. It asks a [`MediaFabric`] for
//! endpoints (opaque [`EndpointId`] handles, resolved to a local
//! [`SocketAddr`] for SDP answer generation) and for bridges between
//! two endpoints. The concrete UDP implementation lives in
//! `smiths-media`; future implementations (mixer, transcoder, SRTP)
//! swap in here without signaling-layer changes.
//!
//! This is the MVP guardrail from `docs/architecture/04-post-mvp-scope.md`
//! — `MediaEndpoint` / `MediaFabric` abstractions, realized as token
//! handles so the interface is object-safe and serializable.

use std::net::{IpAddr, SocketAddr};

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

/// Opaque handle to a live bridge.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct BridgeId(pub u64);

/// Summary of a freshly-allocated endpoint.
#[derive(Clone, Copy, Debug)]
pub struct Endpoint {
    pub id: EndpointId,
    pub local_addr: SocketAddr,
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
}

/// Media-plane factory + lifecycle. Implemented by `smiths-media`.
///
/// All methods are `async` because binding sockets and awaiting
/// bridge-task shutdown are naturally async. `release_*` methods are
/// idempotent and return `()` — observers shouldn't care whether the
/// handle was already gone, only that it is gone now.
#[async_trait]
pub trait MediaFabric: Send + Sync {
    /// Allocate a fresh local media endpoint (today: a UDP socket
    /// bound on `bind_ip:0`). The fabric retains the underlying
    /// resource; callers only see the opaque [`EndpointId`].
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Endpoint, MediaError>;

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
}
