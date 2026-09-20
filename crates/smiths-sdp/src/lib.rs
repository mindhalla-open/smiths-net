//! SDP parse/generate and offer/answer negotiation.
//!
//! The subset of RFC 8866 a passthrough B2BUA needs: `v=`, `o=`,
//! `s=`, `c=`, `t=`, `m=` (audio, video, image), `a=rtpmap`,
//! `a=fmtp`, `a=ptime` / `a=maxptime`, the four direction attributes,
//! and `a=group:BUNDLE` / `a=mid`. `m=` format tokens are kept as
//! strings so non-RTP lines such as `m=image 6250 udptl t38` parse;
//! [`MediaDescription::payload_types`] gives the numeric view.
//!
//! WebRTC / SRTP surface: `a=crypto` (SDES), `a=fingerprint`,
//! `a=setup`, `a=ice-ufrag`, `a=ice-pwd`, `a=ice-options`,
//! `a=ice-lite`, `a=candidate`, `a=end-of-candidates`, `a=extmap`,
//! `a=rtcp-mux`, `a=rtcp`, `a=rtcp-fb`. Session-level DTLS / ICE
//! attributes are inherited by every media block.
//!
//! T.38 fax: `a=T38FaxVersion` and the other `a=T38…` attributes
//! parse into [`T38Params`] on the media block.
//!
//! Not in scope: bandwidth (`b=`), timing repeats, encryption keys
//! (`k=`), `a=ssrc` groups, RTP header-extension semantics beyond
//! echoing the mapping.

// Per-package tightening, same pattern as smiths-core: no production
// code uses `.unwrap` / `.expect`; the lint guards drift.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Every public item in the SDP model carries a one-line description;
// CI gates drift.
#![warn(missing_docs)]

pub mod error;
pub mod negotiate;
pub mod parse;
pub mod privacy;
pub mod srtp_attr;
pub mod types;

pub use error::ParseError;
pub use negotiate::{
    NegotiationResult, Negotiator, fresh_ice_pwd, fresh_ice_ufrag, fresh_sdes_key,
    make_host_candidate,
};
pub use privacy::{
    OfferPrivacyVerdict, redact_ip, reject_direct_candidates, strip_host_candidates,
    strip_host_candidates_on,
};
pub use srtp_attr::{SdesCrypto, SdesParseError};
pub use types::{
    ConnectionInfo, Direction, DtlsSetup, ExtMap, Fingerprint, Fmtp, Group, IceCandidate,
    IcePassword, MediaDescription, MediaKind, Origin, RtcpAttr, RtcpFeedback, RtpMap,
    SessionDescription, T38Params,
};
