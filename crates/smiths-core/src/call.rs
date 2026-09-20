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

use std::collections::BTreeMap;
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

/// Stable identifier for one leg of a call.
///
/// Point-to-point calls have two legs — `LegId(0)` is the offerer
/// (typically the UAC / calling UA), `LegId(1)` is the answerer.
/// Conference participants each get their own `LegId`. The engine's
/// own side is never a `LegId` because the engine doesn't "speak" —
/// it relays, transcodes, or mixes.
///
/// The type is a newtype to keep the FSM's leg-vs-participant-vs-
/// endpoint distinctions un-confused on the wire: a `LegId` can't
/// accidentally be treated as an [`EndpointId`] or a participant id.
#[derive(
    Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct LegId(pub u64);

impl std::fmt::Display for LegId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "leg-{}", self.0)
    }
}

/// Kind of media a session carries.
///
/// Mirrors `smiths_sdp::MediaKind` but lives in `smiths-core` so the
/// call FSM and the `SessionKey` data model don't pull in the SDP
/// crate. Parse / `as_str` round-trip through the canonical SDP
/// tokens (`"audio"`, `"video"`, `"image"`, `"application"`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKindTag {
    /// `m=audio` — voice, DTMF tones.
    Audio,
    /// `m=video` — video passthrough.
    Video,
    /// `m=image` — T.38 FAX-over-IP.
    Image,
    /// `m=application` — MSRP, data-channel, BFCP, …
    Application,
    /// Anything else; preserved verbatim.
    Other(String),
}

impl MediaKindTag {
    /// Parse the `<kind>` token on an `m=` line.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "audio" => Self::Audio,
            "video" => Self::Video,
            "image" => Self::Image,
            "application" => Self::Application,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Wire-format token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Image => "image",
            Self::Application => "application",
            Self::Other(s) => s,
        }
    }
}

impl std::fmt::Display for MediaKindTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Key that identifies one media session inside a dialog
///. Two legs × audio+video = four keys for a full
/// audio-plus-video call; a fax-renegotiated call briefly has two
/// audio keys (being torn down) plus two image keys (replacing them)
/// before the audio keys drop.
///
/// `(LegId, MediaKindTag)` is the natural tuple — the call FSM looks
/// sessions up by it; atomic [`DialogSessions::swap`] operates on it.
pub type SessionKey = (LegId, MediaKindTag);

/// Codec negotiated for one leg of a call.
///
/// Parallel to `smiths_transcode::CodecKind` but lives in
/// `smiths-core` so the call FSM can track codecs without the
/// `smiths-core → smiths-transcode` dependency edge. The transcoder
/// crate builds its own `CodecKind::from(NegotiatedCodec)` bridge
/// when it wires through (5.6b).
///
/// Carries everything the router needs to answer "do we need a
/// transcoder?" — two legs' codecs compare equal iff the call can
/// run passthrough.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NegotiatedCodec {
    /// G.711 μ-law, 8 kHz.
    Pcmu,
    /// G.711 A-law, 8 kHz.
    Pcma,
    /// G.722, 16 kHz (sampled at 8 kHz by convention).
    G722,
    /// Opus — any sample rate the peer advertised.
    Opus,
    /// H.264 video ( passthrough).
    H264,
    /// VP8 video.
    Vp8,
    /// VP9 video.
    Vp9,
    /// `image/t38` — T.38 FAX-over-IP carries no RTP codec but we
    /// record the media kind here so `per_leg_codec` is always
    /// populated for every media leg.
    T38,
    /// Anything else; preserved as the SDP `a=rtpmap:<pt> <name>/…`
    /// codec token verbatim (lowercase).
    Other(String),
}

impl NegotiatedCodec {
    /// Parse from a codec-name token as it appears on an SDP
    /// `a=rtpmap:` line. Case-insensitive.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "pcmu" => Self::Pcmu,
            "pcma" => Self::Pcma,
            "g722" => Self::G722,
            "opus" => Self::Opus,
            "h264" => Self::H264,
            "vp8" => Self::Vp8,
            "vp9" => Self::Vp9,
            "t38" => Self::T38,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Canonical wire-format token (lowercase).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pcmu => "pcmu",
            Self::Pcma => "pcma",
            Self::G722 => "g722",
            Self::Opus => "opus",
            Self::H264 => "h264",
            Self::Vp8 => "vp8",
            Self::Vp9 => "vp9",
            Self::T38 => "t38",
            Self::Other(s) => s,
        }
    }
}

impl std::fmt::Display for NegotiatedCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// Per-leg negotiated codec. Populated by the UAS at
    /// 200 OK INVITE time from the negotiator's output; empty for
    /// dialogs that never carried SDP (registrar-only calls, etc).
    /// The transcoding router (5.6b) compares the two legs' entries
    /// to decide whether a `CallTranscoder` is needed; empty /
    /// equal = passthrough. `#[serde(default)]` so pre-5.6 HA
    /// snapshots deserialize cleanly — an older replica's dialogs
    /// simply come back with an empty map, which is the passthrough
    /// default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_leg_codec: BTreeMap<LegId, NegotiatedCodec>,
    /// ICE parameters. Populated when the
    /// dialog uses native ICE connectivity checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ice: Option<crate::sdp::IceParams>,
    /// Remote target: the `Contact` URI of the request that created
    /// the dialog (RFC 3261 §12.1.1). Request-URI of every request
    /// the engine originates inside the dialog. `None` when the
    /// INVITE carried no usable `Contact`; senders then fall back to
    /// `sip:<peer_signal>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_target: Option<String>,
    /// Route set: the `Record-Route` URIs of the dialog-creating
    /// request in header order — the UAS keeps them as received
    /// (§12.1.1; only a UAC reverses the list it learns from a
    /// response). Emitted as `Route` headers on engine-originated
    /// in-dialog requests (§12.2.1.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub route_set: Vec<String>,
    /// URI of the local party — the INVITE's `To` URI without
    /// display name or parameters. Becomes `From` on requests the
    /// engine sends in this dialog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_uri: Option<String>,
    /// URI of the remote party — the INVITE's `From` URI. Becomes
    /// `To` on requests the engine sends in this dialog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_uri: Option<String>,
    /// `CSeq` number of the last request the engine sent inside this
    /// dialog (§12.2.1.1 local sequence number). `0` = none yet;
    /// the next request uses `local_cseq + 1`.
    #[serde(default)]
    pub local_cseq: u32,
    /// Highest `CSeq` number received from the remote party (§12.2.2
    /// remote sequence number). In-dialog requests with a lower
    /// number are out of order and rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_cseq: Option<u32>,
    /// Transport token of the top `Via` on the dialog-creating
    /// request (`UDP`, `TCP`, `TLS`, …). Engine-originated requests
    /// in the dialog advertise the same transport in their own
    /// `Via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// `Via` branch of the most recent INVITE transaction on this
    /// dialog (initial INVITE or re-INVITE). A 2xx terminates the
    /// server transaction immediately (§17.2.1), so this is what
    /// tells a retransmitted INVITE apart from a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_invite_branch: Option<String>,
    /// Local RTP address of [`Self::media`], kept so a re-INVITE can
    /// re-run offer/answer against the endpoint the dialog already
    /// owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_media: Option<SocketAddr>,
    /// Session interval agreed under RFC 4028, in seconds. `None`
    /// when the dialog runs without a session timer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_expires_secs: Option<u32>,
}

/// Dialog state mutation for replication.
///
/// Carried between HA nodes: the primary streams these to its
/// secondary, and in Raft mode each one is a log entry.
///
/// Adjacently tagged (`{"type": ..., "data": ...}`) rather than
/// internally tagged, because `Delete` wraps a tuple and serde cannot
/// fold a sequence into a tagged object — an internally tagged
/// representation fails at runtime on every delete.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DialogDelta {
    /// Create or update a dialog record.
    Upsert(Box<DialogRecord>),
    /// Remove a dialog record.
    Delete(DialogKey),
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
    /// An internally tagged enum cannot wrap a tuple, so `Delete`
    /// used to fail to serialize at runtime — which meant no dialog
    /// deletion ever replicated.
    #[test]
    fn dialog_delta_round_trips_both_variants() {
        let key: DialogKey = ("call-1".into(), "lt".into(), "rt".into());
        let delete = DialogDelta::Delete(key.clone());
        let encoded = serde_json::to_string(&delete).expect("delete must serialize");
        match serde_json::from_str::<DialogDelta>(&encoded).expect("decode") {
            DialogDelta::Delete(k) => assert_eq!(k, key),
            DialogDelta::Upsert(_) => panic!("variant changed across the wire"),
        }

        let record: DialogRecord = serde_json::from_value(serde_json::json!({
            "call_id": "call-1",
            "local_tag": "lt",
            "remote_tag": "rt",
            "state": "confirmed",
            "peer_signal": "127.0.0.1:5060",
            "rendezvous": null,
            "media": null,
            "remote_media": null,
        }))
        .expect("record");
        let upsert = DialogDelta::Upsert(Box::new(record));
        let encoded = serde_json::to_string(&upsert).expect("upsert must serialize");
        match serde_json::from_str::<DialogDelta>(&encoded).expect("decode") {
            DialogDelta::Upsert(r) => assert_eq!(r.key(), key),
            DialogDelta::Delete(_) => panic!("variant changed across the wire"),
        }
    }

    use super::*;

    /// Compile-time proof: `DialogRecord` implements the traits the HA
    /// guardrail requires.
    #[test]
    fn record_is_serialize_and_clone() {
        fn assert_bounds<T: Clone + Serialize + for<'de> Deserialize<'de>>() {}
        assert_bounds::<DialogRecord>();
    }

    #[test]
    fn negotiated_codec_round_trips_through_lowercase_token() {
        for (token, expected) in &[
            ("PCMU", NegotiatedCodec::Pcmu),
            ("pcmu", NegotiatedCodec::Pcmu),
            ("PCMA", NegotiatedCodec::Pcma),
            ("G722", NegotiatedCodec::G722),
            ("opus", NegotiatedCodec::Opus),
            ("H264", NegotiatedCodec::H264),
            ("VP8", NegotiatedCodec::Vp8),
            ("VP9", NegotiatedCodec::Vp9),
            ("t38", NegotiatedCodec::T38),
        ] {
            assert_eq!(NegotiatedCodec::parse(token), *expected);
            assert_eq!(expected.as_str(), token.to_ascii_lowercase());
        }
    }

    #[test]
    fn negotiated_codec_unknown_falls_through() {
        let g729 = NegotiatedCodec::parse("G729");
        assert!(matches!(g729, NegotiatedCodec::Other(ref s) if s == "g729"));
        assert_eq!(g729.as_str(), "g729");
    }

    #[test]
    fn media_kind_tag_round_trips() {
        for (token, expected) in &[
            ("audio", MediaKindTag::Audio),
            ("video", MediaKindTag::Video),
            ("image", MediaKindTag::Image),
            ("application", MediaKindTag::Application),
        ] {
            assert_eq!(MediaKindTag::parse(token), *expected);
            assert_eq!(expected.as_str(), *token);
        }
    }

    #[test]
    fn pre_slice_5_6_dialog_json_deserializes_with_empty_per_leg_codec() {
        // Simulate an HA snapshot taken before : no
        // `per_leg_codec` field. Deserialization must succeed with
        // `#[serde(default)]` producing an empty map.
        let json = serde_json::json!({
            "call_id": "c@x",
            "local_tag": "lt",
            "remote_tag": "rt",
            "state": "early",
            "peer_signal": "127.0.0.1:5060",
            "rendezvous": null,
            "media": null,
            "remote_media": null,
        });
        let rec: DialogRecord = serde_json::from_value(json).unwrap();
        assert!(rec.per_leg_codec.is_empty());
        assert!(rec.remote_target.is_none());
        assert!(rec.route_set.is_empty());
        assert_eq!(rec.local_cseq, 0);
        assert!(rec.remote_cseq.is_none());
        assert!(rec.transport.is_none());
        assert!(rec.last_invite_branch.is_none());
        assert!(rec.session_expires_secs.is_none());
    }

    #[test]
    fn dialog_routing_fields_round_trip() {
        let rec = DialogRecord {
            call_id: "c@x".into(),
            local_tag: "lt".into(),
            remote_tag: "rt".into(),
            state: DialogState::Confirmed,
            peer_signal: "127.0.0.1:5060".parse().unwrap(),
            rendezvous: None,
            media: None,
            remote_media: None,
            pending_2xx: None,
            per_leg_codec: BTreeMap::new(),
            ice: None,
            remote_target: Some("sip:bob@10.0.0.2:5060".into()),
            route_set: vec!["<sip:p1.example;lr>".into(), "<sip:p2.example;lr>".into()],
            local_uri: Some("sip:alice@example".into()),
            remote_uri: Some("sip:bob@example".into()),
            local_cseq: 3,
            remote_cseq: Some(7),
            transport: Some("TCP".into()),
            last_invite_branch: Some("z9hG4bK-1".into()),
            local_media: Some("10.0.0.1:40000".parse().unwrap()),
            session_expires_secs: Some(1800),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: DialogRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.remote_target, rec.remote_target);
        assert_eq!(back.route_set, rec.route_set);
        assert_eq!(back.local_uri, rec.local_uri);
        assert_eq!(back.remote_uri, rec.remote_uri);
        assert_eq!(back.local_cseq, 3);
        assert_eq!(back.remote_cseq, Some(7));
        assert_eq!(back.transport.as_deref(), Some("TCP"));
        assert_eq!(back.last_invite_branch.as_deref(), Some("z9hG4bK-1"));
        assert_eq!(back.local_media, rec.local_media);
        assert_eq!(back.session_expires_secs, Some(1800));
    }

    #[test]
    fn dialog_record_with_per_leg_codec_round_trips() {
        let mut codecs = BTreeMap::new();
        codecs.insert(LegId(0), NegotiatedCodec::Opus);
        codecs.insert(LegId(1), NegotiatedCodec::Pcmu);
        let rec = DialogRecord {
            call_id: "c@x".into(),
            local_tag: "lt".into(),
            remote_tag: "rt".into(),
            state: DialogState::Early,
            peer_signal: "127.0.0.1:5060".parse().unwrap(),
            rendezvous: None,
            media: None,
            remote_media: None,
            pending_2xx: None,
            per_leg_codec: codecs.clone(),
            ice: None,
            remote_target: None,
            route_set: Vec::new(),
            local_uri: None,
            remote_uri: None,
            local_cseq: 0,
            remote_cseq: None,
            transport: None,
            last_invite_branch: None,
            local_media: None,
            session_expires_secs: None,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: DialogRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.per_leg_codec, codecs);
    }
}
