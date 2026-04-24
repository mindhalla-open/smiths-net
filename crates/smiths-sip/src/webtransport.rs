//! WebTransport signaling scaffold (slice 5.7 / P19).
//!
//! Browsers that speak WebTransport can open a signaling session
//! against the engine over HTTP/3 + QUIC datagrams without
//! implementing any SIP at all. This module lands the **protocol
//! shape** + **listener trait** + **signaling event wiring** so that
//! a later slice can drop in the QUIC runtime (`quinn` +
//! `h3-webtransport`) and have a place to plug in.
//!
//! ## Shared with WebRTC-native (slice 5.10)
//!
//! The same [`WtSignal`] JSON frame shape is reused by the
//! WebRTC-native signaling adapter (slice 5.10 scaffold) — that
//! adapter carries the frames over plain WebSocket, this one
//! over WebTransport. Sharing the wire format means a browser
//! demo built against one transport's listener lights up the
//! other by changing a URL. The scaffold status is identical:
//! the types + listener trait + config surface exist; the two
//! runtimes (QUIC + WebSocket) are focused follow-ons.
//!
//! ## What ships today (v0.55.0)
//!
//! - [`WtSignal`] — the per-frame JSON message schema browsers and
//!   the engine exchange on a bidirectional stream. Typed Rust
//!   struct with `serde` round-trip; the exact wire shape the
//!   browser demo under `examples/browser-webtransport/` encodes.
//! - [`WebTransportListener`] — trait the future runtime
//!   implements. Today the only impl is
//!   [`NullWebTransportListener`], which logs a clear
//!   "scaffold only" error on `bind` so an operator flipping the
//!   feature on discovers the deferral immediately.
//! - [`WebTransportSessionId`] — opaque per-session handle.
//!   Conference / dialog tracking joins against it.
//!
//! ## What's deferred to a follow-on slice
//!
//! - **QUIC runtime.** Picking between `quinn` + `h3-webtransport`
//!   vs `wtransport` vs a hand-rolled h3 CONNECT path is a
//!   multi-day exploration. Out of scope for the scaffold.
//! - **Integration with the UAS.** Once the listener runs,
//!   sessions need to install dialogs in the `DialogRecord` map so
//!   `list_calls` / `end_call` / MCP observability work
//!   identically to SIP. Piggy-backs on the slice 5.6 `DialogSessions`
//!   substrate; the wiring is small but sits on the future-runtime
//!   side of the seam.
//! - **TLS / cert story.** Browsers accept WebTransport only over
//!   HTTPS with a valid cert (or `serverCertificateHashes` on
//!   Chromium, which is a narrower browser slice). The `[webtransport]
//!   cert_path` / `key_path` fields are already in the config surface;
//!   the runtime enforces them at bind.
//!
//! The scaffold is explicitly a **scaffold** — flipping the feature
//! on logs a loud warning at boot. See
//! `docs/deployment/webtransport.md` for the operator-facing story.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

/// Stable identifier for one WebTransport signaling session.
///
/// Opaque u64 minted by the listener at `accept()` time. Distinct
/// from `DialogKey` because one WebTransport session can carry
/// multiple dialog lifecycles (re-INVITEs, multiple logical calls
/// over one signaling channel); the future UAS wiring maps
/// `WebTransportSessionId → [DialogKey]` when dialogs open against
/// this session.
#[derive(
    Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct WebTransportSessionId(pub u64);

impl std::fmt::Display for WebTransportSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wt-{}", self.0)
    }
}

/// Monotonic counter backing [`WebTransportSessionId`] allocation.
/// Exposed so the future runtime can mint IDs without reaching into
/// a singleton.
#[derive(Debug, Default)]
pub struct SessionIdAllocator {
    next: AtomicU64,
}

impl SessionIdAllocator {
    /// Mint a fresh id.
    #[must_use]
    pub fn fresh(&self) -> WebTransportSessionId {
        WebTransportSessionId(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// Discriminator used by [`WtSignal::kind`]. Matches the `type`
/// field on the wire so the browser demo's JS can `switch` on it.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WtSignalKind {
    /// Client opens the session. Engine replies with `session-ack`
    /// carrying a fresh session id.
    SessionInit,
    /// Engine → client acknowledgement. Payload: `{"session_id": u64}`.
    SessionAck,
    /// Client → engine SDP offer (WebRTC).
    Offer,
    /// Engine → client SDP answer.
    Answer,
    /// Either side streams an ICE candidate as it's gathered.
    IceCandidate,
    /// Either side signals "end of candidates" for trickle ICE.
    IceEnd,
    /// Either side hangs up. Engine sends this to force-close;
    /// client sends it to leave cleanly.
    Bye,
    /// Engine → client error surface. Payload carries `reason`
    /// + optional `code`. Terminal for the session.
    Error,
    /// Client → engine data-channel echo test. Engine mirrors the
    /// payload back verbatim. Lets the demo prove the data path
    /// round-trips before the audio path wires through.
    Echo,
}

impl WtSignalKind {
    /// Kebab-case token used on the wire and in
    /// [`smiths_core::event::SipEvent::WebTransportSignal::kind`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionInit => "session-init",
            Self::SessionAck => "session-ack",
            Self::Offer => "offer",
            Self::Answer => "answer",
            Self::IceCandidate => "ice-candidate",
            Self::IceEnd => "ice-end",
            Self::Bye => "bye",
            Self::Error => "error",
            Self::Echo => "echo",
        }
    }
}

/// One frame exchanged over the bidirectional signaling stream.
///
/// Wire format (JSON, one frame per WebTransport *message*, UTF-8):
///
/// ```json
/// { "type": "offer", "session_id": 17, "sdp": "v=0\r\n..." }
/// { "type": "ice-candidate", "session_id": 17, "candidate": "candidate:1 1 UDP ..." }
/// { "type": "bye", "session_id": 17 }
/// ```
///
/// `session_id` is `None` on `SessionInit` (the engine mints one in
/// `SessionAck`) and required for every subsequent frame. Unknown
/// fields round-trip verbatim through `#[serde(flatten)] extras` so
/// a future protocol extension doesn't force a coordinated
/// engine+browser upgrade.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WtSignal {
    /// Client → engine: open the session.
    SessionInit {
        /// Optional client-supplied tag for correlation in logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    /// Engine → client: session opened, here's your id.
    SessionAck {
        /// Minted by the listener.
        session_id: WebTransportSessionId,
        /// Protocol version the engine accepted. Today 1.
        protocol_version: u8,
    },
    /// Client → engine: SDP offer.
    Offer {
        /// Session this offer applies to.
        session_id: WebTransportSessionId,
        /// SDP body (RFC 8866, same as over SIP).
        sdp: String,
    },
    /// Engine → client: SDP answer.
    Answer {
        /// Session this answer applies to.
        session_id: WebTransportSessionId,
        /// SDP body the engine produced via the standard
        /// `SdpNegotiator` path — same answers a SIP peer gets.
        sdp: String,
    },
    /// Trickle ICE candidate.
    IceCandidate {
        /// Session id.
        session_id: WebTransportSessionId,
        /// `a=candidate:` value (without the `a=` prefix).
        candidate: String,
        /// Media line index the candidate applies to.
        sdp_m_line_index: u16,
    },
    /// Trickle ICE "no more candidates".
    IceEnd {
        /// Session id.
        session_id: WebTransportSessionId,
    },
    /// End-of-session signal from either side.
    Bye {
        /// Session id.
        session_id: WebTransportSessionId,
        /// Optional reason phrase.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Engine → client error surface.
    Error {
        /// Session id, if known — `None` when the error fires
        /// before session-init completes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<WebTransportSessionId>,
        /// Short error code (`"codec-mismatch"`, `"auth-failed"`,
        /// `"unsupported-profile"`, …).
        code: String,
        /// Human-readable reason.
        reason: String,
    },
    /// Client ↔ engine data-channel echo. Engine mirrors the payload
    /// verbatim. Shipped so the browser demo can prove the data
    /// path round-trips before the audio path wires through.
    Echo {
        /// Session id.
        session_id: WebTransportSessionId,
        /// Arbitrary bytes the client wants back (base64-encoded
        /// for JSON readability; runtime-side round-trip is
        /// byte-identity).
        payload_b64: String,
    },
}

impl WtSignal {
    /// Discriminator kind as a typed value.
    #[must_use]
    pub fn kind(&self) -> WtSignalKind {
        match self {
            Self::SessionInit { .. } => WtSignalKind::SessionInit,
            Self::SessionAck { .. } => WtSignalKind::SessionAck,
            Self::Offer { .. } => WtSignalKind::Offer,
            Self::Answer { .. } => WtSignalKind::Answer,
            Self::IceCandidate { .. } => WtSignalKind::IceCandidate,
            Self::IceEnd { .. } => WtSignalKind::IceEnd,
            Self::Bye { .. } => WtSignalKind::Bye,
            Self::Error { .. } => WtSignalKind::Error,
            Self::Echo { .. } => WtSignalKind::Echo,
        }
    }

    /// Session this frame belongs to, if already minted. `None`
    /// before `SessionAck` completes.
    #[must_use]
    pub fn session_id(&self) -> Option<WebTransportSessionId> {
        match self {
            Self::SessionInit { .. } => None,
            Self::SessionAck { session_id, .. }
            | Self::Offer { session_id, .. }
            | Self::Answer { session_id, .. }
            | Self::IceCandidate { session_id, .. }
            | Self::IceEnd { session_id, .. }
            | Self::Bye { session_id, .. }
            | Self::Echo { session_id, .. } => Some(*session_id),
            Self::Error { session_id, .. } => *session_id,
        }
    }

    /// Encode to the canonical wire form (JSON, UTF-8).
    ///
    /// # Errors
    /// [`WtSignalError::Encode`] on a serialization failure. In
    /// practice this only fires when a `payload_b64` isn't valid
    /// UTF-8 — which we ensure by construction, since it's a
    /// `String`.
    pub fn encode(&self) -> Result<Vec<u8>, WtSignalError> {
        serde_json::to_vec(self).map_err(|e| WtSignalError::Encode(e.to_string()))
    }

    /// Decode from wire bytes.
    ///
    /// # Errors
    /// [`WtSignalError::Decode`] on a malformed frame. The runtime
    /// should respond with a `WtSignal::Error` frame; the remote
    /// either replays a fixed frame or drops the session.
    pub fn decode(bytes: &[u8]) -> Result<Self, WtSignalError> {
        serde_json::from_slice(bytes).map_err(|e| WtSignalError::Decode(e.to_string()))
    }
}

/// Errors raised by [`WtSignal::encode`] / [`WtSignal::decode`].
#[derive(Debug, Error)]
pub enum WtSignalError {
    /// Serialization failed (serde/json error). Rare — any
    /// construct-at-runtime path should be encode-safe.
    #[error("webtransport signal encode: {0}")]
    Encode(String),
    /// Malformed wire bytes.
    #[error("webtransport signal decode: {0}")]
    Decode(String),
}

/// Listener for WebTransport signaling sessions.
///
/// The trait is deliberately narrow today: `bind` to start
/// accepting, `shutdown` to stop. A future slice adds session
/// enumeration, per-session send/recv streams, and datagram
/// support — all on this trait. Keeping it narrow for now lets the
/// runtime impl land without coordinating breaking changes with
/// callers.
#[async_trait]
pub trait WebTransportListener: Send + Sync {
    /// Start listening on `addr`. Returns once the listener is
    /// ready to accept.
    ///
    /// # Errors
    /// [`WtListenError`] — see variant docs.
    async fn bind(&self, addr: SocketAddr) -> Result<(), WtListenError>;

    /// Stop accepting new sessions and drain in-flight ones.
    async fn shutdown(&self);
}

/// Errors surfaced by [`WebTransportListener::bind`].
#[derive(Debug, Error)]
pub enum WtListenError {
    /// The runtime listener isn't yet wired — this is the only
    /// error today's scaffold listener returns.
    #[error("webtransport runtime not yet wired (scaffold only)")]
    ScaffoldOnly,
    /// Underlying I/O failure.
    #[error("webtransport bind I/O: {0}")]
    Io(String),
    /// TLS / cert configuration rejected.
    #[error("webtransport TLS config: {0}")]
    Tls(String),
}

/// Scaffold listener that refuses `bind` with a clear
/// `ScaffoldOnly` error.
///
/// Operators who flip `[webtransport] enabled = true` with a binary
/// built without the runtime deps get this listener; the engine
/// refuses to start and the log line points at the follow-on slice.
/// When the runtime lands, a real `QuinnWebTransportListener`
/// replaces this in the engine's listener registry.
#[derive(Debug, Default)]
pub struct NullWebTransportListener;

impl NullWebTransportListener {
    /// Build a fresh null listener.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WebTransportListener for NullWebTransportListener {
    async fn bind(&self, addr: SocketAddr) -> Result<(), WtListenError> {
        warn!(%addr, "webtransport: scaffold listener refusing bind; runtime lands with a later slice");
        Err(WtListenError::ScaffoldOnly)
    }

    async fn shutdown(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_init_round_trips_through_json() {
        let s = WtSignal::SessionInit {
            tag: Some("demo-browser".into()),
        };
        let bytes = s.encode().unwrap();
        let back = WtSignal::decode(&bytes).unwrap();
        assert_eq!(s, back);
        // And it matches the documented wire form.
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["type"], "session-init");
        assert_eq!(json["tag"], "demo-browser");
    }

    #[test]
    fn session_init_without_tag_omits_the_field() {
        let s = WtSignal::SessionInit { tag: None };
        let bytes = s.encode().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json.get("tag").is_none(), "tag omitted when None");
    }

    #[test]
    fn offer_and_answer_carry_session_id_and_sdp() {
        let id = WebTransportSessionId(42);
        let offer = WtSignal::Offer {
            session_id: id,
            sdp: "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n".into(),
        };
        let answer = WtSignal::Answer {
            session_id: id,
            sdp: "v=0\r\no=- 2 2 IN IP4 127.0.0.1\r\n".into(),
        };
        let offer_back = WtSignal::decode(&offer.encode().unwrap()).unwrap();
        let answer_back = WtSignal::decode(&answer.encode().unwrap()).unwrap();
        assert_eq!(offer, offer_back);
        assert_eq!(answer, answer_back);
        assert_eq!(offer.session_id(), Some(id));
        assert_eq!(answer.session_id(), Some(id));
        assert_eq!(offer.kind(), WtSignalKind::Offer);
        assert_eq!(answer.kind(), WtSignalKind::Answer);
    }

    #[test]
    fn ice_candidate_shape_matches_browser_event() {
        // Browser `RTCPeerConnection.onicecandidate` yields
        // `{candidate, sdpMLineIndex}`. Our wire shape preserves
        // those names (kebab-cased).
        let s = WtSignal::IceCandidate {
            session_id: WebTransportSessionId(1),
            candidate: "candidate:1 1 UDP 2122252543 192.0.2.1 54321 typ host".into(),
            sdp_m_line_index: 0,
        };
        let json: serde_json::Value = serde_json::from_slice(&s.encode().unwrap()).unwrap();
        assert_eq!(json["type"], "ice-candidate");
        assert!(json["candidate"].is_string());
        assert_eq!(json["sdp_m_line_index"], 0);
    }

    #[test]
    fn bye_with_reason_round_trips() {
        let s = WtSignal::Bye {
            session_id: WebTransportSessionId(7),
            reason: Some("peer hung up".into()),
        };
        let back = WtSignal::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn error_frame_can_precede_session_ack() {
        // Session-init failures fire before the id is minted, so
        // the session_id field is optional on Error.
        let s = WtSignal::Error {
            session_id: None,
            code: "auth-failed".into(),
            reason: "token rejected".into(),
        };
        let back = WtSignal::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.session_id(), None);
    }

    #[test]
    fn echo_carries_base64_payload() {
        let s = WtSignal::Echo {
            session_id: WebTransportSessionId(3),
            payload_b64: "aGVsbG8=".into(), // "hello"
        };
        let back = WtSignal::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.kind(), WtSignalKind::Echo);
    }

    #[test]
    fn unknown_type_token_fails_decode_cleanly() {
        let json = r#"{"type":"no-such-frame","session_id":1}"#;
        let err = WtSignal::decode(json.as_bytes()).unwrap_err();
        assert!(matches!(err, WtSignalError::Decode(_)));
    }

    #[test]
    fn session_id_allocator_issues_monotonic_ids() {
        let alloc = SessionIdAllocator::default();
        let a = alloc.fresh();
        let b = alloc.fresh();
        let c = alloc.fresh();
        assert_eq!(a.0, 0);
        assert_eq!(b.0, 1);
        assert_eq!(c.0, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn null_listener_refuses_bind_with_scaffold_error() {
        let l = NullWebTransportListener::new();
        let err = l.bind("127.0.0.1:0".parse().unwrap()).await.unwrap_err();
        assert!(matches!(err, WtListenError::ScaffoldOnly));
        // Shutdown is a no-op but must complete quickly.
        l.shutdown().await;
    }
}
