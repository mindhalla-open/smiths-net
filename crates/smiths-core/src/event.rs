//! Typed event set flowing across the engine's internal bus.
//!
//! The bus is the single mechanism for cross-module communication.
//! Modules publish variants of [`Event`] and subscribe to a broadcast
//! receiver; they never call each other directly.
//!
//! Phase 0 only defined system lifecycle events. Phase 1 adds SIP
//! signaling events. Media / control / plugin variants follow.

use std::net::SocketAddr;

/// Top-level event envelope.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Event {
    /// Engine-wide lifecycle signals.
    System(SystemEvent),
    /// SIP signaling events.
    Sip(SipEvent),
}

/// Lifecycle signals emitted by the main runtime.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SystemEvent {
    /// All subsystems initialized; the engine is accepting traffic.
    Ready,
    /// A shutdown request has been received (signal or explicit trigger).
    ShutdownRequested,
    /// All subsystems drained; the process is about to exit.
    ShutdownComplete,
}

/// SIP signaling events emitted by `smiths-sip`.
///
/// String fields carry the minimum routing info needed by subscribers
/// that do not link `smiths-sip`. Rich typed views live inside the SIP
/// subsystem.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SipEvent {
    /// Well-formed SIP request received from the network.
    RequestReceived {
        /// Source peer.
        peer: SocketAddr,
        /// SIP method (e.g. `"OPTIONS"`).
        method: String,
        /// `Call-ID` header value, if present.
        call_id: Option<String>,
    },
    /// Response sent back to a peer.
    ResponseSent {
        /// Destination peer.
        peer: SocketAddr,
        /// Numeric status code (e.g. `200`, `405`).
        status: u16,
        /// `Call-ID` header value, if present.
        call_id: Option<String>,
    },
    /// Incoming datagram could not be parsed as SIP.
    ParseError {
        /// Source peer.
        peer: SocketAddr,
        /// Human-readable reason.
        reason: String,
    },
    /// A new dialog has been created (after a 2xx to `INVITE`).
    DialogCreated {
        /// `Call-ID` header value.
        call_id: String,
    },
    /// An existing dialog has been terminated (after `BYE`).
    DialogTerminated {
        /// `Call-ID` header value.
        call_id: String,
    },
}
