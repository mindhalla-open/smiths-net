//! WebRTC privacy enforcement helpers (slice 5.11-runtime).
//!
//! Three operator-selectable modes (`open` / `relay_only` /
//! `strict`) layer additively:
//!
//! - `open` — no hardening; offers go through unchanged.
//! - `relay_only` — reject offers carrying `host` / `srflx`
//!   candidates; strip `host` candidates from engine answers.
//!   Operator also sets `iceTransportPolicy = "relay"` on the
//!   client side.
//! - `strict` — `relay_only` + keyed-hash redaction of every
//!   peer IP in audit / CDR / tracing via `blake3(ip, key)`.
//!
//! This module implements the mode-agnostic helpers: one
//! function that filters candidates on an [`SessionDescription`],
//! one function that redacts IPs via a keyed blake3-like hash.
//! The actual enforcement (which mode the engine is running in)
//! is an operator choice expressed in `[webrtc.privacy]` —
//! callers dispatch based on that.

use std::net::IpAddr;

use crate::{MediaDescription, SessionDescription};

/// ICE candidate types the engine treats as "directly-reachable
/// local addresses". Stripping / rejecting them is the point of
/// `relay_only` mode.
const DIRECTLY_REACHABLE_TYPES: &[&str] = &["host", "srflx"];

/// Outcome of [`reject_direct_candidates`] against an inbound
/// offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferPrivacyVerdict {
    /// No directly-reachable candidate types present — offer is
    /// acceptable under `relay_only` / `strict`.
    Clean,
    /// Offer contains at least one `host` / `srflx` candidate.
    /// Operator in `relay_only` / `strict` mode rejects with
    /// `488 Not Acceptable Here` + `Warning: 399
    /// "host/srflx candidates rejected by privacy policy"`.
    Rejected,
}

/// Inspect every media block in `sdp`; return `Rejected` if any
/// `a=candidate:` line declares a `host` or `srflx` type.
#[must_use]
pub fn reject_direct_candidates(sdp: &SessionDescription) -> OfferPrivacyVerdict {
    for m in &sdp.media {
        for c in &m.candidates {
            let t = c.candidate_type.as_str();
            if DIRECTLY_REACHABLE_TYPES.contains(&t) {
                return OfferPrivacyVerdict::Rejected;
            }
        }
    }
    OfferPrivacyVerdict::Clean
}

/// Strip `host` candidates in-place. Suitable for outbound
/// answers the engine emits in `relay_only` mode: the engine
/// doesn't advertise its LAN addresses to the peer.
///
/// Keeps `srflx` / `relay` / `prflx` candidates intact — those
/// carry the engine's NAT-traversal story.
pub fn strip_host_candidates(sdp: &mut SessionDescription) -> usize {
    let mut removed = 0;
    for m in &mut sdp.media {
        let before = m.candidates.len();
        m.candidates.retain(|c| c.candidate_type != "host");
        removed += before - m.candidates.len();
    }
    removed
}

/// Pure-function variant of [`strip_host_candidates`] for use
/// on a single media block (some callers manipulate one block
/// at a time).
pub fn strip_host_candidates_on(m: &mut MediaDescription) -> usize {
    let before = m.candidates.len();
    m.candidates.retain(|c| c.candidate_type != "host");
    before - m.candidates.len()
}

/// Keyed-hash IP redaction (slice 5.11 `strict` mode).
///
/// `blake3(key || ip_string).first(8)` rendered as 16-hex
/// characters. Uses a simple keyed SHA-256 instead of pulling
/// the `blake3` crate — the security goal is "correlation
/// within a key lifetime", not cryptographic unforgeability,
/// and SHA-256 with a fresh per-rotation key is plenty.
/// Operators who want `blake3` specifically can swap the impl
/// later; the API surface is stable.
#[must_use]
pub fn redact_ip(ip: IpAddr, key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key);
    h.update(ip.to_string().as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(16);
    for b in &digest[..8] {
        let hi = (b >> 4) & 0xF;
        let lo = b & 0xF;
        out.push(hex_nibble(hi));
        out.push(hex_nibble(lo));
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + n - 10) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConnectionInfo, Direction, IceCandidate, MediaKind, Origin, SessionDescription};
    use std::net::Ipv4Addr;

    fn make_sdp(candidates: Vec<(&str, &str)>) -> SessionDescription {
        SessionDescription {
            origin: Origin {
                username: "-".into(),
                session_id: 1,
                session_version: 1,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            },
            session_name: "-".into(),
            connection: Some(ConnectionInfo {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            }),
            media: vec![MediaDescription {
                kind: MediaKind::Audio,
                port: 5004,
                protocol: "RTP/AVP".into(),
                formats: vec![0],
                rtpmap: vec![],
                crypto: vec![],
                direction: Direction::SendRecv,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: vec![],
                candidates: candidates
                    .into_iter()
                    .enumerate()
                    .map(|(i, (addr, kind))| IceCandidate {
                        foundation: format!("{i}"),
                        component: 1,
                        transport: "UDP".into(),
                        priority: 1,
                        address: addr.parse().unwrap(),
                        port: 10_000,
                        candidate_type: kind.to_owned(),
                        related_address: None,
                        related_port: None,
                        raw_params: vec![],
                    })
                    .collect(),
                end_of_candidates: false,
            }],
        }
    }

    #[test]
    fn clean_offer_passes() {
        let sdp = make_sdp(vec![("203.0.113.7", "relay")]);
        assert_eq!(reject_direct_candidates(&sdp), OfferPrivacyVerdict::Clean);
    }

    #[test]
    fn host_candidate_rejects() {
        let sdp = make_sdp(vec![("192.168.1.10", "host")]);
        assert_eq!(
            reject_direct_candidates(&sdp),
            OfferPrivacyVerdict::Rejected
        );
    }

    #[test]
    fn srflx_candidate_rejects() {
        let sdp = make_sdp(vec![("198.51.100.1", "srflx")]);
        assert_eq!(
            reject_direct_candidates(&sdp),
            OfferPrivacyVerdict::Rejected
        );
    }

    #[test]
    fn mixed_candidates_rejects_on_first_direct() {
        let sdp = make_sdp(vec![("203.0.113.7", "relay"), ("192.168.1.10", "host")]);
        assert_eq!(
            reject_direct_candidates(&sdp),
            OfferPrivacyVerdict::Rejected
        );
    }

    #[test]
    fn strip_host_removes_only_host_type() {
        let mut sdp = make_sdp(vec![
            ("192.168.1.10", "host"),
            ("203.0.113.7", "relay"),
            ("198.51.100.1", "srflx"),
        ]);
        let removed = strip_host_candidates(&mut sdp);
        assert_eq!(removed, 1);
        assert_eq!(sdp.media[0].candidates.len(), 2);
        assert!(
            sdp.media[0]
                .candidates
                .iter()
                .all(|c| c.candidate_type != "host")
        );
    }

    #[test]
    fn redact_ip_is_stable_for_same_key_and_ip() {
        let key = b"rotate-me-weekly";
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        let a = redact_ip(ip, key);
        let b = redact_ip(ip, key);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn redact_ip_differs_across_keys() {
        let ip: IpAddr = "198.51.100.7".parse().unwrap();
        let a = redact_ip(ip, b"key-1");
        let b = redact_ip(ip, b"key-2");
        assert_ne!(a, b);
    }

    #[test]
    fn redact_ip_differs_across_ips() {
        let key = b"k";
        let a = redact_ip("198.51.100.7".parse().unwrap(), key);
        let b = redact_ip("198.51.100.8".parse().unwrap(), key);
        assert_ne!(a, b);
    }
}
