//! Concrete [`FaxOrchestrator`] that builds a [`UdptlSession`]
//! when two legs re-INVITE into T.38 (slice 5.6d-runtime).
//!
//! Pairs with the trait seam defined in `smiths-sip::uas`. The
//! UAS's future re-INVITE handler calls `try_orchestrate_fax`
//! with two [`BridgeLeg`]s (each carrying the local
//! [`EndpointId`] + the peer's freshly-learned UDPTL address);
//! this impl resolves both endpoints' UDP sockets through
//! [`UdpMediaFabric::endpoint_socket`] and spawns a
//! [`UdptlSession`] with them.

use std::sync::Arc;

use async_trait::async_trait;
use smiths_core::BridgeLeg;
use smiths_core::media::{BridgeId, MediaError, MediaSession};
use smiths_media::UdpMediaFabric;
use smiths_sip::FaxOrchestrator;

use crate::metrics::FaxMetrics;
use crate::session::{UdptlSession, UdptlSessionConfig};

/// Production [`FaxOrchestrator`] — wraps a
/// [`UdpMediaFabric`] and spawns [`UdptlSession`]s on demand.
///
/// Cheaply cloneable: two `Arc`s inside.
pub struct UdptlFaxOrchestrator {
    fabric: Arc<UdpMediaFabric>,
    metrics: Arc<FaxMetrics>,
    next_bridge_id: std::sync::atomic::AtomicU64,
}

impl UdptlFaxOrchestrator {
    /// Build an orchestrator bound to the given fabric + metrics.
    /// The metrics handle is the same one the FAX module's
    /// `register` hands out; feeding it here means spawned
    /// sessions increment the gauge + forwarding counters.
    #[must_use]
    pub fn new(fabric: Arc<UdpMediaFabric>, metrics: Arc<FaxMetrics>) -> Self {
        Self {
            fabric,
            metrics,
            next_bridge_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn fresh_bridge_id(&self) -> BridgeId {
        // Uses its own counter rather than the fabric's so
        // telemetry on UDPTL sessions doesn't collide with the
        // fabric's RTP-bridge counter (they live in different
        // spaces: fabric ids ↔ `media_bridges_active`; UDPTL
        // ids ↔ `smiths_fax_sessions_active`).
        BridgeId(
            self.next_bridge_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )
    }
}

#[async_trait]
impl FaxOrchestrator for UdptlFaxOrchestrator {
    async fn try_orchestrate_fax(
        &self,
        leg_a: BridgeLeg,
        leg_b: BridgeLeg,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        let Some(sock_a) = self.fabric.endpoint_socket(leg_a.endpoint) else {
            return Err(MediaError::UnknownEndpoint(leg_a.endpoint));
        };
        let Some(sock_b) = self.fabric.endpoint_socket(leg_b.endpoint) else {
            return Err(MediaError::UnknownEndpoint(leg_b.endpoint));
        };
        let cfg = UdptlSessionConfig {
            id: self.fresh_bridge_id(),
            trace_sequence: false,
            metrics: Some(Arc::clone(&self.metrics)),
        };
        let session = UdptlSession::spawn(cfg, (sock_a, leg_a.peer), (sock_b, leg_b.peer));
        Ok(Some(session as Arc<dyn MediaSession>))
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)] // term_a / term_b naming is load-bearing
mod tests {
    use super::*;
    use crate::udptl::UdptlPacket;
    use smiths_core::MediaFabric;
    use std::net::IpAddr;
    use std::time::Duration;
    use tokio::net::UdpSocket;
    use tokio::time::timeout;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orchestrator_spawns_session_that_relays_udptl() {
        // Two "fax terminals" on loopback. The engine allocates
        // two endpoints via the fabric; we ask the orchestrator
        // to wire them. Asserting a terminal's UDPTL datagram
        // reaches the other confirms the full path.
        let fabric = Arc::new(UdpMediaFabric::new());
        let metrics = FaxMetrics::noop();
        let orch = UdptlFaxOrchestrator::new(Arc::clone(&fabric), Arc::clone(&metrics));

        let ep_a = fabric.allocate(IpAddr::from([127, 0, 0, 1])).await.unwrap();
        let ep_b = fabric.allocate(IpAddr::from([127, 0, 0, 1])).await.unwrap();

        let term_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let term_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let term_a_addr = term_a.local_addr().unwrap();
        let term_b_addr = term_b.local_addr().unwrap();

        let session = orch
            .try_orchestrate_fax(
                BridgeLeg::plain(ep_a.id(), term_a_addr),
                BridgeLeg::plain(ep_b.id(), term_b_addr),
            )
            .await
            .unwrap()
            .expect("orchestrator returned Some");

        // Terminal A → engine → terminal B.
        let pkt = UdptlPacket {
            sequence: 1,
            primary: b"IFP primary".to_vec(),
            secondary: vec![],
        };
        term_a
            .send_to(&pkt.encode().unwrap(), ep_a.local_addr())
            .await
            .unwrap();

        let mut buf = [0_u8; 1500];
        let (n, from) = timeout(Duration::from_secs(1), term_b.recv_from(&mut buf))
            .await
            .expect("terminal B never received")
            .unwrap();
        assert_eq!(from, ep_b.local_addr(), "came from wrong leg");
        let back = UdptlPacket::parse(&buf[..n]).unwrap();
        assert_eq!(back, pkt, "UDPTL bytes mutated");

        session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn orchestrator_errors_on_unknown_endpoint() {
        let fabric = Arc::new(UdpMediaFabric::new());
        let metrics = FaxMetrics::noop();
        let orch = UdptlFaxOrchestrator::new(fabric, metrics);
        let result = orch
            .try_orchestrate_fax(
                BridgeLeg::plain(
                    smiths_core::EndpointId(99_999),
                    "127.0.0.1:1".parse().unwrap(),
                ),
                BridgeLeg::plain(
                    smiths_core::EndpointId(99_998),
                    "127.0.0.1:2".parse().unwrap(),
                ),
            )
            .await;
        match result {
            Err(MediaError::UnknownEndpoint(_)) => {}
            Err(e) => panic!("expected UnknownEndpoint, got {e:?}"),
            Ok(_) => panic!("expected UnknownEndpoint, got Ok(_)"),
        }
    }
}
