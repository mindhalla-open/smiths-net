//! [`MediaFabric`] implementation on plain UDP.
//!
//! Owns all media sockets. Hands out opaque [`EndpointId`] handles to
//! the signaling layer, which never touches a socket directly. On
//! [`MediaFabric::bridge`], spins up the byte-transparent forwarder
//! from [`crate::bridge`] and retains the [`Bridge`] so that a later
//! `release_bridge` call can await its shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use smiths_core::media::{BridgeId, Endpoint, EndpointId, MediaError, MediaFabric};
use tokio::net::UdpSocket;
use tracing::{debug, instrument};

use crate::bridge::{Bridge, Leg};

/// Default UDP-backed [`MediaFabric`].
#[derive(Default)]
pub struct UdpMediaFabric {
    next_endpoint: AtomicU64,
    next_bridge: AtomicU64,
    endpoints: DashMap<EndpointId, Arc<UdpSocket>>,
    bridges: DashMap<BridgeId, Bridge>,
}

impl UdpMediaFabric {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
    async fn allocate(&self, bind_ip: IpAddr) -> Result<Endpoint, MediaError> {
        let socket = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await?;
        let local_addr = socket.local_addr()?;
        let id = self.fresh_endpoint_id();
        self.endpoints.insert(id, Arc::new(socket));
        debug!(?id, %local_addr, "media endpoint allocated");
        Ok(Endpoint { id, local_addr })
    }

    #[instrument(skip(self), fields(?a, ?b, %peer_a, %peer_b))]
    async fn bridge(
        &self,
        a: EndpointId,
        peer_a: SocketAddr,
        b: EndpointId,
        peer_b: SocketAddr,
    ) -> Result<BridgeId, MediaError> {
        let sock_a = self
            .endpoints
            .get(&a)
            .ok_or(MediaError::UnknownEndpoint(a))?
            .clone();
        let sock_b = self
            .endpoints
            .get(&b)
            .ok_or(MediaError::UnknownEndpoint(b))?
            .clone();

        let leg_a = Leg {
            socket: sock_a,
            peer: peer_a,
        };
        let leg_b = Leg {
            socket: sock_b,
            peer: peer_b,
        };
        let bridge = Bridge::spawn(&leg_a, &leg_b);
        let id = self.fresh_bridge_id();
        self.bridges.insert(id, bridge);
        Ok(id)
    }

    async fn release_bridge(&self, id: BridgeId) {
        if let Some((_, bridge)) = self.bridges.remove(&id) {
            bridge.shutdown().await;
        }
    }

    async fn release_endpoint(&self, id: EndpointId) {
        // Dropping the Arc<UdpSocket> closes the socket unless a
        // forwarder task still holds a clone.
        self.endpoints.remove(&id);
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
        assert_eq!(ep.local_addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(ep.local_addr.port(), 0);
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
            .bridge(ep_a.id, addr_ua_a, ep_b.id, addr_ua_b)
            .await
            .unwrap();

        ua_a.send_to(b"ping-a", ep_a.local_addr).await.unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"ping-a");

        fab.release_bridge(bid).await;
    }

    #[tokio::test]
    async fn bridge_with_unknown_endpoint_errors() {
        let fab = UdpMediaFabric::new();
        let err = fab
            .bridge(
                EndpointId(999),
                "127.0.0.1:1".parse().unwrap(),
                EndpointId(998),
                "127.0.0.1:2".parse().unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MediaError::UnknownEndpoint(_)));
    }

    #[tokio::test]
    async fn release_bridge_is_idempotent() {
        let fab = UdpMediaFabric::new();
        // Releasing a non-existent bridge is a no-op.
        fab.release_bridge(BridgeId(42)).await;
    }
}
