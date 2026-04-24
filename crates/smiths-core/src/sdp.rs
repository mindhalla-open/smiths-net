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
use crate::call::NegotiatedCodec;

/// DTLS-SRTP parameters extracted from an SDP offer (slice
/// 5.10-dtls). When the UDP/TLS/RTP/SAVP(F) profile is used, SRTP
/// keys aren't on the wire — they're derived from the DTLS
/// handshake after offer/answer completes. The negotiator surfaces
/// the raw material the handshake driver needs, and the media
/// fabric runs the handshake.
///
/// ## Handedness
///
/// - `peer_fingerprint_algorithm` + `peer_fingerprint_value` come
///   from the offer's `a=fingerprint:` line, verbatim on the
///   value so the byte-exact comparison against the cert
///   `webrtc-dtls` negotiates doesn't drift.
/// - `local_role` is what the **engine** plays during the
///   handshake, already resolved against the offer's
///   `a=setup:` (per RFC 5763 §5: offer `actpass` →
///   answer `active`, offer `passive` → answer `active`, etc.).
///   The answer's `a=setup:` echoes `local_role`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtlsParams {
    /// Peer's declared fingerprint hash algorithm — typically
    /// `"sha-256"`. Stored as-is so the handshake can reject
    /// weak algorithms before running the expensive DTLS
    /// handshake.
    pub peer_fingerprint_algorithm: String,
    /// Peer's fingerprint value in RFC 8122 wire form
    /// (`AA:BB:...`, uppercase hex, colon-separated pairs).
    pub peer_fingerprint_value: String,
    /// The role the engine plays during the DTLS handshake.
    /// `"active"` means we send `ClientHello`; `"passive"`
    /// means we await it. Resolved from the offer's
    /// `a=setup:` attribute per RFC 5763 §5.
    pub local_role: DtlsRole,
}

/// DTLS-SRTP handshake role at the media layer. Mirrors
/// [`smiths_dtls::DtlsRole`] but lives in `smiths-core` so the
/// SDP trait seam doesn't force every crate that consumes
/// [`NegotiationOutcome`] to depend on the DTLS crate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DtlsRole {
    /// Engine initiates the handshake (`ClientHello`). Matches
    /// offerer `setup:passive` or `setup:actpass`.
    Client,
    /// Engine awaits the handshake. Matches offerer
    /// `setup:active`.
    Server,
}

impl DtlsRole {
    /// Wire-format token for `a=setup:` answers.
    #[must_use]
    pub fn as_setup_str(self) -> &'static str {
        match self {
            Self::Client => "active",
            Self::Server => "passive",
        }
    }
}

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
///
/// `Accepted` is an ~240 B variant; the error variants are tens of
/// bytes. Boxing the big variant would cascade into every
/// destructuring call-site (UAS, WebRTC handler, tests) for a
/// memory win that doesn't register — SDP negotiation fires once
/// per INVITE, not per packet. We eat the size disparity.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum NegotiationOutcome {
    /// Offer accepted.
    Accepted {
        /// Serialized SDP answer body to embed in the 200 OK.
        answer_body: String,
        /// Peer's first audio RTP endpoint (`m=audio port` + `c=`),
        /// used by the bridge to send forward traffic. `None` if the
        /// offer declared port 0 or omitted a connection line.
        remote_media: Option<SocketAddr>,
        /// Peer's video RTP endpoint (slice 5.1 / P11). Populated
        /// when the offer carried a non-zero `m=video` block with at
        /// least one codec in the passthrough set (H.264 / VP8 /
        /// VP9). `None` when the offer was audio-only, declared
        /// video port 0, or advertised only video codecs the
        /// negotiator can't identify. The UAS spawns a second
        /// [`crate::MediaFabric::bridge`] on this endpoint — that
        /// wiring is a follow-on; today the field surfaces the
        /// peer's video address so deployments that need it can opt
        /// in without a second negotiation pass.
        video_media: Option<SocketAddr>,
        /// SRTP keying material when the offer asked for `RTP/SAVP`
        /// with a supported `a=crypto:` suite. `None` for plain
        /// `RTP/AVP` passthrough calls. The UAS threads this into the
        /// bridge spawn so each leg runs with the right transform.
        ///
        /// **Mutually exclusive with [`Self::dtls`].** SDES and
        /// DTLS-SRTP are alternative key-exchange protocols in
        /// SDP — a well-formed offer picks one transport profile.
        /// Callers that see both fields populated should treat it
        /// as a negotiator bug.
        srtp: Option<SrtpKeys>,
        /// DTLS-SRTP parameters (slice 5.10-dtls). `Some` when the
        /// offer used `UDP/TLS/RTP/SAVP[F]` with an acceptable
        /// `a=fingerprint:` + `a=setup:` combination. The media
        /// fabric runs the actual handshake; SRTP keys come out
        /// via the RFC 5764 §4.2 PRF once the handshake
        /// completes.
        dtls: Option<DtlsParams>,
        /// Audio codec both sides agreed on (slice 5.6). `None`
        /// when the offer had no audio m-line or no common codec
        /// (but then `Mismatch` would have fired). Recorded on the
        /// `DialogRecord`'s `per_leg_codec` map — the transcoding
        /// router (5.6b) compares the two legs' entries to decide
        /// whether a `CallTranscoder` is needed.
        audio_codec: Option<NegotiatedCodec>,
        /// Video codec (slice 5.6). Populated only when the
        /// negotiator accepted a `m=video` block; `None` when
        /// video was declined or absent.
        video_codec: Option<NegotiatedCodec>,
    },
    /// No common codec — responder should send `488 Not Acceptable Here`.
    Mismatch,
    /// Transport profile the engine recognizes but can't terminate yet
    /// (today: `UDP/TLS/RTP/SAVP` — DTLS-SRTP). Responder should reply
    /// `488 Not Acceptable Here` with a `Warning: 399` header carrying
    /// `reason` so the peer knows the call was rejected because of
    /// transport support, not codec mismatch. Distinct variant so the
    /// UAS doesn't have to string-sniff `Mismatch`.
    UnsupportedTransport {
        /// Short, operator-facing reason ("DTLS-SRTP not yet supported",
        /// etc). Emitted into the SIP `Warning:` header verbatim.
        reason: String,
    },
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

    /// Multi-stream negotiation (slice 5.1 / P11). Same contract as
    /// [`Self::negotiate_audio`] but carries an optional video port
    /// so the negotiator can emit a matching `m=video` block on the
    /// answer. `video_port = None` declines any video the offer
    /// carries (answer emits `m=video 0 ...` to preserve m-line
    /// ordering, as required for re-INVITE interop); `Some(port)`
    /// accepts passthrough video if at least one offered codec is
    /// in the supported set.
    ///
    /// Default impl delegates to [`Self::negotiate_audio`] with the
    /// video port dropped — so implementations that predate 5.1
    /// stay trait-compatible. The in-tree
    /// `smiths_sdp::Negotiator` overrides this to do real video
    /// passthrough.
    fn negotiate(
        &self,
        offer_body: &str,
        local_ip: IpAddr,
        local_audio_port: u16,
        _local_video_port: Option<u16>,
    ) -> NegotiationOutcome {
        self.negotiate_audio(offer_body, local_ip, local_audio_port)
    }

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
