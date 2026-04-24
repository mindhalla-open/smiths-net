//! [`MediaFabric`] wrapper that layers conferencing on top of the
//! UDP fabric.
//!
//! The router (UAS) picks between the default UDP fabric and
//! [`MixerFabric`] per call: a 2-peer call uses the UDP path
//! unchanged, a conference participant is joined to a mixer. The
//! fabric is a thin delegator — all `MediaFabric::*` methods pass
//! through to the wrapped [`UdpMediaFabric`]; the new surface lives
//! in the [`MixerFabric`] inherent methods.
//!
//! Bridge integration (binding a [`crate::Conference`] to live RTP
//! sockets via `MediaFabric::bridge`) is the same FSM refactor
//! deferred in slices 5.3 and 5.4 — the fabric stays compile-clean
//! here and the wiring lands alongside the shared follow-on.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, BridgeLeg, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use smiths_media::UdpMediaFabric;

use crate::conference::{ConferenceConfig, ConferenceId, ParticipantFrame, ParticipantId};
use crate::registry::{ConferenceRegistry, ConferenceRegistryError, InMemoryConferenceRegistry};

/// `MediaFabric` with conferencing extensions.
pub struct MixerFabric {
    inner: Arc<UdpMediaFabric>,
    conferences: Arc<dyn ConferenceRegistry>,
}

impl MixerFabric {
    /// Wrap an existing [`UdpMediaFabric`] and pair it with a fresh
    /// in-memory conference registry. The vast majority of
    /// deployments want this constructor.
    #[must_use]
    pub fn new(inner: Arc<UdpMediaFabric>) -> Self {
        Self {
            inner,
            conferences: Arc::new(InMemoryConferenceRegistry::new()),
        }
    }

    /// Wrap an existing UDP fabric with a custom registry impl.
    /// Useful for tests that want to assert on registry events or
    /// future multi-node deployments.
    #[must_use]
    pub fn with_registry(
        inner: Arc<UdpMediaFabric>,
        conferences: Arc<dyn ConferenceRegistry>,
    ) -> Self {
        Self { inner, conferences }
    }

    /// Shared reference to the underlying UDP fabric — exposed so
    /// callers that hold a `MixerFabric` can still reach the plain
    /// 2-peer bridge API when they need to.
    #[must_use]
    pub fn udp(&self) -> &Arc<UdpMediaFabric> {
        &self.inner
    }

    /// Shared reference to the conference registry. MCP tools
    /// (`create_conference` / `join_conference` / `leave_conference`)
    /// call through here.
    #[must_use]
    pub fn conferences(&self) -> &Arc<dyn ConferenceRegistry> {
        &self.conferences
    }

    /// Create a conference; convenience wrapper delegating to the
    /// registry.
    pub async fn create_conference(&self, cfg: ConferenceConfig) -> ConferenceId {
        self.conferences.create(cfg).await
    }

    /// Join a conference; convenience wrapper delegating to the
    /// registry.
    ///
    /// # Errors
    /// [`ConferenceRegistryError`] — see variant docs.
    pub async fn join_conference(
        &self,
        conf: ConferenceId,
    ) -> Result<
        (ParticipantId, tokio::sync::mpsc::Receiver<ParticipantFrame>),
        ConferenceRegistryError,
    > {
        self.conferences.join(conf).await
    }

    /// Leave a conference; convenience wrapper.
    ///
    /// # Errors
    /// [`ConferenceRegistryError`] — see variant docs.
    pub async fn leave_conference(
        &self,
        conf: ConferenceId,
        participant: ParticipantId,
    ) -> Result<(), ConferenceRegistryError> {
        self.conferences.leave(conf, participant).await
    }
}

#[async_trait]
impl MediaFabric for MixerFabric {
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
        self.inner.allocate(bind_ip).await
    }

    async fn bridge(&self, a: BridgeLeg, b: BridgeLeg) -> Result<BridgeId, MediaError> {
        self.inner.bridge(a, b).await
    }

    async fn release_bridge(&self, id: BridgeId) {
        self.inner.release_bridge(id).await;
    }

    async fn release_endpoint(&self, id: EndpointId) {
        self.inner.release_endpoint(id).await;
    }

    async fn send_packet(
        &self,
        src: EndpointId,
        dest: SocketAddr,
        bytes: &[u8],
    ) -> Result<(), MediaError> {
        self.inner.send_packet(src, dest, bytes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fabric_delegates_allocate_to_udp() {
        let fab = MixerFabric::new(Arc::new(UdpMediaFabric::new()));
        let ep = fab
            .allocate(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
            .await
            .unwrap();
        assert_eq!(
            ep.local_addr().ip(),
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        assert_ne!(ep.local_addr().port(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fabric_conference_round_trip() {
        let fab = MixerFabric::new(Arc::new(UdpMediaFabric::new()));
        let id = fab.create_conference(ConferenceConfig::default()).await;
        let (p1, _rx1) = fab.join_conference(id).await.unwrap();
        fab.leave_conference(id, p1).await.unwrap();
    }
}
