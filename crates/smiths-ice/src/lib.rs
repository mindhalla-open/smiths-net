//! ICE MVP — host candidates + STUN Binding checks.
//!
//! Scope of this slice (1.4):
//!
//! - Hand-rolled STUN Binding Request / Response parser + encoder
//!   per RFC 8489, with [`XorMappedAddress`] the one attribute we
//!   actually inspect.
//! - Host-candidate gathering: enumerate the IPv4/IPv6 addrs of the
//!   given `UdpSocket`-binding IP and emit one
//!   [`smiths_sdp::IceCandidate`] per address.
//! - Connectivity check primitive: [`binding_ping`] — bind a UDP
//!   socket, fire a Binding Request, await the Response.
//!
//! Out of scope (slice 1.5+ or follow-ons):
//!
//! - STUN short-term credential mechanism (`USERNAME` +
//!   `MESSAGE-INTEGRITY` + `FINGERPRINT`). The MVP checks are
//!   unauthenticated — fine for the LAN loopback scenario that
//!   closes this slice; a full pairing implementation will layer
//!   short-term creds on top.
//! - Candidate pairing / priority ordering per RFC 8445 §6. The one
//!   pair we exercise is `local host → remote host`; when ICE grows
//!   server-reflexive + relay candidates this module grows a proper
//!   pairing algorithm.
//! - Trickle ICE (`a=end-of-candidates`, re-INVITE piggybacking).

// Same lint posture as smiths-core: no production-code unwraps.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod candidate;
pub mod config;
pub mod stun;
pub mod turn;

pub use agent::{IceAgent, IcePair, PairState};
pub use candidate::{CandidateError, CandidateGatherer, gather_host_candidates};
pub use config::IceConfig;
pub use stun::{StunClass, StunError, StunMessage, StunMethod, TransactionId, binding_ping};
pub use turn::{
    AllocateOutcome, DEFAULT_LIFETIME_S, LongTermCredential, TurnServer, TurnServerConfig,
};
