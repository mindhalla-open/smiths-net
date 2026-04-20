//! SDP parse/generate and offer/answer negotiation.
//!
//! Minimal subset of RFC 8866 needed for audio passthrough calls:
//! `v=`, `o=`, `s=`, `c=`, `t=`, `m=audio`, `a=rtpmap`, and the four
//! direction attributes. Enough to carry PCMU / PCMA / Opus between two
//! UAs through the engine.
//!
//! Not in scope: bandwidth (`b=`), timing repeats, encryption keys
//! (`k=`), `a=fmtp`/`a=ptime`, ICE candidates, `rtcp-mux` (accepted but
//! not required).

// Per-package tightening, same pattern as smiths-core: no production
// code uses `.unwrap()` / `.expect()`; the lint guards drift.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod error;
pub mod negotiate;
pub mod parse;
pub mod srtp_attr;
pub mod types;

pub use error::ParseError;
pub use negotiate::{NegotiationResult, Negotiator, fresh_sdes_key};
pub use srtp_attr::{SdesCrypto, SdesParseError};
pub use types::{
    ConnectionInfo, Direction, MediaDescription, MediaKind, Origin, RtpMap, SessionDescription,
};
