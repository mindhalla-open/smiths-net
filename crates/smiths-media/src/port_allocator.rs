//! Even-RTP / odd-RTCP port-pair allocation (RFC 3550 §11).
//!
//! A media endpoint wants two adjacent UDP ports: even for RTP,
//! even+1 for RTCP. The OS doesn't guarantee pairing on ephemeral
//! bind, so we retry: bind one socket, reject odd ports, try to bind
//! `port+1`, and give up after `max_attempts`.
//!
//! Two modes: ephemeral (OS-assigned random port) or a configured
//! `[media.rtp_ports]` window the caller pins so the media plane fits
//! one firewall rule.

#![allow(clippy::similar_names)] // `rtp_*` / `rtcp_*` naming is deliberate.

use std::net::{IpAddr, SocketAddr};

use smiths_core::media::MediaError;
use tokio::net::UdpSocket;
use tracing::{debug, instrument};

/// A freshly-bound RTP/RTCP socket pair.
#[derive(Debug)]
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

/// Allocate one RTP/RTCP pair on `bind_ip`.
///
/// With `range = None`, binds a random ephemeral port (retrying up to
/// `max_attempts` times until it lands on an even one whose `+1`
/// neighbour is free). With `range = Some((min, max))`, scans even
/// ports across the inclusive `[min, max]` window so operators can pin
/// media to a firewall-friendly range; `max_attempts` is then ignored
/// (the window bounds the search). RTP takes the even port, RTCP
/// `port + 1`.
#[instrument(skip_all, fields(%bind_ip, ?range, max_attempts))]
pub async fn allocate_rtp_rtcp_pair(
    bind_ip: IpAddr,
    range: Option<(u16, u16)>,
    max_attempts: usize,
) -> Result<PortPair, MediaError> {
    match range {
        None => allocate_ephemeral(bind_ip, max_attempts).await,
        Some((min, max)) => allocate_in_range(bind_ip, min, max).await,
    }
}

/// Random ephemeral allocation (the pre-range behaviour).
async fn allocate_ephemeral(bind_ip: IpAddr, max_attempts: usize) -> Result<PortPair, MediaError> {
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
                debug!(%rtp_addr, %rtcp_addr, attempt, "RTP/RTCP pair bound (ephemeral)");
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

/// Scan even ports across `[min, max]` (inclusive), binding the first
/// even port whose `+1` RTCP neighbour is also free.
async fn allocate_in_range(bind_ip: IpAddr, min: u16, max: u16) -> Result<PortPair, MediaError> {
    // RTP must be even; round the floor up. The last usable even port
    // is `max - 1` so the odd RTCP neighbour still fits in the window.
    let mut port = if min.is_multiple_of(2) {
        min
    } else {
        min.saturating_add(1)
    };
    while port < max {
        if let Ok(rtp) = UdpSocket::bind(SocketAddr::new(bind_ip, port)).await {
            let rtcp_target = SocketAddr::new(bind_ip, port + 1);
            if let Ok(rtcp) = UdpSocket::bind(rtcp_target).await {
                let rtp_addr = rtp.local_addr()?;
                let rtcp_addr = rtcp.local_addr()?;
                debug!(%rtp_addr, %rtcp_addr, "RTP/RTCP pair bound (range)");
                return Ok(PortPair {
                    rtp,
                    rtcp,
                    rtp_addr,
                    rtcp_addr,
                });
            }
            // RTP bound but RTCP neighbour taken — release and advance.
            drop(rtp);
        }
        port = match port.checked_add(2) {
            Some(p) => p,
            None => break,
        };
    }
    Err(MediaError::PortExhausted(format!(
        "no free even/odd RTP/RTCP pair in range {min}..={max} on {bind_ip}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test(flavor = "multi_thread")]
    async fn allocates_even_odd_pair() {
        let pair =
            allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), None, DEFAULT_MAX_ATTEMPTS)
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
        let a = allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), None, DEFAULT_MAX_ATTEMPTS)
            .await
            .unwrap();
        let b = allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), None, DEFAULT_MAX_ATTEMPTS)
            .await
            .unwrap();
        assert_ne!(a.rtp_addr.port(), b.rtp_addr.port());
        assert_ne!(a.rtcp_addr.port(), b.rtcp_addr.port());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn range_allocation_stays_within_window_and_is_even() {
        let (min, max) = (40_000u16, 40_010u16);
        let pair = allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), Some((min, max)), 0)
            .await
            .unwrap();
        let rtp = pair.rtp_addr.port();
        assert_eq!(rtp % 2, 0, "RTP port must be even");
        assert!((min..max).contains(&rtp), "RTP {rtp} outside [{min},{max})");
        assert_eq!(pair.rtcp_addr.port(), rtp + 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn range_too_small_is_port_exhausted() {
        // A 1-port window can't hold an even RTP + odd RTCP pair.
        let err =
            allocate_rtp_rtcp_pair(IpAddr::V4(Ipv4Addr::LOCALHOST), Some((40_020, 40_020)), 0)
                .await
                .unwrap_err();
        assert!(matches!(err, MediaError::PortExhausted(_)), "got {err:?}");
    }
}
