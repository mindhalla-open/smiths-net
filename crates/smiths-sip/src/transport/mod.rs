//! Transport abstraction for SIP signaling.
//!
//! The `Transport` trait sends and receives whole SIP messages, not
//! raw bytes on a stream. This is the MVP guardrail that keeps later
//! phases (QUIC / SIP-over-QUIC, WebTransport, SOCKS-tunneled
//! sockets) additive rather than intrusive.

use std::net::{IpAddr, SocketAddr};

use bytes::Bytes;

pub mod framing;
pub mod proxy;
mod stream;
pub mod tcp;
pub mod tls;
pub mod udp;

/// A SIP message received from the network with its origin.
#[derive(Debug, Clone)]
pub struct Datagram {
    /// Raw SIP message bytes (one complete request or response).
    pub bytes: Bytes,
    /// Source peer address.
    pub peer: SocketAddr,
}

/// Which wire protocol a [`Transport`] speaks. Drives two RFC 3261
/// behaviours that differ between datagram and stream transports:
/// the `Via` transport token (§18.1.1) and whether the transaction
/// FSMs run retransmission timers (§17: reliable transports skip
/// timers A/E/G and use zero-length D/I/J/K waits).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransportKind {
    /// SIP over UDP (§18.1).
    Udp,
    /// SIP over TCP (§18.2).
    Tcp,
    /// SIP over TLS (RFC 5630 / §26.2).
    Tls,
}

impl TransportKind {
    /// `true` for stream transports that guarantee delivery, so the
    /// transaction layer must not retransmit.
    #[must_use]
    pub const fn is_reliable(self) -> bool {
        match self {
            Self::Udp => false,
            Self::Tcp | Self::Tls => true,
        }
    }

    /// The `SIP/2.0/<token>` protocol token for a `Via` header.
    #[must_use]
    pub const fn via_token(self) -> &'static str {
        match self {
            Self::Udp => "UDP",
            Self::Tcp => "TCP",
            Self::Tls => "TLS",
        }
    }
}

/// Common interface implemented by every SIP transport.
///
/// Implementations own the socket and are cheap to `Arc::clone`. They
/// must be `Send + Sync + 'static` because the UAS and outbound paths
/// both hold references across await points.
pub trait Transport: Send + Sync + 'static {
    /// Send one complete SIP message to `peer`.
    fn send(
        &self,
        bytes: Bytes,
        peer: SocketAddr,
    ) -> impl std::future::Future<Output = std::io::Result<()>> + Send;

    /// Local bind address (post-bind, so `:0` resolves to the chosen port).
    fn local_addr(&self) -> std::io::Result<SocketAddr>;

    /// Wire protocol this transport speaks. Callers derive the `Via`
    /// token and the transaction-timer profile from it.
    fn kind(&self) -> TransportKind;
}

/// Pick the local IP to publish in outbound SDP (and `Via` /
/// `Contact`) for a given peer.
///
/// - If `bind_ip` is a concrete address, trust it.
/// - Otherwise (wildcard `0.0.0.0` / `::`) use the kernel's routing
///   table: bind an ephemeral UDP socket of the peer's address family,
///   `connect(peer)` to pick a route (no packets sent), and read back
///   the local address the kernel chose. Fall back to loopback if
///   anything fails.
pub async fn resolve_local_ip_for(bind_ip: IpAddr, peer: SocketAddr) -> IpAddr {
    if !bind_ip.is_unspecified() {
        return bind_ip;
    }
    let unspec: SocketAddr = match peer {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    if let Ok(sock) = tokio::net::UdpSocket::bind(unspec).await
        && sock.connect(peer).await.is_ok()
        && let Ok(addr) = sock.local_addr()
        && !addr.ip().is_unspecified()
    {
        return addr.ip();
    }
    match peer {
        SocketAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_reliability_and_via_tokens() {
        assert!(!TransportKind::Udp.is_reliable());
        assert!(TransportKind::Tcp.is_reliable());
        assert!(TransportKind::Tls.is_reliable());
        assert_eq!(TransportKind::Udp.via_token(), "UDP");
        assert_eq!(TransportKind::Tcp.via_token(), "TCP");
        assert_eq!(TransportKind::Tls.via_token(), "TLS");
    }

    #[tokio::test]
    async fn concrete_bind_ip_is_returned_verbatim() {
        let ip: IpAddr = "192.0.2.10".parse().unwrap();
        let peer: SocketAddr = "198.51.100.1:5060".parse().unwrap();
        assert_eq!(resolve_local_ip_for(ip, peer).await, ip);
    }

    #[tokio::test]
    async fn wildcard_bind_resolves_loopback_peer_to_loopback() {
        let peer: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let got = resolve_local_ip_for(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), peer).await;
        assert!(got.is_loopback(), "got {got}");
    }
}
