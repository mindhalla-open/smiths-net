//! Transport abstraction for SIP signaling.
//!
//! The `Transport` trait sends and receives whole SIP messages, not
//! raw bytes on a stream. This is the MVP guardrail that keeps later
//! phases (TCP framing, TLS, QUIC / SIP-over-QUIC, WebTransport,
//! SOCKS-tunneled sockets) additive rather than intrusive.

use std::net::SocketAddr;

use bytes::Bytes;

pub mod framing;
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
}
