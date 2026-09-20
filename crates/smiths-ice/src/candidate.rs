//! Candidate gathering.
//!
//! "Host" candidates are the endpoint's **directly-bound** IP:port
//! pairs — no STUN / TURN involved; they cover LAN peers and any
//! environment where the peer is reachable without NAT traversal.
//! [`CandidateGatherer::gather_all`] adds server-reflexive
//! candidates (one Binding round trip per configured STUN server)
//! and a relay candidate (a TURN allocation) on top.
//!
//! Intended caller: the CLI's WebRTC signaling handler, when
//! `webrtc.ice.stun_servers` is set, gathers on the media socket the
//! fabric allocated and puts the result into the answer's
//! `a=candidate` lines; the `smiths-sdp` negotiator alone only emits
//! the host candidate.
//!
//! The output is a `Vec<smiths_sdp::IceCandidate>` — ready to slot
//! straight into the answer's `m=audio` block and into
//! [`crate::IceAgent::new`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use smiths_sdp::IceCandidate;
use thiserror::Error;
use tokio::net::UdpSocket;

use crate::turn::{LongTermCredential, TurnClient};

/// Errors raised by [`CandidateGatherer`].
#[derive(Debug, Error)]
pub enum CandidateError {
    /// A binding address couldn't be inspected.
    #[error("address error: {0}")]
    Io(String),
}

/// Result of [`CandidateGatherer::gather_all`].
pub struct Gathered {
    /// Host, then srflx, then relay candidates.
    pub candidates: Vec<IceCandidate>,
    /// The TURN allocation backing the relay candidate, when one was
    /// obtained. Keep it alive (and refresh it) for as long as the
    /// relay candidate may be used.
    pub turn: Option<TurnClient>,
}

/// Fluent builder for candidate generation. Callers register the
/// bound addresses and then call [`Self::gather`] (host only) or
/// [`Self::gather_all`].
#[derive(Clone, Debug, Default)]
pub struct CandidateGatherer {
    /// `(address, port)` tuples to emit candidates for. The first
    /// entry gets component 1 (RTP); `component` parameter on
    /// [`Self::gather`] is the RTP component id.
    binds: Vec<SocketAddr>,
}

impl CandidateGatherer {
    /// Fresh gatherer with no binds.
    #[must_use]
    pub fn new() -> Self {
        Self { binds: Vec::new() }
    }

    /// Append a `(ip, port)` to gather a host candidate for.
    #[must_use]
    pub fn with_bind(mut self, bind: SocketAddr) -> Self {
        self.binds.push(bind);
        self
    }

    /// Gather one host candidate per registered bind. `component` is
    /// the RTP component id — typically 1. Foundation strings are
    /// derived from (addr family, base address) per RFC 8445 §5.1.1.3
    /// — two candidates with the same foundation share an equivalence
    /// class.
    pub fn gather(&self, component: u8) -> Result<Vec<IceCandidate>, CandidateError> {
        let mut out = Vec::with_capacity(self.binds.len());
        for (idx, bind) in self.binds.iter().enumerate() {
            out.push(make_host_candidate(*bind, component, idx));
        }
        Ok(out)
    }

    /// Gather host, srflx and relay candidates.
    ///
    /// - `socket`: the bound media socket srflx / relay candidates
    ///   are based on (its own address should be among the binds).
    /// - `stun_servers`: queried concurrently with a 1 s budget each;
    ///   every distinct observed address becomes an `srflx`
    ///   candidate.
    /// - `turn_server`: optional TURN server + credentials; a
    ///   successful allocation becomes a `relay` candidate and the
    ///   [`TurnClient`] is returned so the caller can refresh it and
    ///   install permissions. Allocation failure is logged and
    ///   skipped — the host candidates are still returned.
    pub async fn gather_all(
        &self,
        socket: &Arc<UdpSocket>,
        stun_servers: &[SocketAddr],
        turn_server: Option<(SocketAddr, LongTermCredential)>,
        component: u8,
    ) -> Result<Gathered, CandidateError> {
        let mut candidates = self.gather(component)?;
        let local_addr = socket
            .local_addr()
            .map_err(|e| CandidateError::Io(e.to_string()))?;

        if !stun_servers.is_empty() {
            let srflx_addrs =
                crate::stun::gather_srflx_candidates(socket, stun_servers, Duration::from_secs(1))
                    .await;
            for (idx, addr) in srflx_addrs.into_iter().enumerate() {
                candidates.push(make_srflx_candidate(addr, local_addr, component, idx));
            }
        }

        let mut turn = None;
        if let Some((server, cred)) = turn_server {
            match TurnClient::allocate(Arc::clone(socket), server, cred, Duration::from_secs(2))
                .await
            {
                Ok(client) => {
                    candidates.push(make_relay_candidate(
                        client.relay_addr(),
                        local_addr,
                        component,
                        0,
                    ));
                    turn = Some(client);
                }
                Err(e) => tracing::debug!(?e, "TURN allocation failed during gathering"),
            }
        }

        Ok(Gathered { candidates, turn })
    }
}

/// Convenience for the common case — one bind, one candidate.
#[must_use]
pub fn gather_host_candidates(binds: &[SocketAddr], component: u8) -> Vec<IceCandidate> {
    binds
        .iter()
        .enumerate()
        .map(|(idx, b)| make_host_candidate(*b, component, idx))
        .collect()
}

fn make_host_candidate(bind: SocketAddr, component: u8, idx: usize) -> IceCandidate {
    // Foundation: per RFC 8445 §5.1.1.3, equal for candidates with the
    // same (type, base address, transport protocol, STUN/TURN server).
    let foundation = format!("host{idx}");
    let priority = candidate_priority(126, &bind.ip(), component);
    IceCandidate {
        foundation,
        component,
        transport: "UDP".to_owned(),
        priority,
        address: bind.ip(),
        port: bind.port(),
        candidate_type: "host".to_owned(),
        related_address: None,
        related_port: None,
        raw_params: Vec::new(),
    }
}

fn make_srflx_candidate(
    addr: SocketAddr,
    base: SocketAddr,
    component: u8,
    idx: usize,
) -> IceCandidate {
    let foundation = format!("srflx{idx}");
    let priority = candidate_priority(100, &base.ip(), component);
    IceCandidate {
        foundation,
        component,
        transport: "UDP".to_owned(),
        priority,
        address: addr.ip(),
        port: addr.port(),
        candidate_type: "srflx".to_owned(),
        related_address: Some(base.ip()),
        related_port: Some(base.port()),
        raw_params: Vec::new(),
    }
}

fn make_relay_candidate(
    addr: SocketAddr,
    base: SocketAddr,
    component: u8,
    idx: usize,
) -> IceCandidate {
    let foundation = format!("relay{idx}");
    let priority = candidate_priority(0, &base.ip(), component);
    IceCandidate {
        foundation,
        component,
        transport: "UDP".to_owned(),
        priority,
        address: addr.ip(),
        port: addr.port(),
        candidate_type: "relay".to_owned(),
        related_address: Some(base.ip()),
        related_port: Some(base.port()),
        raw_params: Vec::new(),
    }
}

/// RFC 8445 §5.1.2.1 priority formula:
///
/// ```text
///   priority = (2^24) * type-pref + (2^8) * local-pref + (256 - component)
/// ```
fn candidate_priority(type_pref: u32, ip: &IpAddr, component: u8) -> u32 {
    let local_pref: u32 = if matches!(ip, IpAddr::V6(_)) {
        65_535
    } else {
        65_534
    };
    (type_pref << 24) + (local_pref << 8) + u32::from(256 - u16::from(component))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn host_candidate_priority(ip: &IpAddr, component: u8) -> u32 {
        candidate_priority(126, ip, component)
    }

    #[test]
    fn gather_single_ipv4_bind_yields_host_candidate() {
        let bind: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 5)), 40_000);
        let cands = CandidateGatherer::new().with_bind(bind).gather(1).unwrap();
        assert_eq!(cands.len(), 1);
        let c = &cands[0];
        assert_eq!(c.component, 1);
        assert_eq!(c.address, bind.ip());
        assert_eq!(c.port, 40_000);
        assert_eq!(c.candidate_type, "host");
        assert_eq!(c.transport, "UDP");
        assert!(c.related_address.is_none());
    }

    #[test]
    fn gather_preserves_bind_order_and_foundations_differ() {
        let b1: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 100);
        let b2: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 200);
        let cands = CandidateGatherer::new()
            .with_bind(b1)
            .with_bind(b2)
            .gather(1)
            .unwrap();
        assert_eq!(cands.len(), 2);
        assert_ne!(cands[0].foundation, cands[1].foundation);
        assert_eq!(cands[0].address, b1.ip());
        assert_eq!(cands[1].address, b2.ip());
    }

    #[test]
    fn ipv6_candidate_has_higher_local_pref_than_ipv4() {
        let v4_prio = host_candidate_priority(&IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        let v6_prio = host_candidate_priority(&IpAddr::V6(Ipv6Addr::LOCALHOST), 1);
        assert!(
            v6_prio > v4_prio,
            "IPv6 host candidate should outrank IPv4: v4={v4_prio}, v6={v6_prio}"
        );
    }

    #[test]
    fn priority_decreases_as_component_increases() {
        // Higher component id = lower priority (RTCP < RTP).
        let rtp_pri = host_candidate_priority(&IpAddr::V4(Ipv4Addr::LOCALHOST), 1);
        let control_pri = host_candidate_priority(&IpAddr::V4(Ipv4Addr::LOCALHOST), 2);
        assert!(rtp_pri > control_pri);
    }

    #[test]
    fn srflx_and_relay_rank_below_host() {
        let base: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let host = make_host_candidate(base, 1, 0);
        let srflx = make_srflx_candidate("203.0.113.9:6000".parse().unwrap(), base, 1, 0);
        let relay = make_relay_candidate("198.51.100.2:7000".parse().unwrap(), base, 1, 0);
        assert!(host.priority > srflx.priority && srflx.priority > relay.priority);
        assert_eq!(srflx.related_address, Some(base.ip()));
        assert_eq!(relay.related_port, Some(base.port()));
    }

    #[test]
    fn free_function_matches_builder() {
        let bind: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5_004);
        let a = gather_host_candidates(&[bind], 1);
        let b = CandidateGatherer::new().with_bind(bind).gather(1).unwrap();
        assert_eq!(a, b);
    }
}
