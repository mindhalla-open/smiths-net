//! WebRTC-native signaling runtime (slice 5.10-followup).
//!
//! Hosts the axum WebSocket route + the concrete
//! [`WebRtcSessionHandler`] impl that routes offers through the
//! shared `SdpNegotiator`. Lives in `smiths-cli` so the CLI owns
//! the axum dep and `smiths-sip` stays free of a `smiths-sdp`
//! dep (same shape as the UAS ⇄ negotiator split on the SIP side).
//!
//! ## What ships today
//!
//! - [`CliWebRtcHandler`] — wraps an `Arc<dyn SdpNegotiator>` +
//!   a local-IP/port allocator. `handle_offer` parses the SDP,
//!   negotiates an answer, and returns it as a string. Errors
//!   surface as [`WebRtcHandlerError::OfferRejected`] and the
//!   WebSocket listener translates those into `error` frames.
//! - [`serve_webrtc`] — axum router with `GET /smiths/webrtc`
//!   upgrading to a WebSocket, piping binary frames through the
//!   listener's `handle_frame`.
//!
//! ## Honest deferrals
//!
//! - **DTLS-SRTP termination.** Browsers only speak
//!   `UDP/TLS/RTP/SAVP`. The current negotiator recognizes that
//!   profile but replies `UnsupportedTransport` (see
//!   `smiths-sdp`'s negotiate module). Until the DTLS terminator
//!   lands, the handler surfaces that rejection to the browser
//!   as `offer-rejected: DTLS-SRTP not yet supported` — which
//!   is the correct signal for the operator, and the right
//!   behaviour for a scaffold that refuses to pretend it can
//!   carry media.
//! - **SIP ⇄ WebRTC bridging.** No rendezvous key wiring yet.
//!   A future slice installs a `DialogRecord` + calls
//!   `MediaFabric::bridge` when a WebRTC leg pairs with either a
//!   SIP INVITE or another WebRTC leg. See
//!   `.vscode/implementation-slices.md`.
//! - **TLS termination.** `serve_webrtc` binds plain HTTP/
//!   WebSocket today; production deployments front the engine
//!   with nginx/Caddy for `wss://`. See
//!   `docs/deployment/webrtc.md`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use smiths_core::{NegotiationOutcome, SdpNegotiator};
use smiths_sip::webrtc::{
    WebRtcHandlerError, WebRtcSession, WebRtcSessionHandler, WebSocketSignalingListener,
};
use smiths_sip::webtransport::WebTransportSessionId;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Concrete [`WebRtcSessionHandler`] used by the CLI. Holds the
/// shared SDP negotiator + a local-port allocator so every
/// session gets its own `m=audio` port.
///
/// Today this is a signaling-only handler — the negotiated
/// answer is returned to the browser but the call isn't wired
/// into `MediaFabric::bridge` yet (see the module doc for the
/// deferred bridge-install plan). The negotiator's rejection of
/// DTLS-SRTP offers surfaces as `offer-rejected` on the wire, so
/// operators running real browsers get a clear diagnostic, not
/// silent failure.
pub(crate) struct CliWebRtcHandler {
    negotiator: Arc<dyn SdpNegotiator>,
    local_ip: IpAddr,
    /// Next media port to hand out. Starts at `media_base_port`
    /// from config + 2 per session (one RTP + one RTCP slot);
    /// wraps at `media_base_port + media_port_range`.
    next_port: AtomicU16,
    port_base: u16,
    port_range: u16,
}

impl CliWebRtcHandler {
    /// Build a handler bound to the given negotiator + local IP
    /// (what the engine publishes in the answer's `c=` line).
    ///
    /// `port_base` / `port_range` carve a dedicated slice out of
    /// the engine's UDP range for WebRTC sessions so WebRTC and
    /// SIP allocations don't collide. When `port_range == 0` the
    /// handler falls back to `port_base` for every session,
    /// which is fine for dev / one-call scenarios.
    #[must_use]
    pub(crate) fn new(
        negotiator: Arc<dyn SdpNegotiator>,
        local_ip: IpAddr,
        port_base: u16,
        port_range: u16,
    ) -> Self {
        Self {
            negotiator,
            local_ip,
            next_port: AtomicU16::new(port_base),
            port_base,
            port_range,
        }
    }

    fn alloc_port(&self) -> u16 {
        if self.port_range == 0 {
            return self.port_base;
        }
        // Advance by 2 per session — leaves room for RTCP at port+1
        // once RTCP is wired. Wrap inside the configured range.
        let p = self.next_port.fetch_add(2, Ordering::Relaxed);
        self.port_base + ((p - self.port_base) % self.port_range)
    }
}

#[async_trait]
impl WebRtcSessionHandler for CliWebRtcHandler {
    async fn handle_offer(
        &self,
        session: WebTransportSessionId,
        sdp_offer: &str,
    ) -> Result<String, WebRtcHandlerError> {
        let port = self.alloc_port();
        debug!(?session, port, "webrtc: negotiating offer");
        let outcome = self
            .negotiator
            .negotiate_audio(sdp_offer, self.local_ip, port);
        match outcome {
            NegotiationOutcome::Accepted { answer_body, .. } => {
                debug!(
                    ?session,
                    port,
                    answer_len = answer_body.len(),
                    "webrtc: offer accepted"
                );
                Ok(answer_body)
            }
            NegotiationOutcome::Mismatch => {
                Err(WebRtcHandlerError::OfferRejected("no common codec".into()))
            }
            NegotiationOutcome::UnsupportedTransport { reason } => {
                Err(WebRtcHandlerError::OfferRejected(reason))
            }
            NegotiationOutcome::Malformed(e) => Err(WebRtcHandlerError::OfferRejected(format!(
                "malformed SDP: {e}"
            ))),
        }
    }

    async fn handle_bye(&self, session: WebTransportSessionId) {
        debug!(?session, "webrtc: session bye");
        // Bridge teardown lands with the bridge-install slice
        // (see module doc). Today the session held no live
        // `MediaFabric` resources, so there's nothing to release.
    }
}

/// Axum app state: the WebSocket route handler needs the
/// shared [`WebSocketSignalingListener`] to parse frames.
#[derive(Clone)]
struct WsState {
    listener: Arc<WebSocketSignalingListener>,
}

/// Serve the WebRTC signaling WebSocket on `bind` until
/// `cancel` fires. Returns when the axum server exits — either
/// because the cancellation token tripped or a bind error
/// surfaced.
///
/// # Errors
/// Returns an error when the TCP listener can't bind or the
/// axum server encounters a fatal I/O error.
pub(crate) async fn serve_webrtc(
    bind: SocketAddr,
    handler: Arc<CliWebRtcHandler>,
    cancel: CancellationToken,
) -> Result<()> {
    let listener_sip = Arc::new(WebSocketSignalingListener::new(handler));
    let state = WsState {
        listener: Arc::clone(&listener_sip),
    };
    let app: Router = Router::new()
        .route("/smiths/webrtc", get(ws_upgrade))
        .with_state(state);
    let tcp = TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding WebRTC WebSocket on {bind}"))?;
    info!(%bind, "WebRTC WebSocket adapter ready at /smiths/webrtc");
    axum::serve(tcp, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .context("WebRTC WebSocket server exited with error")?;
    Ok(())
}

async fn ws_upgrade(State(state): State<WsState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Drive a single WebSocket connection through the listener's
/// `handle_frame`. The listener already knows the `WtSignal` JSON
/// shape; this wrapper just converts between axum's `Message`
/// and raw bytes, and closes the socket on terminal frames.
async fn handle_socket(mut socket: WebSocket, state: WsState) {
    let mut session: Option<WebRtcSession> = None;
    loop {
        let msg = match socket.recv().await {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                debug!(error = %e, "webrtc ws: recv error");
                break;
            }
            None => break,
        };
        let payload: Vec<u8> = match msg {
            Message::Text(t) => t.as_bytes().to_vec(),
            Message::Binary(b) => b.to_vec(),
            Message::Ping(p) => {
                if let Err(e) = socket.send(Message::Pong(p)).await {
                    debug!(error = %e, "webrtc ws: pong send failed");
                    break;
                }
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => break,
        };
        let reply = state.listener.handle_frame(&mut session, &payload).await;
        let Some(frame) = reply else {
            // Non-terminal silent ack (ice-candidate / ice-end) — keep
            // reading. Terminal `bye` also returns None, but the client
            // is expected to close its side; if it doesn't, we'll see
            // either more frames or EOF on the next recv().
            continue;
        };
        let encoded = match frame.encode() {
            Ok(b) => b,
            Err(e) => {
                warn!(error = %e, "webrtc ws: encode reply failed");
                break;
            }
        };
        if let Err(e) = socket.send(Message::Binary(encoded.into())).await {
            debug!(error = %e, "webrtc ws: send failed; closing");
            break;
        }
    }
    debug!("webrtc ws: session closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_sdp::Negotiator;
    use std::net::Ipv4Addr;

    fn handler() -> CliWebRtcHandler {
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        CliWebRtcHandler::new(neg, IpAddr::V4(Ipv4Addr::LOCALHOST), 40_000, 100)
    }

    #[tokio::test]
    async fn dtls_srtp_offer_rejected_with_reason() {
        // Browsers send `UDP/TLS/RTP/SAVP`; today's negotiator
        // flags that as UnsupportedTransport. The handler must
        // forward the reason verbatim so the browser sees a
        // diagnostic, not a silent drop.
        let h = handler();
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 UDP/TLS/RTP/SAVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n\
                     a=setup:actpass\r\n\
                     a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n";
        let err = h
            .handle_offer(WebTransportSessionId(1), offer)
            .await
            .unwrap_err();
        match err {
            WebRtcHandlerError::OfferRejected(reason) => {
                assert!(
                    reason.contains("DTLS-SRTP"),
                    "reason should name DTLS-SRTP, got: {reason}"
                );
            }
            other => panic!("expected OfferRejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn plain_rtp_avp_offer_is_accepted() {
        // The SIP-style `RTP/AVP` offer goes through fine — the
        // handler isn't WebRTC-exclusive, it reuses the same
        // negotiator smiths-sip uses for UDP SIP. Useful for
        // SIP-over-WebSocket callers that ride this adapter
        // without DTLS.
        let h = handler();
        let offer = "v=0\r\n\
                     o=- 1 1 IN IP4 192.0.2.1\r\n\
                     s=-\r\n\
                     c=IN IP4 192.0.2.1\r\n\
                     t=0 0\r\n\
                     m=audio 49170 RTP/AVP 0\r\n\
                     a=rtpmap:0 PCMU/8000\r\n";
        let answer = h
            .handle_offer(WebTransportSessionId(2), offer)
            .await
            .expect("RTP/AVP offer must negotiate");
        assert!(answer.contains("m=audio"));
        assert!(answer.contains("PCMU"));
    }

    #[tokio::test]
    async fn malformed_offer_surfaces_as_offer_rejected() {
        let h = handler();
        let err = h
            .handle_offer(WebTransportSessionId(3), "not an sdp body")
            .await
            .unwrap_err();
        assert!(matches!(err, WebRtcHandlerError::OfferRejected(_)));
    }

    #[tokio::test]
    async fn port_allocator_wraps_within_configured_range() {
        // 3 sessions with range=4 should hand out the first two
        // ports then wrap on the third — proves the wrap arithmetic.
        let neg: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(IpAddr::V4(
            Ipv4Addr::LOCALHOST,
        )));
        let h = CliWebRtcHandler::new(neg, IpAddr::V4(Ipv4Addr::LOCALHOST), 40_000, 4);
        let ports: Vec<u16> = (0..3).map(|_| h.alloc_port()).collect();
        assert_eq!(ports[0], 40_000);
        assert_eq!(ports[1], 40_002);
        assert_eq!(ports[2], 40_000, "third alloc should wrap");
    }
}
