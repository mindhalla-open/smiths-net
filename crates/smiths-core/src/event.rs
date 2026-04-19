//! Typed event set flowing across the engine's internal bus.
//!
//! The bus is the single mechanism for cross-module communication.
//! Modules publish variants of [`Event`] and subscribe to a broadcast
//! receiver; they never call each other directly.
//!
//! Phase 0 only defined system lifecycle events. Phase 1 adds SIP
//! signaling events. Media / control / plugin variants follow.

use std::net::SocketAddr;

use serde_json::Value;

use crate::media::EndpointId;

/// Top-level event envelope.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Event {
    /// Engine-wide lifecycle signals.
    System(SystemEvent),
    /// SIP signaling events.
    Sip(SipEvent),
    /// Plugin-originated events forwarded from sidecar notifications.
    Plugin(PluginEvent),
}

/// Plugin → engine event stream.
///
/// Today one variant: a bare forwarding of the plugin's JSON-RPC
/// notification. Specialised variants (streaming ASR partials,
/// streaming TTS audio chunks) may land as typed events later, but
/// the generic `Notification` path stays so unknown methods keep
/// flowing end-to-end.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PluginEvent {
    /// A JSON-RPC notification the plugin emitted over stdout.
    Notification {
        /// Plugin name (from its manifest).
        plugin: String,
        /// Notification method (e.g. `"emit_partial"`).
        method: String,
        /// Params the plugin attached, if any.
        params: Option<Value>,
    },
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
        /// Engine-allocated media endpoint, if the dialog carries media.
        media_endpoint: Option<EndpointId>,
        /// Peer's RTP endpoint learned from the SDP offer, if any.
        remote_rtp: Option<SocketAddr>,
    },
    /// An existing dialog has been terminated (after `BYE`).
    DialogTerminated {
        /// `Call-ID` header value.
        call_id: String,
    },
}
