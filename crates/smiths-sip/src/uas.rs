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
use smiths_core::{Event, EventBus, SipEvent};
use smiths_media::{Bridge, Leg};
use smiths_sdp::{MediaKind, NegotiationResult, Negotiator, SessionDescription};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::transport::{Datagram, Transport};

/// Bounded cache of per-branch final responses for UDP retransmission
/// dedupe.
const DEDUPE_CAPACITY: usize = 4096;

/// Dialog key: `(Call-ID, local tag, remote tag)`.
type DialogKey = (String, String, String);

/// Internal dialog record.
#[derive(Debug)]
struct Dialog {
    call_id: String,
    local_tag: String,
    state: DialogState,
    /// UDP socket bound for this dialog's local RTP endpoint. Kept alive
    /// here so the OS-assigned port stays ours; the bridge (if any)
    /// clones the `Arc` and drives the socket.
    #[allow(dead_code)] // referenced via the bridge; field holds ownership
    media_socket: Option<Arc<UdpSocket>>,
    /// Rendezvous key this dialog joined, if any.
    rendezvous: Option<String>,
}

/// First leg of a pending rendezvous bridge, waiting for a matching
/// second `INVITE`.
#[derive(Debug)]
struct PendingLeg {
    dialog_key: DialogKey,
    media_socket: Arc<UdpSocket>,
    remote_rtp: SocketAddr,
}

/// Handle to a live bridge shared between the two dialogs it connects.
/// Wrapped in `Mutex<Option<…>>` so whichever side receives `BYE`
/// first can `take()` it and drive `Bridge::shutdown`.
type BridgeHandle = Arc<Mutex<Option<Bridge>>>;

/// Dialog lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogState {
    /// Response was a 2xx-class — ACK not yet seen.
    Early,
    /// ACK received; call is established.
    Confirmed,
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
pub struct UasServer<T: Transport> {
    transport: Arc<T>,
    bus: EventBus,
    /// `branch` → cached final response bytes.
    dedupe: Arc<DashMap<String, Bytes>>,
    /// Active + early dialogs keyed by `(Call-ID, local-tag, remote-tag)`.
    dialogs: Arc<DashMap<DialogKey, Dialog>>,
    /// `Contact` header value used in responses that establish or
    /// target a dialog. Preformatted at startup from the local bind.
    contact: String,
    /// SDP offer/answer engine used on `INVITE`.
    negotiator: Negotiator,
    /// IP to bind RTP sockets on when allocating a media port per
    /// dialog. Mirrors the signaling transport's local IP.
    media_bind_ip: IpAddr,
    /// First-come leg of a rendezvous bridge, keyed by Request-URI
    /// user-part. The second `INVITE` with the same key pairs with it.
    pending_bridges: Arc<DashMap<String, PendingLeg>>,
    /// Live bridges keyed by dialog. Both sides of a paired call hold
    /// `Arc` clones of the same `BridgeHandle` so the first `BYE` can
    /// tear the bridge down.
    bridges_by_dialog: Arc<DashMap<DialogKey, BridgeHandle>>,
}

impl<T: Transport> UasServer<T> {
    /// Build a new UAS. Reads the transport's local address to compose
    /// the `Contact` header and seed the SDP negotiator.
    pub fn new(transport: Arc<T>, bus: EventBus) -> Result<Self, crate::Error> {
        let local = transport.local_addr()?;
        // A bind of `0.0.0.0` or `[::]` would produce a non-routable
        // Contact — fine for localhost tests; the B2BUA work in later
        // phases will compute this per outbound peer.
        let contact = format!("<sip:smiths@{local}>");
        let negotiator = Negotiator::with_default_codecs(local.ip());
        Ok(Self {
            transport,
            bus,
            dedupe: Arc::new(DashMap::new()),
            dialogs: Arc::new(DashMap::new()),
            contact,
            negotiator,
            media_bind_ip: local.ip(),
            pending_bridges: Arc::new(DashMap::new()),
            bridges_by_dialog: Arc::new(DashMap::new()),
        })
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
                debug!(%peer, "ignoring unsolicited response (no UAC state yet)");
            }
        }
    }

    async fn handle_request(&self, req: RequestSummary, peer: SocketAddr) {
        let _ = self.bus.publish(Event::Sip(SipEvent::RequestReceived {
            peer,
            method: req.method.clone(),
            call_id: req.call_id.clone(),
        }));

        // Retransmission dedupe — replay the cached FINAL response.
        if let Some(branch) = req.branch.as_deref()
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

    #[allow(clippy::too_many_lines)] // negotiation + bridge wiring belong together
    async fn handle_invite(&self, req: &RequestSummary, peer: SocketAddr) {
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

        // Parse + negotiate SDP when the body is `application/sdp`.
        let offer_parsed: Option<SessionDescription> =
            match (req.content_type.as_deref(), req.body.as_deref()) {
                (Some("application/sdp"), Some(body)) => match SessionDescription::parse(body) {
                    Ok(o) => Some(o),
                    Err(e) => {
                        warn!(%peer, ?e, "malformed SDP offer");
                        self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                            .await;
                        return;
                    }
                },
                _ => None,
            };

        // Allocate a local media socket + build an SDP answer whenever
        // we have an offer.
        let (media_socket, sdp_answer_body) = if let Some(offer) = offer_parsed.as_ref() {
            let socket = match self.allocate_media_socket().await {
                Ok(s) => s,
                Err(e) => {
                    warn!(?e, "failed to allocate media port for INVITE");
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
            let port = socket.local_addr().map(|a| a.port()).unwrap_or_default();
            match self.negotiator.answer(offer, port) {
                NegotiationResult::Answer(sdp) => (Some(socket), Some(sdp.to_string())),
                NegotiationResult::Mismatch => {
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
            }
        } else {
            (None, None)
        };

        let local_tag = next_tag();
        let rendezvous = req.ruri_user.clone();
        let dialog_key: DialogKey = (call_id.clone(), local_tag.clone(), remote_tag);

        // Try to bridge: need a rendezvous key, a local socket, and
        // the peer RTP address from the offer.
        if let (Some(key), Some(local_sock), Some(offer)) = (
            rendezvous.as_ref(),
            media_socket.as_ref(),
            offer_parsed.as_ref(),
        ) && let Some(remote_rtp) = sdp_remote_rtp(offer)
        {
            if let Some((_, pending)) = self.pending_bridges.remove(key) {
                let leg_a = Leg {
                    socket: pending.media_socket,
                    peer: pending.remote_rtp,
                };
                let leg_b = Leg {
                    socket: Arc::clone(local_sock),
                    peer: remote_rtp,
                };
                let bridge = Bridge::spawn(&leg_a, &leg_b);
                let handle: BridgeHandle = Arc::new(Mutex::new(Some(bridge)));
                self.bridges_by_dialog
                    .insert(pending.dialog_key, Arc::clone(&handle));
                self.bridges_by_dialog
                    .insert(dialog_key.clone(), Arc::clone(&handle));
                info!(rendezvous = %key, "rendezvous bridge established");
            } else {
                self.pending_bridges.insert(
                    key.clone(),
                    PendingLeg {
                        dialog_key: dialog_key.clone(),
                        media_socket: Arc::clone(local_sock),
                        remote_rtp,
                    },
                );
                info!(rendezvous = %key, "rendezvous leg parked, awaiting peer");
            }
        }

        let dialog = Dialog {
            call_id: call_id.clone(),
            local_tag: local_tag.clone(),
            state: DialogState::Early,
            media_socket,
            rendezvous,
        };
        self.dialogs.insert(dialog_key, dialog);

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

        let _ = self
            .bus
            .publish(Event::Sip(SipEvent::DialogCreated { call_id }));
    }

    /// Bind a fresh UDP socket on an ephemeral port for this dialog.
    /// Step 3 will use it for RTP forwarding; for now we just hold it.
    async fn allocate_media_socket(&self) -> std::io::Result<Arc<UdpSocket>> {
        let bind = SocketAddr::new(self.media_bind_ip, 0);
        let socket = UdpSocket::bind(bind).await?;
        Ok(Arc::new(socket))
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

    async fn handle_bye(&self, req: &RequestSummary, peer: SocketAddr) {
        let Some(key) = in_dialog_key(req) else {
            self.respond(req, 400, "Bad Request", Some(&next_tag()), &[], &[], peer)
                .await;
            return;
        };

        match self.dialogs.remove(&key) {
            Some((_, dialog)) => {
                // Drop an unpaired pending leg if this was it.
                if let Some(rv) = dialog.rendezvous.as_ref() {
                    if let Some(entry) = self.pending_bridges.get(rv) {
                        if entry.dialog_key == key {
                            drop(entry);
                            self.pending_bridges.remove(rv);
                        }
                    }
                }
                // Tear down the live bridge if this dialog is part of one.
                if let Some((_, handle)) = self.bridges_by_dialog.remove(&key) {
                    // Remove the sibling's entry too.
                    self.bridges_by_dialog
                        .retain(|_, other| !Arc::ptr_eq(other, &handle));
                    let bridge_opt = handle.lock().await.take();
                    if let Some(bridge) = bridge_opt {
                        bridge.shutdown().await;
                        debug!(call_id = %dialog.call_id, "rendezvous bridge stopped");
                    }
                }
                self.respond(req, 200, "OK", Some(&dialog.local_tag), &[], &[], peer)
                    .await;
                let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
                    call_id: dialog.call_id,
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
    let ruri_user = tokens.next().and_then(ruri_user_from);

    let mut branch = None;
    let mut call_id = None;
    let mut from_tag = None;
    let mut to_tag = None;
    let mut content_type: Option<String> = None;

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
        }
    }

    RequestSummary {
        method,
        branch,
        call_id,
        from_tag,
        to_tag,
        ruri_user,
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

/// Extract the remote RTP endpoint from an SDP offer: first `m=audio`
/// port + media-level or session-level `c=` address.
fn sdp_remote_rtp(sdp: &SessionDescription) -> Option<SocketAddr> {
    let audio = sdp.media.iter().find(|m| m.kind == MediaKind::Audio)?;
    if audio.port == 0 {
        return None;
    }
    let conn = audio.connection.as_ref().or(sdp.connection.as_ref())?;
    Some(SocketAddr::new(conn.address, audio.port))
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
