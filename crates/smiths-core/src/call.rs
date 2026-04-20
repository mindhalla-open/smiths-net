//! Call / dialog state types, shared across the engine.
//!
//! This is the canonical, serializable snapshot of a SIP dialog the
//! engine holds. UAS / UAC code composes this record with runtime
//! resources (sockets, bridges) that live outside the serializable
//! state — per the HA MVP guardrail, everything here is `Serialize`
//! and `Deserialize` so a future replication layer can snapshot the
//! full call set without touching SIP internals.
//!
//! Also hosts the [`CallOriginator`] seam — MCP / A2A tools that
//! place outbound calls go through this trait, not through the SIP
//! crate directly, so the control plane stays adapter-agnostic.

use std::net::SocketAddr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::media::EndpointId;

/// Three-tuple identifying a dialog: `(Call-ID, local tag, remote tag)`.
///
/// Matches RFC 3261 §12.1 dialog identification.
pub type DialogKey = (String, String, String);

/// Dialog lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogState {
    /// 2xx response sent, ACK not yet observed.
    Early,
    /// ACK received; call established.
    Confirmed,
}

/// Serializable dialog record.
///
/// Holds only values — no sockets, no task handles, no bridge owners.
/// Runtime resources (media endpoint allocations, bridge handles) are
/// referenced by ID fields and live in `MediaFabric` + the owning
/// subsystem (today: UAS). That split is what lets this type derive
/// `Serialize` for the HA snapshot path.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DialogRecord {
    /// `Call-ID` header value — identifies the end-to-end dialog.
    pub call_id: String,
    /// Tag this engine picked for the dialog's local leg.
    pub local_tag: String,
    /// Tag the peer sent in the INVITE's From header.
    pub remote_tag: String,
    /// Current dialog lifecycle state (Early / Confirmed).
    pub state: DialogState,
    /// Peer's SIP signaling address (source of the INVITE).
    pub peer_signal: SocketAddr,
    /// Optional rendezvous key (Request-URI user-part) used to pair two
    /// INVITEs into a bridge.
    pub rendezvous: Option<String>,
    /// Handle to the media endpoint the engine allocated for this
    /// dialog, if any. `None` when the call carried no SDP.
    pub media: Option<EndpointId>,
    /// Peer's RTP endpoint learned from the SDP offer, if any.
    pub remote_media: Option<SocketAddr>,
    /// Cached 2xx final-response bytes, parked here so the UAS can
    /// drive the RFC 3261 §13.3.1.4 per-dialog retransmit loop until
    /// ACK confirms the dialog. Cleared on Early → Confirmed. Stored
    /// on the record (not a side table) so an HA snapshot captures
    /// any in-flight 2xx retransmit — a failover primary can resume
    /// the schedule without dropping the call. `None` everywhere the
    /// dialog is already Confirmed or the call never carried an
    /// INVITE 2xx (registrar-only dialogs, etc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_2xx: Option<Vec<u8>>,
}

impl DialogRecord {
    /// Compose the dialog key.
    #[must_use]
    pub fn key(&self) -> DialogKey {
        (
            self.call_id.clone(),
            self.local_tag.clone(),
            self.remote_tag.clone(),
        )
    }
}

/// Seam for "given a call-id, where do I send RTP?" lookups. The MCP
/// control plane implements this against its live `ControlState`;
/// `smiths-wasm` consumes it via [`smiths-core::media::MediaFabric`]
/// so WASM guests can call `smiths::send_rtp` without linking the
/// MCP crate. Returns `None` for unknown / media-less calls.
pub trait CallLookup: Send + Sync {
    /// Return the media endpoint id + peer RTP address for `call_id`,
    /// or `None` if the call is unknown or has no media attached.
    fn endpoint_for(&self, call_id: &str) -> Option<(EndpointId, SocketAddr)>;
}

/// Errors surfaced by a [`CallOriginator`] operation.
#[derive(Debug, Error)]
pub enum CallError {
    /// The target URI could not be parsed or resolved.
    #[error("invalid target: {0}")]
    InvalidTarget(String),
    /// Remote peer rejected the INVITE (4xx/5xx/6xx final response).
    #[error("rejected: {status} {reason}")]
    Rejected {
        /// Numeric status code from the SIP response.
        status: u16,
        /// Human-readable reason phrase.
        reason: String,
    },
    /// No dialog exists for the supplied Call-ID.
    #[error("no such call: {0}")]
    NotFound(String),
    /// Operation timed out waiting for a response.
    #[error("timeout after {millis} ms")]
    Timeout {
        /// Configured budget for the operation.
        millis: u64,
    },
    /// Transport / fabric / parser failure — opaque message.
    #[error("{0}")]
    Internal(String),
}

/// Read-only snapshot of one SIP registration binding. Populated by
/// `smiths-sip`'s auth stores and surfaced through MCP's
/// `sip://registrations` resource. Kept in `smiths-core` so the MCP
/// crate doesn't need to pull in the SIP crate just to inspect live
/// registrations.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RegistrationSnapshot {
    /// Address-of-record (`sip:user@realm`).
    pub aor: String,
    /// Contact URI the UA registered.
    pub contact: String,
    /// Unix seconds at which this binding expires.
    pub expires_at_unix: i64,
}

/// Read-only observability surface over a subscriber-DB's live
/// registrations. Implemented by `smiths-sip::auth::sqlite_store::
/// SqliteAuthStore` (and future backends). The MCP control plane
/// uses it to render `sip://registrations` without taking a
/// cross-crate dep on the SIP auth module.
///
/// Intentionally one method — snapshots — so third-party stores
/// (HTTP webhook, sidecar plugin) can expose "what's registered
/// right now?" for ops without first mirroring the whole
/// `RegistrationStore` mutation surface.
pub trait RegistrationView: Send + Sync + 'static {
    /// Every live (non-expired) binding the store knows about.
    /// Implementations should filter out already-expired rows
    /// before returning.
    fn snapshot(&self) -> Vec<RegistrationSnapshot>;
}

/// Originator surface consumed by the MCP `make_call` / `end_call`
/// tools. Implemented by `smiths-sip::UacClient`; the trait seam
/// keeps `smiths-mcp` free of any direct dependency on the SIP crate.
#[async_trait]
pub trait CallOriginator: Send + Sync {
    /// Place an outbound call to `target` (a SIP URI like
    /// `sip:alice@example.com:5060`). Returns the freshly-allocated
    /// `Call-ID` once the remote sends 200 OK.
    async fn place_call(&self, target: &str) -> Result<String, CallError>;

    /// Tear down one of our outbound calls by `Call-ID`. No-op /
    /// `NotFound` for unknown ids.
    async fn hangup(&self, call_id: &str) -> Result<(), CallError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time proof: `DialogRecord` implements the traits the HA
    /// guardrail requires.
    #[test]
    fn record_is_serialize_and_clone() {
        fn assert_bounds<T: Clone + Serialize + for<'de> Deserialize<'de>>() {}
        assert_bounds::<DialogRecord>();
    }
}
