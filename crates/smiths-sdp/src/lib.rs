//! SDP parse/generate and offer/answer negotiation.
//!
//! Minimal subset of RFC 8866 needed for audio passthrough calls:
//! `v=`, `o=`, `s=`, `c=`, `t=`, `m=audio`, `a=rtpmap`, and the four
//! direction attributes. Enough to carry PCMU / PCMA / Opus between two
//! UAs through the engine.
//!
//! Also parses (but does not yet negotiate end-to-end) the DTLS-SRTP +
//! ICE surface needed for WebRTC interop: `a=fingerprint`, `a=setup`,
//! `a=ice-ufrag`, `a=ice-pwd`, `a=ice-options`, and `a=candidate`.
//! Typed fields live on [`MediaDescription`]; the negotiator uses them
//! to recognize DTLS-SRTP offers and emit `488` with a descriptive
//! `Warning:` header so peers get a clear diagnostic until the
//! handshake layer (slice 1.3) lands.
//!
//! Not in scope: bandwidth (`b=`), timing repeats, encryption keys
//! (`k=`), `a=fmtp`/`a=ptime`, `rtcp-mux` (accepted but not required).

// Per-package tightening, same pattern as smiths-core: no production
// code uses `.unwrap()` / `.expect()`; the lint guards drift.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Slice 1.7: missing_docs promoted on smiths-sdp alongside smiths-core
// and smiths-proto. Every public item in the SDP model carries a
// one-line description; CI (slice 1.8) gates drift.
#![warn(missing_docs)]

pub mod error;
pub mod negotiate;
pub mod parse;
pub mod privacy;
pub mod srtp_attr;
pub mod types;

pub use error::ParseError;
pub use negotiate::{NegotiationResult, Negotiator, fresh_sdes_key};
pub use privacy::{
    OfferPrivacyVerdict, redact_ip, reject_direct_candidates, strip_host_candidates,
    strip_host_candidates_on,
};
pub use srtp_attr::{SdesCrypto, SdesParseError};
pub use types::{
    ConnectionInfo, Direction, DtlsSetup, Fingerprint, IceCandidate, IcePassword, MediaDescription,
    MediaKind, Origin, RtpMap, SessionDescription,
};
