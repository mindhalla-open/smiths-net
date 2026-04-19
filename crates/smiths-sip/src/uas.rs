//! Minimal User-Agent Server.
//!
//! Scope today:
//! - `OPTIONS` → `200 OK`.
//! - `INVITE` with SDP body → `100 Trying`, then `200 OK` carrying an
//!   SDP answer with an engine-allocated UDP port. Creates an early
//!   dialog; `ACK` confirms; `BYE` tears it down with `200 OK`.
//! - `INVITE` with no common codec → `488 Not Acceptable Here`.
//! - **Rendezvous bridging**: two `INVITE`s with the same Request-URI
//!   user-part (e.g. both to `sip:room-1@engine`) are paired. The engine
//!   spins up a byte-transparent UDP bridge between their media sockets
//!   and tears it down on `BYE` from either side.
//! - Every other method → `405 Method Not Allowed`.
//! - UDP retransmission dedupe by `Via` branch: retransmits replay the
//!   cached final response byte-for-byte.
//!
//! Non-scope (follow-up passes): full RFC 3261 transaction FSMs with
//! timers A–K, `CANCEL`, re-`INVITE`, `UPDATE`, N-party conferences,
//! TCP/TLS transports.

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use dashmap::DashMap;
use smiths_core::metrics::{Metrics, SipCodeLabel, SipMethodLabel};
use smiths_core::{
    BridgeId, DialogKey, DialogRecord, DialogState, EndpointId, Event, EventBus, MediaFabric,
    NegotiationOutcome, SdpNegotiator, SipEvent,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::transport::{Datagram, Transport};

/// Bounded cache of per-branch final responses for UDP retransmission
/// dedupe.
const DEDUPE_CAPACITY: usize = 4096;

/// First leg of a pending rendezvous bridge, waiting for a matching
/// second `INVITE`. Holds only tokens — the socket lives in the
/// [`MediaFabric`].
#[derive(Clone, Debug)]
struct PendingLeg {
    dialog_key: DialogKey,
    endpoint: EndpointId,
    remote_media: SocketAddr,
}

/// Parsed request summary.
struct RequestSummary {
    method: String,
    branch: Option<String>,
    call_id: Option<String>,
    from_tag: Option<String>,
    to_tag: Option<String>,
    /// User-part of the Request-URI (everything between `sip:` and the
    /// `@` on the request line). Used as a rendezvous key.
    ruri_user: Option<String>,
    /// Full Request-URI from the request line, used by digest auth.
    request_uri: Option<String>,
    /// Raw `Authorization:` header value, if present.
    authorization: Option<String>,
    /// Normalized `Content-Type` header value, lowercased without
    /// trailing whitespace or parameters.
    content_type: Option<String>,
    /// Message body as UTF-8 (SDP is ASCII).
    body: Option<String>,
    /// Raw request bytes; the response builder copies header lines
    /// from them verbatim.
    raw: Bytes,
}

/// UAS answering a subset of RFC 3261 requests.
///
/// Owns no sockets of its own beyond the SIP signaling transport. All
/// media-plane resources are borrowed from the injected
/// [`MediaFabric`]; SDP parsing/negotiation happens entirely through
/// the injected [`SdpNegotiator`]. That split is what lets this crate
/// depend on `smiths-core` only — no cross-sibling deps on
/// `smiths-media` or `smiths-sdp`.
pub struct UasServer<T: Transport> {
    transport: Arc<T>,
    bus: EventBus,
    /// `branch` → cached final response bytes.
    dedupe: Arc<DashMap<String, Bytes>>,
    /// Active + early dialog records keyed by
    /// `(Call-ID, local-tag, remote-tag)`. [`DialogRecord`] is
    /// serializable — this is the HA snapshot surface.
    dialogs: Arc<DashMap<DialogKey, DialogRecord>>,
    /// `Contact` header value used in responses that establish or
    /// target a dialog. Preformatted at startup from the local bind.
    contact: String,
    /// Media fabric: allocator + bridge factory. All RTP sockets live
    /// here; the UAS only ever holds [`EndpointId`] / [`BridgeId`].
    media_fabric: Arc<dyn MediaFabric>,
    /// SDP offer/answer engine. Opaque behind the trait.
    negotiator: Arc<dyn SdpNegotiator>,
    /// IP to bind RTP sockets on. Mirrors the signaling transport's
    /// local IP.
    media_bind_ip: IpAddr,
    /// First-come leg of a rendezvous bridge, keyed by Request-URI
    /// user-part. The second `INVITE` with the same key pairs with it.
    pending_bridges: Arc<DashMap<String, PendingLeg>>,
    /// Live bridges keyed by dialog. Both sides of a paired call point
    /// at the same [`BridgeId`]; the first `BYE` releases it from the
    /// fabric and clears both entries.
    bridges_by_dialog: Arc<DashMap<DialogKey, BridgeId>>,
    /// Registrar: digest-auths `REGISTER` against a [`CredentialStore`].
    /// `None` = auth disabled, registrar accepts any REGISTER blindly
    /// (dev convenience; never do that in prod).
    registrar: Option<crate::auth::digest::Registrar>,
    /// Prometheus metrics. Defaults to [`Metrics::noop`] so tests and
    /// single-server setups can ignore observability entirely.
    metrics: Arc<Metrics>,
    /// Shared correlator for responses to locally-originated requests
    /// (the [`crate::UacClient`]). `None` = UAS-only deployment;
    /// responses are simply dropped (old behaviour).
    response_router: Option<Arc<crate::ResponseRouter>>,
}

impl<T: Transport> UasServer<T> {
    /// Build a new UAS. Reads the transport's local address to compose
    /// the `Contact` header and derive the media bind IP.
    pub fn new(
        transport: Arc<T>,
        bus: EventBus,
        media_fabric: Arc<dyn MediaFabric>,
        negotiator: Arc<dyn SdpNegotiator>,
    ) -> Result<Self, crate::Error> {
        let local = transport.local_addr()?;
        // A bind of `0.0.0.0` or `[::]` would produce a non-routable
        // Contact — fine for localhost tests; the B2BUA work in later
        // phases will compute this per outbound peer.
        let contact = format!("<sip:smiths@{local}>");
        Ok(Self {
            transport,
            bus,
            dedupe: Arc::new(DashMap::new()),
            dialogs: Arc::new(DashMap::new()),
            contact,
            media_fabric,
            negotiator,
            media_bind_ip: local.ip(),
            pending_bridges: Arc::new(DashMap::new()),
            bridges_by_dialog: Arc::new(DashMap::new()),
            registrar: None,
            metrics: Metrics::noop(),
            response_router: None,
        })
    }

    /// Attach a digest registrar — `REGISTER` now requires valid auth.
    #[must_use]
    pub fn with_registrar(mut self, registrar: crate::auth::digest::Registrar) -> Self {
        self.registrar = Some(registrar);
        self
    }

    /// Attach a shared metrics handle. Without this, the UAS uses a
    /// throwaway registry — safe for tests, invisible to operators.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Install a [`crate::ResponseRouter`] so responses arriving on
    /// the UAS's socket get forwarded to the UAC. Without this, the
    /// UAS drops responses (pre-UAC behaviour).
    #[must_use]
    pub fn with_response_router(mut self, router: Arc<crate::ResponseRouter>) -> Self {
        self.response_router = Some(router);
        self
    }

    /// Run the UAS event loop. Exits when `cancel` fires or `rx` closes.
    #[instrument(skip_all)]
    pub async fn run(self, mut rx: mpsc::Receiver<Datagram>, cancel: CancellationToken) {
        info!("UAS started");
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!("UAS cancelled");
                    break;
                }
                maybe = rx.recv() => {
                    let Some(dg) = maybe else {
                        debug!("UAS datagram channel closed");
                        break;
                    };
                    self.handle_datagram(dg).await;
                }
            }
        }
        info!("UAS stopped");
    }

    async fn handle_datagram(&self, dg: Datagram) {
        let peer = dg.peer;
        let parse = match rsip::SipMessage::try_from(dg.bytes.as_ref()) {
            Ok(msg) => msg,
            Err(e) => {
                let reason = e.to_string();
                warn!(%peer, %reason, "malformed SIP message dropped");
                self.metrics.sip_parse_errors.inc();
                let _ = self
                    .bus
                    .publish(Event::Sip(SipEvent::ParseError { peer, reason }));
                return;
            }
        };

        match parse {
            rsip::SipMessage::Request(_) => {
                let summary = summarize_request(&dg.bytes);
                self.handle_request(summary, peer).await;
            }
            rsip::SipMessage::Response(_) => {
                if let Some(router) = self.response_router.as_ref() {
                    if let Some(branch) = extract_via_branch(&dg.bytes) {
                        let delivered = router.deliver(&branch, dg.bytes.clone());
                        if !delivered {
                            debug!(%peer, branch, "response with unknown branch; dropped");
                        }
                    } else {
                        debug!(%peer, "response without Via branch; dropped");
                    }
                } else {
                    debug!(%peer, "ignoring response (no UAC attached)");
                }
            }
        }
    }

    async fn handle_request(&self, req: RequestSummary, peer: SocketAddr) {
        self.metrics
            .sip_requests
            .get_or_create(&SipMethodLabel {
                method: req.method.clone(),
            })
            .inc();
        let _ = self.bus.publish(Event::Sip(SipEvent::RequestReceived {
            peer,
            method: req.method.clone(),
            call_id: req.call_id.clone(),
        }));

        // Retransmission dedupe — replay the cached FINAL response.
        // ACK carries the same branch as the INVITE it acknowledges when
        // the final response was non-2xx (RFC 3261 §17.1.1.3); replaying
        // the 401/4xx on top of that ACK would loop the transaction, so
        // the dedupe path skips ACK deliberately.
        if req.method != "ACK"
            && let Some(branch) = req.branch.as_deref()
            && let Some(cached) = self.dedupe.get(branch)
        {
            debug!(%peer, branch, "replaying cached response");
            let _ = self.transport.send(cached.clone(), peer).await;
            return;
        }

        match req.method.as_str() {
            "OPTIONS" => self.handle_options(&req, peer).await,
            "INVITE" => self.handle_invite(&req, peer).await,
            "ACK" => self.handle_ack(&req, peer),
            "BYE" => self.handle_bye(&req, peer).await,
            "REGISTER" => self.handle_register(&req, peer).await,
            _ => {
                self.respond(
                    &req,
                    405,
                    "Method Not Allowed",
                    Some(&next_tag()),
                    &[],
                    b"",
                    peer,
                )
                .await;
            }
        }
    }

    async fn handle_options(&self, req: &RequestSummary, peer: SocketAddr) {
        self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
            .await;
    }

    /// `REGISTER` with digest auth. No registrar attached → blindly
    /// `200 OK` (dev mode). Registrar attached → full challenge-response.
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_register(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(reg) = self.registrar.as_ref() else {
            self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
                .await;
            return;
        };
        let ruri = req.request_uri.as_deref().unwrap_or("");
        match req.authorization.as_deref() {
            None => {
                let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, false);
                let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                    .await;
            }
            Some(auth) => match reg.authenticate("REGISTER", ruri, auth) {
                Ok(user) => {
                    info!(%user, %peer, "REGISTER authenticated");
                    self.respond(req, 200, "OK", Some(&next_tag()), &[], b"", peer)
                        .await;
                }
                Err(e) => {
                    info!(?e, %peer, "REGISTER auth failed; re-challenging");
                    let stale = matches!(e, crate::auth::digest::AuthError::StaleNonce);
                    let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, stale);
                    let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                    self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                        .await;
                }
            },
        }
    }

    /// Digest-authenticate an incoming INVITE. Returns `true` when the
    /// request may proceed; emits the appropriate `401 Unauthorized` and
    /// returns `false` otherwise. No registrar attached → every INVITE
    /// is waved through (dev mode, matching `handle_register`).
    async fn invite_auth_ok(&self, req: &RequestSummary, peer: SocketAddr) -> bool {
        let Some(reg) = self.registrar.as_ref() else {
            return true;
        };
        let ruri = req.request_uri.as_deref().unwrap_or("");
        match req.authorization.as_deref() {
            None => {
                let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, false);
                let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                    .await;
                false
            }
            Some(auth) => match reg.authenticate("INVITE", ruri, auth) {
                Ok(user) => {
                    info!(%user, %peer, "INVITE authenticated");
                    true
                }
                Err(e) => {
                    info!(?e, %peer, "INVITE auth failed; re-challenging");
                    let stale = matches!(e, crate::auth::digest::AuthError::StaleNonce);
                    let challenge = reg.challenge(crate::auth::digest::Algorithm::Md5, stale);
                    let hdr: [(&str, &str); 1] = [("WWW-Authenticate", &challenge)];
                    self.respond(req, 401, "Unauthorized", Some(&next_tag()), &hdr, b"", peer)
                        .await;
                    false
                }
            },
        }
    }

    #[allow(clippy::too_many_lines)] // negotiation + bridge wiring belong together
    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_invite(&self, req: &RequestSummary, peer: SocketAddr) {
        // When a registrar is attached, INVITE requires digest auth. We
        // challenge before emitting 100 Trying so the rejection path
        // stays tight — no media allocation, no dialog state, just the
        // 401 back to the caller. The ACK that closes the rejected
        // transaction is handled by the normal ACK dispatch below.
        if !self.invite_auth_ok(req, peer).await {
            return;
        }

        // 100 Trying short-circuits UDP INVITE retransmission.
        self.send_provisional(req, 100, "Trying", peer).await;

        let call_id = req.call_id.clone().unwrap_or_default();
        let remote_tag = req.from_tag.clone().unwrap_or_default();
        if call_id.is_empty() || remote_tag.is_empty() {
            warn!(%peer, "INVITE missing Call-ID or From-tag; rejecting 400");
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                .await;
            return;
        }

        let has_offer =
            matches!(req.content_type.as_deref(), Some("application/sdp")) && req.body.is_some();

        // Allocate a media endpoint first (so we can include its port
        // in the answer), then negotiate. On any failure the endpoint
        // is released so the fabric's table doesn't grow unbounded.
        let (endpoint, sdp_answer_body, remote_media) = if has_offer {
            let endpoint = match self.media_fabric.allocate(self.media_bind_ip).await {
                Ok(ep) => ep,
                Err(e) => {
                    warn!(?e, "failed to allocate media endpoint for INVITE");
                    self.respond(
                        req,
                        500,
                        "Server Internal Error",
                        Some(&next_tag()),
                        &[],
                        &[],
                        peer,
                    )
                    .await;
                    return;
                }
            };
            // Safe unwrap: `has_offer` verified Some(body) above.
            let body = req.body.as_deref().unwrap_or_default();
            // When the signaling transport is bound to a wildcard
            // (0.0.0.0 / ::), `media_bind_ip` is unroutable. Ask the
            // kernel which local address it would use to reach `peer`
            // and publish *that* in SDP — otherwise the remote UA
            // tries to sendto(0.0.0.0) and fails.
            let effective_local_ip = resolve_local_ip_for(self.media_bind_ip, peer).await;
            match self.negotiator.negotiate_audio(
                body,
                effective_local_ip,
                endpoint.local_addr().port(),
            ) {
                NegotiationOutcome::Accepted {
                    answer_body,
                    remote_media,
                } => (Some(endpoint), Some(answer_body), remote_media),
                NegotiationOutcome::Mismatch => {
                    self.media_fabric.release_endpoint(endpoint.id()).await;
                    info!(%peer, "SDP offer had no acceptable codec; 488");
                    self.respond(
                        req,
                        488,
                        "Not Acceptable Here",
                        Some(&next_tag()),
                        &[],
                        &[],
                        peer,
                    )
                    .await;
                    return;
                }
                NegotiationOutcome::Malformed(err) => {
                    self.media_fabric.release_endpoint(endpoint.id()).await;
                    warn!(%peer, %err, "malformed SDP offer");
                    self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                        .await;
                    return;
                }
            }
        } else {
            (None, None, None)
        };

        let local_tag = next_tag();
        let rendezvous = req.ruri_user.clone();
        let dialog_key: DialogKey = (call_id.clone(), local_tag.clone(), remote_tag.clone());

        // Rendezvous pairing: need a key, an endpoint, and the peer RTP
        // address from the offer.
        if let (Some(key), Some(ep), Some(remote_rtp)) =
            (rendezvous.as_ref(), endpoint.as_ref(), remote_media)
        {
            if let Some((_, pending)) = self.pending_bridges.remove(key) {
                match self
                    .media_fabric
                    .bridge(pending.endpoint, pending.remote_media, ep.id(), remote_rtp)
                    .await
                {
                    Ok(bid) => {
                        self.bridges_by_dialog.insert(pending.dialog_key, bid);
                        self.bridges_by_dialog.insert(dialog_key.clone(), bid);
                        info!(rendezvous = %key, "rendezvous bridge established");
                    }
                    Err(e) => warn!(?e, rendezvous = %key, "rendezvous bridge failed"),
                }
            } else {
                self.pending_bridges.insert(
                    key.clone(),
                    PendingLeg {
                        dialog_key: dialog_key.clone(),
                        endpoint: ep.id(),
                        remote_media: remote_rtp,
                    },
                );
                info!(rendezvous = %key, "rendezvous leg parked, awaiting peer");
            }
        }

        let record = DialogRecord {
            call_id: call_id.clone(),
            local_tag: local_tag.clone(),
            remote_tag,
            state: DialogState::Early,
            peer_signal: peer,
            rendezvous,
            media: endpoint.as_ref().map(|ep| ep.id()),
            remote_media,
        };
        self.dialogs.insert(dialog_key, record);
        self.metrics.dialogs_active.inc();

        let mut extras: Vec<(&str, &str)> = vec![("Contact", self.contact.as_str())];
        if sdp_answer_body.is_some() {
            extras.push(("Content-Type", "application/sdp"));
        }
        let body_slice = sdp_answer_body.as_deref().unwrap_or("");
        self.respond(
            req,
            200,
            "OK",
            Some(&local_tag),
            &extras,
            body_slice.as_bytes(),
            peer,
        )
        .await;

        let _ = self.bus.publish(Event::Sip(SipEvent::DialogCreated {
            call_id,
            media_endpoint: endpoint.as_ref().map(|ep| ep.id()),
            remote_rtp: remote_media,
        }));
    }

    fn handle_ack(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(key) = in_dialog_key(req) else {
            debug!(%peer, "ACK missing dialog identifiers; dropping");
            return;
        };
        if let Some(mut entry) = self.dialogs.get_mut(&key) {
            if entry.state == DialogState::Early {
                entry.state = DialogState::Confirmed;
                info!(call_id = %entry.call_id, "dialog confirmed");
            }
        } else {
            debug!(?key, "ACK for unknown dialog; ignoring");
        }
    }

    #[instrument(skip_all, fields(%peer, call_id = %req.call_id.as_deref().unwrap_or("-")))]
    async fn handle_bye(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(key) = in_dialog_key(req) else {
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                .await;
            return;
        };

        match self.dialogs.remove(&key) {
            Some((_, record)) => {
                self.metrics.dialogs_active.dec();
                // Drop an unpaired pending leg if this was it.
                if let Some(rv) = record.rendezvous.as_ref()
                    && let Some(entry) = self.pending_bridges.get(rv)
                {
                    let same = entry.dialog_key == key;
                    drop(entry);
                    if same {
                        self.pending_bridges.remove(rv);
                    }
                }
                // Tear down the live bridge if this dialog is part of
                // one. Whichever side BYE-s first wins the race; the
                // second BYE finds no entry and the fabric release is
                // idempotent.
                if let Some((_, bid)) = self.bridges_by_dialog.remove(&key) {
                    self.bridges_by_dialog.retain(|_, other| *other != bid);
                    self.media_fabric.release_bridge(bid).await;
                    debug!(call_id = %record.call_id, "rendezvous bridge stopped");
                }
                if let Some(ep) = record.media {
                    self.media_fabric.release_endpoint(ep).await;
                }
                self.respond(req, 200, "OK", Some(&record.local_tag), &[], &[], peer)
                    .await;
                let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
                    call_id: record.call_id,
                }));
            }
            None => {
                self.respond(
                    req,
                    481,
                    "Call/Transaction Does Not Exist",
                    Some(&next_tag()),
                    &[],
                    &[],
                    peer,
                )
                .await;
            }
        }
    }

    /// Send a provisional (1xx) response. Not cached for dedupe.
    async fn send_provisional(
        &self,
        req: &RequestSummary,
        status: u16,
        reason: &str,
        peer: SocketAddr,
    ) {
        let bytes = Bytes::from(build_response(&req.raw, status, reason, None, &[], b""));
        if let Err(e) = self.transport.send(bytes, peer).await {
            warn!(%peer, ?e, "failed to send provisional response");
            return;
        }
        self.metrics
            .sip_responses
            .get_or_create(&SipCodeLabel {
                code: status.to_string(),
            })
            .inc();
        let _ = self.bus.publish(Event::Sip(SipEvent::ResponseSent {
            peer,
            status,
            call_id: req.call_id.clone(),
        }));
    }

    /// Send a final (>= 200) response and cache it for retransmission
    /// dedupe. `body` may be empty.
    #[allow(clippy::too_many_arguments)] // a response is genuinely this many knobs
    async fn respond(
        &self,
        req: &RequestSummary,
        status: u16,
        reason: &str,
        local_tag: Option<&str>,
        extras: &[(&str, &str)],
        body: &[u8],
        peer: SocketAddr,
    ) {
        let bytes = Bytes::from(build_response(
            &req.raw, status, reason, local_tag, extras, body,
        ));

        if let Some(branch) = req.branch.as_ref() {
            if self.dedupe.len() >= DEDUPE_CAPACITY
                && let Some(entry) = self.dedupe.iter().next()
            {
                let k = entry.key().clone();
                drop(entry);
                self.dedupe.remove(&k);
            }
            self.dedupe.insert(branch.clone(), bytes.clone());
        }

        if let Err(e) = self.transport.send(bytes, peer).await {
            warn!(%peer, ?e, "failed to send response");
            return;
        }
        self.metrics
            .sip_responses
            .get_or_create(&SipCodeLabel {
                code: status.to_string(),
            })
            .inc();
        let _ = self.bus.publish(Event::Sip(SipEvent::ResponseSent {
            peer,
            status,
            call_id: req.call_id.clone(),
        }));
    }
}

/// Key for an in-dialog request (ACK, BYE, re-INVITE).
///
/// Incoming request sees From as remote and To as local.
/// Extract the first `Via` header's `branch` parameter from any raw
/// SIP message (request or response). Returns `None` when the header
/// or parameter is missing. Used by the response-router forwarder.
fn extract_via_branch(raw: &Bytes) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.split("\r\n") {
        if line.is_empty() {
            break; // headers done
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("via:") || lower.starts_with("v:") {
            let idx = lower.find(";branch=")?;
            let after = &line[idx + ";branch=".len()..];
            let end = after
                .find(|c: char| c == ';' || c == ',' || c.is_whitespace())
                .unwrap_or(after.len());
            return Some(after[..end].to_owned());
        }
    }
    None
}

fn in_dialog_key(req: &RequestSummary) -> Option<DialogKey> {
    let call_id = req.call_id.clone()?;
    let local_tag = req.to_tag.clone()?;
    let remote_tag = req.from_tag.clone()?;
    Some((call_id, local_tag, remote_tag))
}

/// Extract the minimum routing info we need from a request's raw bytes.
fn summarize_request(raw: &Bytes) -> RequestSummary {
    let text = String::from_utf8_lossy(raw);
    let (headers, body) = split_headers_body(&text);

    let mut lines = headers.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut tokens = request_line.split_whitespace();
    let method = tokens.next().unwrap_or("").to_ascii_uppercase();
    let ruri_raw = tokens.next();
    let ruri_user = ruri_raw.and_then(ruri_user_from);
    let request_uri = ruri_raw.map(str::to_owned);

    let mut branch = None;
    let mut call_id = None;
    let mut from_tag = None;
    let mut to_tag = None;
    let mut content_type: Option<String> = None;
    let mut authorization: Option<String> = None;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if branch.is_none() && (lower.starts_with("via:") || lower.starts_with("v:")) {
            if let Some(idx) = lower.find(";branch=") {
                let rest = &line[idx + ";branch=".len()..];
                let end = rest
                    .find(|c: char| c == ';' || c.is_whitespace())
                    .unwrap_or(rest.len());
                branch = Some(rest[..end].to_owned());
            }
        } else if call_id.is_none() && (lower.starts_with("call-id:") || lower.starts_with("i:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if !v.is_empty() {
                call_id = Some(v.to_owned());
            }
        } else if from_tag.is_none() && (lower.starts_with("from:") || lower.starts_with("f:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v);
            from_tag = extract_tag_param(v);
        } else if to_tag.is_none() && (lower.starts_with("to:") || lower.starts_with("t:")) {
            let v = line.split_once(':').map_or("", |(_, v)| v);
            to_tag = extract_tag_param(v);
        } else if content_type.is_none()
            && (lower.starts_with("content-type:") || lower.starts_with("c:"))
        {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            // Strip any `; charset=...` and normalize.
            let media_type = v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase();
            if !media_type.is_empty() {
                content_type = Some(media_type);
            }
        } else if authorization.is_none() && lower.starts_with("authorization:") {
            let v = line.split_once(':').map_or("", |(_, v)| v).trim();
            if !v.is_empty() {
                authorization = Some(v.to_owned());
            }
        }
    }

    RequestSummary {
        method,
        branch,
        call_id,
        from_tag,
        to_tag,
        ruri_user,
        request_uri,
        authorization,
        content_type,
        body: (!body.is_empty()).then(|| body.to_owned()),
        raw: raw.clone(),
    }
}

/// Pull the user-part out of a Request-URI like `sip:user@host:port` or
/// `sips:user@host`. Returns `None` when the URI has no `@` segment
/// (e.g. `sip:host:port` — valid, just anonymous).
fn ruri_user_from(request_uri: &str) -> Option<String> {
    let rest = request_uri
        .strip_prefix("sip:")
        .or_else(|| request_uri.strip_prefix("sips:"))
        .or_else(|| request_uri.strip_prefix("<sip:"))
        .or_else(|| request_uri.strip_prefix("<sips:"))?;
    let (user, _) = rest.split_once('@')?;
    if user.is_empty() {
        None
    } else {
        Some(user.to_owned())
    }
}

/// Split a SIP message text into `(headers, body)`. RFC 3261 uses
/// `\r\n\r\n` as the delimiter; we also tolerate `\n\n` since some
/// tools normalize newlines.
fn split_headers_body(text: &str) -> (&str, &str) {
    if let Some(idx) = text.find("\r\n\r\n") {
        (&text[..idx], &text[idx + 4..])
    } else if let Some(idx) = text.find("\n\n") {
        (&text[..idx], &text[idx + 2..])
    } else {
        (text, "")
    }
}

/// Extract the `;tag=VALUE` parameter from a `From` / `To` header value.
fn extract_tag_param(header_value: &str) -> Option<String> {
    let lower = header_value.to_ascii_lowercase();
    let idx = lower.find(";tag=")?;
    let rest = &header_value[idx + ";tag=".len()..];
    let end = rest
        .find(|c: char| c == ';' || c == ',' || c == '>' || c.is_whitespace())
        .unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_owned())
    }
}

/// Build a response by copying the mandatory headers from `request`.
///
/// - Copies `Via`, `From`, `Call-ID`, `CSeq` verbatim.
/// - Rewrites `To`: preserves existing `;tag=` if set, otherwise appends
///   `local_tag` if provided.
/// - Appends any `extras` after the copied headers.
/// - Emits `Content-Length: N` derived from `body.len()` and appends
///   `body` after the blank line.
fn build_response(
    request: &Bytes,
    status: u16,
    reason: &str,
    local_tag: Option<&str>,
    extras: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let text = String::from_utf8_lossy(request);
    // Consider only the header region of the request; bodies can contain
    // lines that look like SIP headers (SDP never does, but other MIME
    // types could) and must not be echoed.
    let (headers, _) = split_headers_body(&text);
    let mut out = String::with_capacity(request.len() + body.len() + 64);
    // `write!` on String is infallible.
    let _ = write!(out, "SIP/2.0 {status} {reason}\r\n");

    let mut lines = headers.split("\r\n");
    let _ = lines.next(); // skip the request line

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("via:")
            || lower.starts_with("v:")
            || lower.starts_with("from:")
            || lower.starts_with("f:")
            || lower.starts_with("call-id:")
            || lower.starts_with("i:")
            || lower.starts_with("cseq:")
        {
            out.push_str(line);
            out.push_str("\r\n");
        } else if lower.starts_with("to:") || lower.starts_with("t:") {
            out.push_str(line);
            if let Some(tag) = local_tag
                && !lower.contains(";tag=")
            {
                out.push_str(";tag=");
                out.push_str(tag);
            }
            out.push_str("\r\n");
        }
    }

    for (name, value) in extras {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }

    let _ = write!(out, "Content-Length: {}\r\n\r\n", body.len());
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Pick the local IP to publish in outbound SDP for a given peer.
///
/// - If `bind_ip` is a concrete address, trust it.
/// - Otherwise (wildcard `0.0.0.0` / `::`) use the kernel's routing
///   table: bind an ephemeral UDP socket, `connect(peer)` to pick a
///   route (no packets sent), and read back the local address the
///   kernel chose. Fall back to loopback if anything fails.
async fn resolve_local_ip_for(bind_ip: IpAddr, peer: SocketAddr) -> IpAddr {
    if !bind_ip.is_unspecified() {
        return bind_ip;
    }
    let unspec: SocketAddr = match peer {
        SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    if let Ok(sock) = tokio::net::UdpSocket::bind(unspec).await
        && sock.connect(peer).await.is_ok()
        && let Ok(addr) = sock.local_addr()
        && !addr.ip().is_unspecified()
    {
        return addr.ip();
    }
    match peer {
        SocketAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        SocketAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
    }
}

/// Monotonic, process-unique tag for `From` / `To`.
fn next_tag() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Low 64 bits of wall-clock nanos are enough: tags only need
    // uniqueness within a process, not cryptographic entropy.
    #[allow(clippy::cast_possible_truncation)]
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let pid = u64::from(std::process::id());
    format!("smiths-{:016x}", n.wrapping_add(nanos ^ pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OPTIONS: &str = concat!(
        "OPTIONS sip:alice@smiths.local SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport\r\n",
        "From: Bob <sip:bob@smiths.local>;tag=314\r\n",
        "To: Alice <sip:alice@smiths.local>\r\n",
        "Call-ID: cid-xyz-42@10.0.0.1\r\n",
        "CSeq: 1 OPTIONS\r\n",
        "Max-Forwards: 70\r\n",
        "User-Agent: Smiths-Testkit/0.1\r\n",
        "Content-Length: 0\r\n\r\n",
    );

    const SAMPLE_BYE: &str = concat!(
        "BYE sip:alice@smiths.local SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-bye-1\r\n",
        "From: Bob <sip:bob@smiths.local>;tag=bob-1\r\n",
        "To: Alice <sip:alice@smiths.local>;tag=smiths-xyz\r\n",
        "Call-ID: cid-xyz-42@10.0.0.1\r\n",
        "CSeq: 2 BYE\r\n",
        "Content-Length: 0\r\n\r\n",
    );

    #[test]
    fn summary_extracts_branch_call_id_and_tags() {
        let raw = Bytes::copy_from_slice(SAMPLE_BYE.as_bytes());
        let s = summarize_request(&raw);
        assert_eq!(s.method, "BYE");
        assert_eq!(s.branch.as_deref(), Some("z9hG4bK-bye-1"));
        assert_eq!(s.call_id.as_deref(), Some("cid-xyz-42@10.0.0.1"));
        assert_eq!(s.from_tag.as_deref(), Some("bob-1"));
        assert_eq!(s.to_tag.as_deref(), Some("smiths-xyz"));
    }

    #[test]
    fn response_copies_required_headers_and_adds_to_tag() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let resp = build_response(&raw, 200, "OK", Some("abc-tag"), &[], b"");
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(s.contains("Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport\r\n"));
        assert!(s.contains("From: Bob <sip:bob@smiths.local>;tag=314\r\n"));
        assert!(s.contains("Call-ID: cid-xyz-42@10.0.0.1\r\n"));
        assert!(s.contains("CSeq: 1 OPTIONS\r\n"));
        assert!(s.contains(";tag=abc-tag\r\n"));
        assert!(s.ends_with("Content-Length: 0\r\n\r\n"));
        assert!(!s.contains("User-Agent:"));
        assert!(!s.contains("Max-Forwards:"));
    }

    #[test]
    fn response_preserves_existing_to_tag() {
        let raw = Bytes::copy_from_slice(SAMPLE_BYE.as_bytes());
        let resp = build_response(&raw, 200, "OK", Some("replacement"), &[], b"");
        let s = std::str::from_utf8(&resp).unwrap();
        // BYE's To already has tag=smiths-xyz; must NOT be overwritten.
        assert!(s.contains("To: Alice <sip:alice@smiths.local>;tag=smiths-xyz\r\n"));
        assert!(!s.contains("replacement"));
    }

    #[test]
    fn response_appends_extra_headers() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let resp = build_response(
            &raw,
            200,
            "OK",
            Some("t"),
            &[("Contact", "<sip:engine@10.0.0.9:5060>")],
            b"",
        );
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.contains("Contact: <sip:engine@10.0.0.9:5060>\r\n"));
    }

    #[test]
    fn response_body_and_content_length() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let body = b"v=0\r\n";
        let resp = build_response(
            &raw,
            200,
            "OK",
            Some("t"),
            &[("Content-Type", "application/sdp")],
            body,
        );
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.contains("Content-Type: application/sdp\r\n"));
        assert!(s.contains("Content-Length: 5\r\n\r\n"));
        assert!(s.ends_with("v=0\r\n"));
    }

    #[test]
    fn tags_are_unique_across_calls() {
        let a = next_tag();
        let b = next_tag();
        assert_ne!(a, b);
    }

    #[test]
    fn extract_tag_param_handles_edge_cases() {
        assert_eq!(extract_tag_param("<sip:x>;tag=abc"), Some("abc".to_owned()));
        assert_eq!(
            extract_tag_param("<sip:x>;TAG=abc;foo=bar"),
            Some("abc".to_owned())
        );
        assert_eq!(extract_tag_param("<sip:x>"), None);
        assert_eq!(extract_tag_param("<sip:x>;tag="), None);
    }
}
