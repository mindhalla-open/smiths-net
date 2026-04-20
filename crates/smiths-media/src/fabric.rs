//! [`MediaFabric`] implementation on plain UDP.
//!
//! Owns all media sockets, one pair per endpoint (even RTP / odd
//! RTCP). Hands out [`MediaEndpoint`] trait objects to the signaling
//! layer, which never touches a socket directly. On
//! [`MediaFabric::bridge`], spins up the SSRC-rewriting forwarder
//! from [`crate::bridge`] and retains the [`Bridge`] so that a later
//! `release_bridge` call can await its shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use smiths_core::Metrics;
use smiths_core::media::{BridgeId, Endpoint, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use tokio::net::UdpSocket;
use tracing::{debug, instrument};

use crate::bridge::{Bridge, BridgeConfig, Leg, RtcpLeg};
use crate::port_allocator::{DEFAULT_MAX_ATTEMPTS, allocate_rtp_rtcp_pair};

/// Derive the peer's RTCP socket address from its RTP address per
/// RFC 3550 §11 (even RTP / odd RTCP, i.e. `port + 1`). This is the
/// standard convention when SDP doesn't carry an explicit `a=rtcp:`
/// attribute — which our minimal SDP generator doesn't.
fn peer_rtcp_from_rtp(peer_rtp: SocketAddr) -> SocketAddr {
    let mut out = peer_rtp;
    out.set_port(peer_rtp.port().wrapping_add(1));
    out
}

/// RTP + RTCP socket pair the fabric owns for one endpoint.
struct EndpointSockets {
    rtp: Arc<UdpSocket>,
    /// RTCP socket paired with `rtp` (port = `rtp_port` + 1). Bridges
    /// spawned after v0.11.0 use it to emit periodic Sender Reports.
    rtcp: Arc<UdpSocket>,
}

/// Default UDP-backed [`MediaFabric`].
#[derive(Default)]
pub struct UdpMediaFabric {
    next_endpoint: AtomicU64,
    next_bridge: AtomicU64,
    endpoints: DashMap<EndpointId, EndpointSockets>,
    bridges: DashMap<BridgeId, Bridge>,
    /// Shared metrics handle. `None` on test fabrics; the CLI wires
    /// the engine-wide `Arc<Metrics>` via [`Self::with_metrics`].
    metrics: Option<Arc<Metrics>>,
}

impl UdpMediaFabric {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a metrics handle. Builder-style so existing tests can
    /// keep using `new()` without changes.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn fresh_endpoint_id(&self) -> EndpointId {
        EndpointId(self.next_endpoint.fetch_add(1, Ordering::Relaxed))
    }

    fn fresh_bridge_id(&self) -> BridgeId {
        BridgeId(self.next_bridge.fetch_add(1, Ordering::Relaxed))
    }
}

#[async_trait]
impl MediaFabric for UdpMediaFabric {
    #[instrument(skip(self), fields(%bind_ip))]
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
        let pair = allocate_rtp_rtcp_pair(bind_ip, DEFAULT_MAX_ATTEMPTS).await?;
        let id = self.fresh_endpoint_id();
        let rtp_addr = pair.rtp_addr;
        let rtcp_addr = pair.rtcp_addr;
        self.endpoints.insert(
            id,
            EndpointSockets {
                rtp: Arc::new(pair.rtp),
                rtcp: Arc::new(pair.rtcp),
            },
        );
        debug!(?id, %rtp_addr, %rtcp_addr, "media endpoint allocated");
        Ok(Arc::new(Endpoint {
            id,
            local_addr: rtp_addr,
            rtcp_addr: Some(rtcp_addr),
        }))
    }

    #[instrument(skip(self), fields(?a, ?b, %peer_a, %peer_b))]
    async fn bridge(
        &self,
        a: EndpointId,
        peer_a: SocketAddr,
        b: EndpointId,
        peer_b: SocketAddr,
    ) -> Result<BridgeId, MediaError> {
        let (sock_a, rtcp_a) = {
            let entry = self
                .endpoints
                .get(&a)
                .ok_or(MediaError::UnknownEndpoint(a))?;
            (Arc::clone(&entry.rtp), Arc::clone(&entry.rtcp))
        };
        let (sock_b, rtcp_b) = {
            let entry = self
                .endpoints
                .get(&b)
                .ok_or(MediaError::UnknownEndpoint(b))?;
            (Arc::clone(&entry.rtp), Arc::clone(&entry.rtcp))
        };

        let id = self.fresh_bridge_id();
        let leg_a = Leg {
            socket: sock_a,
            peer: peer_a,
            rtcp: Some(RtcpLeg {
                socket: rtcp_a,
                // Peer RTCP port = peer RTP port + 1 (RFC 3550 §11).
                peer: peer_rtcp_from_rtp(peer_a),
            }),
            // SRTP is bound by the SDP negotiator path, which passes
            // `LegSrtp` in via a follow-on fabric method. Today the
            // default path stays plain-RTP passthrough.
            srtp: None,
        };
        let leg_b = Leg {
            socket: sock_b,
            peer: peer_b,
            rtcp: Some(RtcpLeg {
                socket: rtcp_b,
                peer: peer_rtcp_from_rtp(peer_b),
            }),
            srtp: None,
        };
        let cfg = BridgeConfig {
            metrics: self.metrics.clone(),
            ..BridgeConfig::default()
        };
        let bridge = Bridge::spawn_with(id, &leg_a, &leg_b, &cfg);
        self.bridges.insert(id, bridge);
        if let Some(m) = &self.metrics {
            m.bridges_active.inc();
        }
        Ok(id)
    }

    async fn release_bridge(&self, id: BridgeId) {
        if let Some((_, bridge)) = self.bridges.remove(&id) {
            bridge.shutdown().await;
            if let Some(m) = &self.metrics {
                m.bridges_active.dec();
            }
        }
    }

    async fn release_endpoint(&self, id: EndpointId) {
        // Dropping the `EndpointSockets` closes both UDP sockets unless
        // a forwarder task still holds a clone of `rtp`.
        self.endpoints.remove(&id);
    }

    async fn send_packet(
        &self,
        src: EndpointId,
        dest: SocketAddr,
        bytes: &[u8],
    ) -> Result<(), MediaError> {
        let sock = self
            .endpoints
            .get(&src)
            .ok_or(MediaError::UnknownEndpoint(src))?
            .rtp
            .clone();
        sock.send_to(bytes, dest).await.map_err(MediaError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test(flavor = "multi_thread")]
    async fn allocate_returns_bound_local_addr() {
        let fab = UdpMediaFabric::new();
        let ep = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        assert_eq!(ep.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(ep.local_addr().port(), 0);
        assert_eq!(ep.local_addr().port() % 2, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_forwards_between_allocated_endpoints() {
        let fab = UdpMediaFabric::new();
        let ep_a = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ep_b = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();

        // Stand-in for UA-A / UA-B.
        let ua_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_ua_a = ua_a.local_addr().unwrap();
        let addr_ua_b = ua_b.local_addr().unwrap();

        let bid = fab
            .bridge(ep_a.id(), addr_ua_a, ep_b.id(), addr_ua_b)
            .await
            .unwrap();

        // Minimal valid RTP header (V=2, PT=0 PCMU, SEQ=1, TS=0, SSRC=0xDEAD_BEEF)
        // + 4 bytes of payload. Must be parseable by the SSRC router.
        let rtp_a_to_b: &[u8] = &[
            0x80, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF, 1, 2, 3, 4,
        ];
        ua_a.send_to(rtp_a_to_b, ep_a.local_addr()).await.unwrap();
        let mut buf = [0u8; 256];
        let (n, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        // Payload preserved.
        assert_eq!(&buf[12..n], &[1, 2, 3, 4]);

        fab.release_bridge(bid).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_with_unknown_endpoint_errors() {
        let fab = UdpMediaFabric::new();
        let ep = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let bogus = EndpointId(9999);
        let err = fab
            .bridge(
                ep.id(),
                "127.0.0.1:1".parse().unwrap(),
                bogus,
                "127.0.0.1:2".parse().unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UnknownEndpoint(x) if x == bogus));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn release_bridge_is_idempotent() {
        let fab = UdpMediaFabric::new();
        let ep_a = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ep_b = fab.allocate(IpAddr::V4(Ipv4Addr::LOCALHOST)).await.unwrap();
        let ua_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ua_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let bid = fab
            .bridge(
                ep_a.id(),
                ua_a.local_addr().unwrap(),
                ep_b.id(),
                ua_b.local_addr().unwrap(),
            )
            .await
            .unwrap();
        fab.release_bridge(bid).await;
        // Second release must not panic or block.
        fab.release_bridge(bid).await;
    }
}
