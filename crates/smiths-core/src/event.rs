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
    /// A WASM plugin called `smiths::publish_event` with a free-form
    /// topic + payload. The host forwards it verbatim so subscribers
    /// can react without the plugin needing to know about the
    /// engine's internal event taxonomy.
    Published {
        /// Plugin name from the host state.
        plugin: String,
        /// Caller-chosen topic string.
        topic: String,
        /// Raw payload bytes the plugin wrote into its buffer.
        data: Vec<u8>,
    },
    /// A WASM plugin called `smiths::timer_set` and its timer fired.
    /// `event_id` is the tag the plugin passed so it can match the
    /// fire-back to the scheduling site on its own.
    TimerFired {
        /// Plugin name from the host state.
        plugin: String,
        /// Opaque guest-chosen tag (passed back verbatim).
        event_id: i32,
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
    /// Media-plane security failure — typed so dashboards + alerting
    /// treat DTLS fingerprint mismatches and SRTP auth-tag failures as
    /// first-class events rather than scraping logs. Emitted from the
    /// media fabric when a leg tears down for a crypto reason.
    MediaSecurityError {
        /// `Call-ID` of the affected dialog, when known.
        call_id: Option<String>,
        /// Failure class — see [`MediaSecurityFailure`].
        kind: MediaSecurityFailure,
        /// Human-readable detail, safe to log.
        detail: String,
    },
}

/// Media-plane security failure kinds. Stable — operators' dashboards
/// key on these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaSecurityFailure {
    /// DTLS handshake's peer cert did not match the SDP-advertised
    /// fingerprint (RFC 5763 §8).
    DtlsFingerprint,
    /// DTLS handshake failed for a non-fingerprint reason — cipher
    /// mismatch, malformed message, peer reset.
    DtlsHandshake,
    /// SRTP auth-tag verification failed on an inbound packet.
    SrtpAuthTag,
    /// Negotiated protection profile isn't one we support (MVP is
    /// `AES_CM_128_HMAC_SHA1_80` only).
    UnsupportedSuite,
}
