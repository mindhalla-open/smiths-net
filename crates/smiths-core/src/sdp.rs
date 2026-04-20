//! SDP offer/answer trait seam.
//!
//! `smiths-sip` only sees the trait + [`NegotiationOutcome`] enum; it
//! does not parse SDP or depend on `smiths-sdp`. Implementations
//! (today: `smiths-sdp::Negotiator`, tomorrow: transcoding-aware
//! variants) return a ready-to-embed answer body plus the peer RTP
//! endpoint derived from the offer — SIP never introspects the parse
//! tree.

use std::net::{IpAddr, SocketAddr};

use crate::SrtpSuite;

/// SRTP keying material negotiated via SDES (RFC 4568).
///
/// Emitted by [`NegotiationOutcome::Accepted`] when the offer used
/// `RTP/SAVP` + a supported `a=crypto:` line. The responder (UAS)
/// threads this into the media fabric's bridge spawn so each leg runs
/// with the correct decrypt/encrypt pair.
///
/// Key-material handedness follows SDES semantics:
/// - `peer_tx_key` is what **the peer** encrypts with — the engine uses
///   it to **decrypt** ingress on the leg facing this peer.
/// - `local_tx_key` is the key the **engine** put in its `a=crypto:`
///   answer — the engine uses it to **encrypt** egress toward this
///   peer; the peer decrypts with it on receipt.
///
/// `Debug` is hand-written to redact the key bytes; SDES key material
/// is a long-lived secret per call and must not leak into logs.
#[derive(Clone, PartialEq, Eq)]
pub struct SrtpKeys {
    /// Cipher suite both sides agreed on.
    pub suite: SrtpSuite,
    /// Peer-chosen key material (from the offer's `a=crypto:`). Length
    /// equals `suite.key_material_len()`.
    pub peer_tx_key: Vec<u8>,
    /// Engine-chosen key material (emitted in the answer's `a=crypto:`).
    /// Length equals `suite.key_material_len()`.
    pub local_tx_key: Vec<u8>,
}

impl std::fmt::Debug for SrtpKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SrtpKeys")
            .field("suite", &self.suite)
            .field("peer_tx_key_len", &self.peer_tx_key.len())
            .field("local_tx_key_len", &self.local_tx_key.len())
            .finish()
    }
}

/// Outcome of running offer/answer against an inbound SDP offer.
#[derive(Clone, Debug)]
pub enum NegotiationOutcome {
    /// Offer accepted.
    Accepted {
        /// Serialized SDP answer body to embed in the 200 OK.
        answer_body: String,
        /// Peer's first audio RTP endpoint (`m=audio port` + `c=`),
        /// used by the bridge to send forward traffic. `None` if the
        /// offer declared port 0 or omitted a connection line.
        remote_media: Option<SocketAddr>,
        /// SRTP keying material when the offer asked for `RTP/SAVP`
        /// with a supported `a=crypto:` suite. `None` for plain
        /// `RTP/AVP` passthrough calls. The UAS threads this into the
        /// bridge spawn so each leg runs with the right transform.
        srtp: Option<SrtpKeys>,
    },
    /// No common codec — responder should send `488 Not Acceptable Here`.
    Mismatch,
    /// Offer body was malformed; responder should send `400 Bad Request`.
    Malformed(String),
}

/// SDP offer/answer engine.
///
/// Stateless: the engine-wide `local_rtp_port` is supplied by the
/// caller for each negotiation, because the port is dialog-specific.
/// Implementations must be `Sync` so a single instance can serve
/// concurrent requests without locking.
pub trait SdpNegotiator: Send + Sync {
    /// Consume an SDP offer body and produce an answer carrying
    /// `local_rtp_port` as the engine's media port and `local_ip` as
    /// the engine-side address to publish in `c=` / `o=`.
    ///
    /// `local_ip` lets the caller override whatever address the
    /// negotiator was seeded with — essential when the signaling
    /// transport is bound to `0.0.0.0` / `::` and the routable address
    /// varies by peer.
    fn negotiate_audio(
        &self,
        offer_body: &str,
        local_ip: IpAddr,
        local_rtp_port: u16,
    ) -> NegotiationOutcome;

    /// Build a UAC-side SDP offer advertising `local_ip` +
    /// `local_rtp_port`. Called by `smiths-sip::UacClient` when the
    /// engine places an outbound INVITE. Default implementations
    /// should advertise whatever codec set the negotiator supports;
    /// the MVP `smiths-sdp::Negotiator` emits a PCMU-only offer.
    fn build_offer(&self, local_ip: IpAddr, local_rtp_port: u16) -> String;

    /// Parse an SDP answer (typically received in a 200 OK to our
    /// INVITE) and return the peer's audio RTP endpoint. Returns
    /// `None` when the body is malformed, has no audio stream, or
    /// declares port 0.
    fn parse_remote_rtp(&self, answer_body: &str) -> Option<SocketAddr>;
}
