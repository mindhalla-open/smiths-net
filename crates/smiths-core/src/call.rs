//! Call / dialog state types, shared across the engine.
//!
//! This is the canonical, serializable snapshot of a SIP dialog the
//! engine holds. UAS / UAC code composes this record with runtime
//! resources (sockets, bridges) that live outside the serializable
//! state — per the HA MVP guardrail, everything here is `Serialize`
//! and `Deserialize` so a future replication layer can snapshot the
//! full call set without touching SIP internals.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

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
    pub call_id: String,
    pub local_tag: String,
    pub remote_tag: String,
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
