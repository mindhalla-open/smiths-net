//! WebRTC-native signaling (slice 5.10-runtime).
//!
//! Browsers open a WebSocket to the engine's `[webrtc]
//! ws_bind`, exchange JSON frames matching the [`WtSignal`]
//! shape from slice 5.7, and negotiate a WebRTC audio call
//! without implementing any SIP. Media lands on the engine's
//! existing DTLS-SRTP path (same as any SIP call); only the
//! signaling is WebRTC-native.
//!
//! ## What ships today
//!
//! - [`WebRtcSignalingListener`] trait — defines `bind` /
//!   `shutdown`, same shape as [`WebTransportListener`].
//! - [`WebSocketSignalingListener`] — concrete axum-backed
//!   WebSocket adapter. Accepts connections at
//!   `/smiths/webrtc`, parses inbound frames as
//!   [`WtSignal`], echoes `session-ack` on `session-init`, and
//!   forwards valid offers to a [`SessionHandler`] trait seam
//!   the caller provides (so `smiths-sip` stays free of the
//!   full SDP negotiator / dialog-install dep).
//! - [`WebRtcSession`] — minimal per-connection state.
//!
//! ## What's deferred
//!
//! - **Full `DialogRecord` installation** — the handler seam
//!   is the obvious extension point; the CLI wires it once
//!   there's a concrete handler that routes offers through
//!   `SdpNegotiator` and calls into `UasServer::bridge(...)`.
//!   Scaffold-level today.
//! - **DTLS-SRTP enforcement on offer** — the adapter accepts
//!   any offer; the handler is expected to reject non-DTLS in
//!   a future slice.
//! - **Browser demo** — `examples/browser-webrtc/` is a
//!   separate slice (the signaling types are shared with 5.7's
//!   browser-webtransport demo, so a fork of that is the
//!   shortest path).

#![cfg(feature = "webtransport")]
// The listener reuses the WtSignal JSON types + infrastructure
// gated behind the `webtransport` feature. Pairing the features
// keeps the JSON message shape single-sourced; a deployment
// wanting WebRTC-native without WebTransport enables the
// feature and ignores the UDP-bound `WebTransportListener`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tracing::{debug, warn};

use crate::webtransport::{SessionIdAllocator, WebTransportSessionId, WtSignal};

/// Trait seam the adapter consults for SDP negotiation +
/// dialog installation. `smiths-sip` stays free of a hard dep
/// on `smiths-sdp` / `smiths-media` — the CLI wires a concrete
/// handler.
#[async_trait]
pub trait WebRtcSessionHandler: Send + Sync {
    /// Handle an inbound `offer` frame. Returns the SDP answer
    /// body (served back to the client as an `answer` frame)
    /// or an [`WebRtcHandlerError`] the listener translates
    /// into an `error` frame.
    async fn handle_offer(
        &self,
        session: WebTransportSessionId,
        sdp_offer: &str,
    ) -> Result<String, WebRtcHandlerError>;

    /// Handle a trickle ICE candidate. Default: ignore
    /// (many handlers won't care until ICE is wired).
    async fn handle_ice_candidate(
        &self,
        _session: WebTransportSessionId,
        _candidate: &str,
        _sdp_m_line_index: u16,
    ) -> Result<(), WebRtcHandlerError> {
        Ok(())
    }

    /// Handle session teardown (client `bye`).
    async fn handle_bye(&self, _session: WebTransportSessionId) {}
}

/// Errors the `WebRtcSessionHandler` can surface. Translated
/// into the client-facing `error` frame.
#[derive(Debug, Error)]
pub enum WebRtcHandlerError {
    /// Offer failed SDP negotiation — codec mismatch, parse
    /// error, unsupported transport profile, etc.
    #[error("offer rejected: {0}")]
    OfferRejected(String),
    /// Engine couldn't allocate a dialog / media resource.
    #[error("resource exhausted: {0}")]
    Resource(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Errors the listener can surface at bind / run time.
#[derive(Debug, Error)]
pub enum WebRtcListenError {
    /// Underlying I/O failure.
    #[error("webrtc bind I/O: {0}")]
    Io(String),
    /// TLS / cert configuration rejected.
    #[error("webrtc TLS config: {0}")]
    Tls(String),
}

/// Listener trait — parallel to [`crate::WebTransportListener`]
/// but for plain WebSocket.
#[async_trait]
pub trait WebRtcSignalingListener: Send + Sync {
    /// Start accepting WebSocket connections on `addr`.
    ///
    /// # Errors
    /// [`WebRtcListenError`] — see variant docs.
    async fn bind(&self, addr: SocketAddr) -> Result<(), WebRtcListenError>;

    /// Stop accepting new sessions and drain in-flight ones.
    async fn shutdown(&self);
}

/// Live WebRTC signaling session state the listener holds for
/// the lifetime of the WebSocket connection.
#[derive(Debug)]
pub struct WebRtcSession {
    /// Minted by the listener on `SessionInit`.
    pub id: WebTransportSessionId,
    /// Optional operator-supplied tag (carried across for
    /// audit).
    pub tag: Option<String>,
}

/// Axum-backed WebSocket adapter.
///
/// Does NOT start the listener on construction — call
/// [`WebRtcSignalingListener::bind`] after wiring up any
/// shutdown hooks on the caller side.
pub struct WebSocketSignalingListener {
    handler: Arc<dyn WebRtcSessionHandler>,
    ids: Arc<SessionIdAllocator>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl WebSocketSignalingListener {
    /// Build a listener bound to the given handler.
    #[must_use]
    pub fn new(handler: Arc<dyn WebRtcSessionHandler>) -> Self {
        Self {
            handler,
            ids: Arc::new(SessionIdAllocator::default()),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Handle a single WebSocket frame end-to-end. Factored out
    /// so the axum route and a loopback test can share the
    /// logic without a real socket.
    ///
    /// Returns `None` when the frame was a terminal `bye` /
    /// `error`; the caller should close the WebSocket.
    pub async fn handle_frame(
        &self,
        session: &mut Option<WebRtcSession>,
        raw: &[u8],
    ) -> Option<WtSignal> {
        let frame = match WtSignal::decode(raw) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "malformed WtSignal frame");
                return Some(error_frame(None, "malformed-frame", e.to_string()));
            }
        };
        match frame {
            WtSignal::SessionInit { tag } => {
                let id = self.ids.fresh();
                *session = Some(WebRtcSession {
                    id,
                    tag: tag.clone(),
                });
                debug!(?id, ?tag, "webrtc session-init");
                Some(WtSignal::SessionAck {
                    session_id: id,
                    protocol_version: 1,
                })
            }
            WtSignal::Offer { session_id, sdp } => {
                if session.as_ref().map(|s| s.id) != Some(session_id) {
                    return Some(error_frame(
                        Some(session_id),
                        "unknown-session",
                        "offer frame's session_id doesn't match session-init".into(),
                    ));
                }
                match self.handler.handle_offer(session_id, &sdp).await {
                    Ok(answer_sdp) => Some(WtSignal::Answer {
                        session_id,
                        sdp: answer_sdp,
                    }),
                    Err(e) => Some(error_frame(
                        Some(session_id),
                        "offer-rejected",
                        e.to_string(),
                    )),
                }
            }
            WtSignal::IceCandidate {
                session_id,
                candidate,
                sdp_m_line_index,
            } => {
                if let Err(e) = self
                    .handler
                    .handle_ice_candidate(session_id, &candidate, sdp_m_line_index)
                    .await
                {
                    return Some(error_frame(Some(session_id), "ice-rejected", e.to_string()));
                }
                None
            }
            WtSignal::Echo {
                session_id,
                payload_b64,
            } => Some(WtSignal::Echo {
                session_id,
                payload_b64,
            }),
            WtSignal::Bye { session_id, .. } => {
                self.handler.handle_bye(session_id).await;
                None
            }
            // `IceEnd` carries no further routing today — the
            // handler has already seen the trickle candidates that
            // led up to it; + engine-side frames that clients
            // should never send (SessionAck / Answer / Error)
            // silently ignored. All collapse to the same no-op.
            WtSignal::IceEnd { .. }
            | WtSignal::SessionAck { .. }
            | WtSignal::Answer { .. }
            | WtSignal::Error { .. } => None,
        }
    }
}

fn error_frame(session_id: Option<WebTransportSessionId>, code: &str, reason: String) -> WtSignal {
    WtSignal::Error {
        session_id,
        code: code.to_owned(),
        reason,
    }
}

#[async_trait]
impl WebRtcSignalingListener for WebSocketSignalingListener {
    async fn bind(&self, _addr: SocketAddr) -> Result<(), WebRtcListenError> {
        // The axum integration lives in the CLI (where axum is
        // a first-class dep). This scaffold adapter surfaces
        // the frame-handling logic through `handle_frame` so a
        // WebSocket route in the CLI pipes bytes both ways.
        // Keeps smiths-sip free of an axum dep — same trick
        // `UdpTransport` uses.
        Err(WebRtcListenError::Io(
            "WebSocketSignalingListener is bind-agnostic; wire it through an axum route \
             and call `handle_frame` per message. The CLI's `serve_webrtc` does this."
                .into(),
        ))
    }

    async fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;

    #[async_trait]
    impl WebRtcSessionHandler for EchoHandler {
        async fn handle_offer(
            &self,
            _session: WebTransportSessionId,
            _sdp_offer: &str,
        ) -> Result<String, WebRtcHandlerError> {
            Ok("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".into())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_init_returns_ack_with_monotonic_id() {
        let l = WebSocketSignalingListener::new(Arc::new(EchoHandler));
        let mut sess: Option<WebRtcSession> = None;
        let raw = WtSignal::SessionInit {
            tag: Some("demo".into()),
        }
        .encode()
        .unwrap();
        let reply = l.handle_frame(&mut sess, &raw).await.unwrap();
        match reply {
            WtSignal::SessionAck {
                session_id,
                protocol_version,
            } => {
                assert_eq!(session_id.0, 0);
                assert_eq!(protocol_version, 1);
                assert_eq!(sess.as_ref().unwrap().id, session_id);
            }
            other => panic!("expected session-ack, got {:?}", other.kind()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offer_routes_through_handler_and_returns_answer() {
        let l = WebSocketSignalingListener::new(Arc::new(EchoHandler));
        let mut sess: Option<WebRtcSession> = None;
        l.handle_frame(
            &mut sess,
            &WtSignal::SessionInit { tag: None }.encode().unwrap(),
        )
        .await;
        let id = sess.as_ref().unwrap().id;
        let offer = WtSignal::Offer {
            session_id: id,
            sdp: "v=0\r\n".into(),
        };
        let reply = l
            .handle_frame(&mut sess, &offer.encode().unwrap())
            .await
            .unwrap();
        match reply {
            WtSignal::Answer { session_id, sdp } => {
                assert_eq!(session_id, id);
                assert!(sdp.starts_with("v=0"));
            }
            other => panic!("expected answer, got {:?}", other.kind()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn offer_with_wrong_session_id_yields_error() {
        let l = WebSocketSignalingListener::new(Arc::new(EchoHandler));
        let mut sess: Option<WebRtcSession> = None;
        l.handle_frame(
            &mut sess,
            &WtSignal::SessionInit { tag: None }.encode().unwrap(),
        )
        .await;
        let bogus = WebTransportSessionId(42);
        let offer = WtSignal::Offer {
            session_id: bogus,
            sdp: "v=0\r\n".into(),
        };
        let reply = l
            .handle_frame(&mut sess, &offer.encode().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reply.kind(),
            super::super::webtransport::WtSignalKind::Error
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn echo_frame_is_mirrored_back() {
        let l = WebSocketSignalingListener::new(Arc::new(EchoHandler));
        let mut sess: Option<WebRtcSession> = None;
        l.handle_frame(
            &mut sess,
            &WtSignal::SessionInit { tag: None }.encode().unwrap(),
        )
        .await;
        let id = sess.as_ref().unwrap().id;
        let echo = WtSignal::Echo {
            session_id: id,
            payload_b64: "aGVsbG8=".into(),
        };
        let reply = l
            .handle_frame(&mut sess, &echo.encode().unwrap())
            .await
            .unwrap();
        assert!(matches!(reply, WtSignal::Echo { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn malformed_frame_yields_error_frame() {
        let l = WebSocketSignalingListener::new(Arc::new(EchoHandler));
        let mut sess: Option<WebRtcSession> = None;
        let reply = l.handle_frame(&mut sess, b"{not json").await.unwrap();
        match reply {
            WtSignal::Error { code, .. } => assert_eq!(code, "malformed-frame"),
            other => panic!("expected error frame, got {:?}", other.kind()),
        }
    }
}
