//! SDP offer/answer trait seam.
//!
//! `smiths-sip` only sees the trait + [`NegotiationOutcome`] enum; it
//! does not parse SDP or depend on `smiths-sdp`. Implementations
//! (today: `smiths-sdp::Negotiator`, tomorrow: transcoding-aware
//! variants) return a ready-to-embed answer body plus the peer RTP
//! endpoint derived from the offer — SIP never introspects the parse
//! tree.

use std::net::{IpAddr, SocketAddr};

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
}
