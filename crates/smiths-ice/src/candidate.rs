//! Host-candidate gathering for the ICE MVP.
//!
//! "Host" candidates are the endpoint's **directly-bound** IP:port
//! pairs — no STUN / TURN involved. For a typical deployment this
//! covers LAN peers and any environment where the peer is reachable
//! without NAT traversal. Server-reflexive and relay candidates land
//! in later slices once `STUN` and `TURN` servers are wired in.
//!
//! The output is a `Vec<smiths_sdp::IceCandidate>` — one per bound
//! address — ready to slot straight into the answer's `m=audio`
//! block.

use std::net::{IpAddr, SocketAddr};

use smiths_sdp::IceCandidate;
use thiserror::Error;

/// Errors raised by [`CandidateGatherer`].
#[derive(Debug, Error)]
pub enum CandidateError {
    /// A binding address couldn't be inspected.
    #[error("address error: {0}")]
    Io(String),
}

/// Fluent builder for host-candidate generation. Wraps nothing
/// clever — it exists so callers can extend the bind list before
/// calling [`Self::gather`], which is an entry point other slices
/// can easily test.
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
    // For host-only we collapse it to index — each bind gets its own
    // foundation string.
    let foundation = format!("host{idx}");
    let priority = host_candidate_priority(&bind.ip(), component);
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

/// RFC 8445 §5.1.2.1 priority formula, restricted to host candidates:
///
/// ```text
///   priority = (2^24) * type-pref + (2^8) * local-pref + (256 - component)
/// ```
///
/// With `type-pref = 126` for host, `local-pref = 65535` for the single
/// interface we picked, and `component` typically 1 (RTP). Returns a
/// plain `u32` — the ICE tie-breaker only needs the ordering, not the
/// exact value.
fn host_candidate_priority(ip: &IpAddr, component: u8) -> u32 {
    // Slight nudge for IPv6 to match browsers' default preference
    // when dual-stacking; irrelevant on IPv4-only deployments.
    let type_pref: u32 = 126;
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
    fn free_function_matches_builder() {
        let bind: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5_004);
        let a = gather_host_candidates(&[bind], 1);
        let b = CandidateGatherer::new().with_bind(bind).gather(1).unwrap();
        assert_eq!(a, b);
    }
}
