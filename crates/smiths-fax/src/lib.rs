//! T.38 FAX-over-IP (slice 5.4 / P13).
//!
//! Fax signaling survives on the PSTN today mostly because G.711
//! audio transcoded through a packet network shreds the modem
//! negotiation — jitter and dropped samples that a voice call shrugs
//! off destroy a V.17/V.34 handshake. T.38 fixes this by demodulating
//! the fax tones at the gateway, carrying the **structured** IFP
//! (Internet Facsimile Protocol) frames over UDPTL, and remodulating
//! at the far end. The transport is loss-tolerant by design: UDPTL's
//! error-recovery field (T.38 Annex A) lets a receiver reconstruct a
//! lost primary frame from one of the next N packets' redundancy
//! copies.
//!
//! ## What this crate ships (slice 5.4)
//!
//! - [`udptl`] — UDPTL framing. Parse + emit primary IFP + secondary
//!   (redundancy) IFP frames with per-packet length prefixes. Pure
//!   bytes; no IFP semantics.
//! - [`session::UdptlSession`] — [`MediaSession`] impl that relays
//!   UDPTL datagrams between two peers. Forwards whole datagrams
//!   byte-identity; no rewrite, no SSRC mangling (UDPTL has no SSRC).
//! - [`sdp`] — helpers to detect `m=image 0 udptl t38`, extract the
//!   T.38 attribute surface (`T38FaxVersion`, `T38MaxBitRate`,
//!   `T38FaxRateManagement`, `T38FaxUdpEC`, …), and emit the answer.
//! - [`renegotiate::fax_renegotiate`] — takes an active audio
//!   [`SessionDescription`](smiths_sdp::SessionDescription) and a
//!   local UDPTL port, returns a re-INVITE SDP that declines the
//!   audio m-line (`m=audio 0 …`) and adds `m=image <port> udptl t38`.
//!   The UAC calls this on CED-tone detection; the UAS answers with
//!   its own [`sdp::answer_fax_offer`] output.
//!
//! ## What this crate does NOT do (slice 5.4)
//!
//! - **IFP state machine.** T.38 carries a full fax FSM over the
//!   wire (V.21 preamble, V.27ter/V.29/V.17 training, page data, MCF
//!   ack, …). That's the job of a terminal or gateway — the engine's
//!   role in the T.38 call is a **relay**, identical in spirit to
//!   the RTP passthrough bridge.  Terminals + gateways live outside
//!   the engine; the common deployment pattern is two ATA/PBX boxes
//!   negotiating T.38 through us.
//! - **spandsp FFI.** The C `spandsp` library is the canonical IFP
//!   reference implementation. Pulling it in conflicts with the
//!   workspace's `unsafe_code = "deny"` policy; the reference tests
//!   here use canned fixture bytes instead.
//! - **Bridge integration.** Wiring a `UdptlSession` into the UAS
//!   / media-fabric renegotiation path (detect CED tone, emit
//!   re-INVITE, swap the `dyn MediaSession` in place) is the same
//!   call-FSM refactor the slice 5.1 video dual-bridge and slice
//!   5.3 transcoding work are queued behind. Primitives + SDP +
//!   session type + renegotiation helper land here; wiring lands
//!   alongside them.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod renegotiate;
pub mod sdp;
pub mod session;
pub mod udptl;

pub use renegotiate::{FaxRenegotiateError, fax_renegotiate};
pub use sdp::{T38Params, answer_fax_offer, find_fax_media, offer_fax};
pub use session::{UdptlSession, UdptlSessionConfig};
pub use udptl::{UdptlError, UdptlPacket};
