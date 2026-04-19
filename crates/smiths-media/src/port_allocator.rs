//! Even-RTP / odd-RTCP port-pair allocation (RFC 3550 §11).
//!
//! A media endpoint wants two adjacent UDP ports: even for RTP,
//! even+1 for RTCP. The OS doesn't guarantee pairing on ephemeral
//! bind, so we retry: bind one socket, reject odd ports, try to bind
//! `port+1`, and give up after `max_attempts`.
//!
//! This is deliberately simple — no port ranges, no reserved list.
//! Phase 6 ops work can slot in a configurable range without changing
//! callers.

#![allow(clippy::similar_names)] // `rtp_*` / `rtcp_*` naming is deliberate.

use std::net::{IpAddr, SocketAddr};

use smiths_core::media::MediaError;
use tokio::net::UdpSocket;
use tracing::{debug, instrument};

/// A freshly-bound RTP/RTCP socket pair.
pub struct PortPair {
    /// Even-port UDP socket for RTP traffic.
    pub rtp: UdpSocket,
    /// RTP-port-plus-one UDP socket for RTCP traffic.
    pub rtcp: UdpSocket,
    pub rtp_addr: SocketAddr,
    pub rtcp_addr: SocketAddr,
}

/// Default retry ceiling — plenty for the ephemeral port range even
/// on a busy host.
pub const DEFAULT_MAX_ATTEMPTS: usize = 64;

/// Allocate one RTP/RTCP pair on `bind_ip`, retrying up to
/// `max_attempts` times.
///
/// Each iteration binds a random-port UDP socket. Odd ports are
/// discarded; even ports prompt a second bind at `port + 1`. If the
/// neighbour is already taken, both sockets are closed and we retry.
#[instrument(skip_all, fields(%bind_ip, max_attempts))]
pub async fn allocate_rtp_rtcp_pair(
    bind_ip: IpAddr,
    max_attempts: usize,
) -> Result<PortPair, MediaError> {
    for attempt in 0..max_attempts {
        let rtp = UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await?;
        let rtp_addr = rtp.local_addr()?;
        if rtp_addr.port() % 2 != 0 {
            // Odd → close and retry.
            drop(rtp);
            continue;
        }
        let rtcp_target = SocketAddr::new(bind_ip, rtp_addr.port() + 1);
        match UdpSocket::bind(rtcp_target).await {
            Ok(rtcp) => {
                let rtcp_addr = rtcp.local_addr()?;
                debug!(%rtp_addr, %rtcp_addr, attempt, "RTP/RTCP pair bound");
                return Ok(PortPair {
                    rtp,
                    rtcp,
                    rtp_addr,
                    rtcp_addr,
                });
            }
            Err(_) => {
                // Neighbour port taken — try again with a fresh bind.
                drop(rtp);
            }
        }
    }
    Err(MediaError::PortExhausted(format!(
        "no even/odd RTP/RTCP pair within {max_attempts} attempts on {bind_ip}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test(flavor = "multi_thread")]
    async fn allocates_even_odd_pair() {
        let pair = allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_MAX_ATTEMPTS)
            .await
            .unwrap();
        assert_eq!(pair.rtp_addr.port() % 2, 0, "RTP port must be even");
        assert_eq!(
            pair.rtcp_addr.port(),
            pair.rtp_addr.port() + 1,
            "RTCP port must be RTP port + 1"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn distinct_pairs_do_not_collide() {
        let a = allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_MAX_ATTEMPTS)
            .await
            .unwrap();
        let b = allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_MAX_ATTEMPTS)
            .await
            .unwrap();
        assert_ne!(a.rtp_addr.port(), b.rtp_addr.port());
        assert_ne!(a.rtcp_addr.port(), b.rtcp_addr.port());
    }
}
