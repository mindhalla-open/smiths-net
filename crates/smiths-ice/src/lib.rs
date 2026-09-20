//! ICE (RFC 8445) with an embedded STUN codec (RFC 8489) and TURN
//! server + client (RFC 8656).
//!
//! - [`stun`] — Binding request / response / indication codec with
//!   the ICE attributes and the short-term credential mechanism
//!   (`USERNAME`, `MESSAGE-INTEGRITY`, `FINGERPRINT`), plus
//!   [`binding_ping`] and [`gather_srflx_candidates`] for
//!   server-reflexive gathering against a public STUN server.
//! - [`candidate`] — host / srflx / relay candidate gathering
//!   ([`CandidateGatherer`]).
//! - [`agent`] — the sans-IO [`IceAgent`]: candidate pairing,
//!   authenticated connectivity checks with retransmits, triggered
//!   checks, peer-reflexive discovery, role-conflict resolution,
//!   nomination, keepalives and a `Failed` terminal state. The
//!   module doc describes the driver call sequence.
//! - [`turn`] — the embedded [`TurnServer`] and the [`TurnClient`]
//!   used for relay candidates.
//!
//! Configuration lives on [`IceConfig`] (`[ice]` block).

// Same lint posture as smiths-core: no production-code unwraps.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod candidate;
pub mod config;
pub mod stun;
pub mod turn;

pub use agent::{IceAgent, IcePair, IceState, Outgoing, PairState};
pub use candidate::{CandidateError, CandidateGatherer, gather_host_candidates};
pub use config::IceConfig;
pub use stun::{
    StunClass, StunError, StunMessage, StunMethod, TransactionId, binding_ping,
    gather_srflx_candidates, is_stun, verify_fingerprint, verify_message_integrity,
};
pub use turn::{
    AllocateOutcome, DEFAULT_LIFETIME_S, LongTermCredential, TurnClient, TurnClientError,
    TurnServer, TurnServerConfig,
};
