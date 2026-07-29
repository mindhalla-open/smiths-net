//! Engine-side User Agent Client.
//!
//! Places outbound INVITEs, handles the 100/200 round-trip, sends the
//! 2xx ACK, tracks the resulting dialog, and later tears it down with
//! a BYE. Outbound request/response correlation goes through the
//! shared [`ResponseRouter`] so the UAS reader can forward responses
//! coming in on the same socket.
//!
//! MVP scope — no transaction FSM with RFC 3261 timers A–K; one-shot
//! send + wait with a configurable budget. Full FSM lands alongside
//! the deferred dialog work.

#![allow(clippy::cast_possible_truncation, clippy::needless_pass_by_value)]

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use smiths_core::call::{CallError, CallOriginator};
use smiths_core::metrics::{Metrics, SipMethodLabel};
use smiths_core::{Event, EventBus, MediaFabric, SdpNegotiator, SipEvent};
use thiserror::Error;
use tracing::{debug, info, instrument, warn};

use crate::response_router::ResponseRouter;
use crate::transport::Transport;

/// Default overall budget for an INVITE / BYE transaction.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

/// Errors surfaced directly by the UAC (for internal call sites).
/// The [`CallOriginator`] trait impl translates these into
/// [`CallError`].
#[derive(Debug, Error)]
pub enum UacError {
    #[error("invalid target uri: {0}")]
    InvalidTarget(String),
    #[error("transport: {0}")]
    Io(#[from] std::io::Error),
    #[error("peer rejected: {status} {reason}")]
    Rejected { status: u16, reason: String },
    #[error("no dialog for call id {0}")]
    NotFound(String),
    #[error("response timeout after {millis} ms")]
    Timeout { millis: u64 },
    #[error("{0}")]
    Internal(String),
}

impl From<UacError> for CallError {
    fn from(e: UacError) -> Self {
        match e {
            UacError::InvalidTarget(m) => Self::InvalidTarget(m),
            UacError::Rejected { status, reason } => Self::Rejected { status, reason },
            UacError::NotFound(m) => Self::NotFound(m),
            UacError::Timeout { millis } => Self::Timeout { millis },
            UacError::Io(e) => Self::Internal(e.to_string()),
            UacError::Internal(m) => Self::Internal(m),
        }
    }
}

/// Live outbound dialog.
#[derive(Debug)]
#[allow(dead_code)] // `call_id` kept for logs/debug when we grow more fields
struct UacDialog {
    call_id: String,
    local_tag: String,
    remote_tag: String,
    target_uri: String,
    peer: SocketAddr,
    local_sip_addr: SocketAddr,
    next_cseq: AtomicU32,
    endpoint_id: smiths_core::EndpointId,
}

/// Engine-side User Agent Client.
pub struct UacClient<T: Transport> {
    transport: Arc<T>,
    bus: EventBus,
    media_fabric: Arc<dyn MediaFabric>,
    negotiator: Arc<dyn SdpNegotiator>,
    media_bind_ip: IpAddr,
    local_sip_addr: SocketAddr,
    contact: String,
    /// Dialogs keyed by Call-ID. The UAC only places one dialog per
    /// Call-ID so this is unique without the full `DialogKey` tuple.
    dialogs: Arc<DashMap<String, Arc<UacDialog>>>,
    metrics: Arc<Metrics>,
    deadline: Duration,
    /// Transaction-layer driver. Today only BYE flows through it
    /// (v0.17.0); INVITE migrates once the client-INVITE FSM
    /// (timers A/B/D) lands.
    txn_driver: crate::txn::TransactionDriver<T>,
}

impl<T: Transport> UacClient<T> {
    /// Build a new UAC. `local_sip_addr` must be the address the
    /// transport actually binds on — it ends up in `Via` / `Contact`
    /// / SDP `o=` / `c=` lines.
    #[must_use]
    pub fn new(
        transport: Arc<T>,
        bus: EventBus,
        media_fabric: Arc<dyn MediaFabric>,
        negotiator: Arc<dyn SdpNegotiator>,
        router: Arc<ResponseRouter>,
        local_sip_addr: SocketAddr,
        metrics: Arc<Metrics>,
    ) -> Self {
        let contact = format!("<sip:smiths@{local_sip_addr}>");
        // The response router is owned by the driver now — both INVITE
        // and BYE paths route responses through it, and the UAC has
        // no direct use for the router after slice 3 (v0.18.0).
        let txn_driver = crate::txn::TransactionDriver::new(Arc::clone(&transport), router);
        Self {
            transport,
            bus,
            media_fabric,
            negotiator,
            media_bind_ip: local_sip_addr.ip(),
            local_sip_addr,
            contact,
            dialogs: Arc::new(DashMap::new()),
            metrics,
            deadline: DEFAULT_DEADLINE,
            txn_driver,
        }
    }

    /// Override the overall request budget. Applies to each
    /// `place_call` / `hangup` independently.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Place an outbound call to the SIP URI `target`. Returns the
    /// allocated `Call-ID` on 200 OK.
    #[instrument(skip(self), fields(target = %target))]
    #[allow(clippy::too_many_lines)] // End-to-end INVITE setup lives in one place; splitting hurts readability.
    pub async fn place_call(&self, target: &str) -> Result<String, UacError> {
        let (target_uri, peer_host_port) = parse_target(target)?;
        let peer: SocketAddr = peer_host_port
            .parse()
            .map_err(|e| UacError::InvalidTarget(format!("{peer_host_port}: {e}")))?;

        // Allocate our side of the media plane.
        let endpoint = self
            .media_fabric
            .allocate(self.media_bind_ip)
            .await
            .map_err(|e| UacError::Internal(format!("media allocate: {e}")))?;

        // Build the SDP offer — negotiator-driven so new codecs get
        // picked up here without touching the UAC.
        let effective_ip = resolve_local_ip_for(self.media_bind_ip, peer).await;
        let sdp_offer = self
            .negotiator
            .build_offer(effective_ip, endpoint.local_addr().port());

        // Generate transaction identifiers.
        let call_id = fresh_call_id(&self.local_sip_addr.ip().to_string());
        let local_tag = fresh_tag("uac");
        let branch = fresh_branch();
        let cseq: u32 = 1;

        let invite = build_invite(InviteFields {
            request_uri: &target_uri,
            via_sent_by: self.local_sip_addr,
            branch: &branch,
            from_uri: &self.contact,
            from_tag: &local_tag,
            to_uri: &target_uri,
            call_id: &call_id,
            cseq,
            contact: &self.contact,
            sdp: &sdp_offer,
        });

        self.metrics
            .sip_requests
            .get_or_create(&SipMethodLabel {
                method: "INVITE".into(),
            })
            .inc();

        // Drive the INVITE through the RFC 3261 §17.1.1 client FSM.
        // The FSM handles timer-A retransmits, timer-B overall
        // timeout, and (crucially) auto-generates ACK for any non-2xx
        // final — the TU only needs to handle 2xx end-to-end ACK
        // itself, which still happens below.
        let txn = crate::txn::ClientInviteTxn::new(branch.clone(), Bytes::from(invite));
        let mut tu_rx = self.txn_driver.start_client(Box::new(txn), peer);
        debug!(%peer, branch, "UAC → INVITE (via FSM driver)");

        // Drain TuEvents until we see a final (≥200) or the overall
        // deadline expires. Provisionals are logged and skipped; a
        // `Terminated` without a final means timer B fired.
        let final_result = tokio::time::timeout(self.deadline, async {
            loop {
                match tu_rx.recv().await {
                    Some(crate::txn::TuEvent::Response { status, .. })
                        if (100..200).contains(&status) =>
                    {
                        debug!(status, "provisional during INVITE");
                    }
                    Some(crate::txn::TuEvent::Response { status, bytes }) => {
                        return Ok::<_, UacError>((status, bytes));
                    }
                    Some(crate::txn::TuEvent::Terminated) | None => {
                        return Err(UacError::Timeout { millis: u64::MAX });
                    }
                }
            }
        })
        .await;
        let (status, final_bytes) = match final_result {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                self.media_fabric.release_endpoint(endpoint.id()).await;
                return Err(e);
            }
            Err(_) => {
                self.media_fabric.release_endpoint(endpoint.id()).await;
                return Err(UacError::Timeout {
                    millis: u64::try_from(self.deadline.as_millis()).unwrap_or(u64::MAX),
                });
            }
        };

        if status != 200 {
            let reason = parse_reason(&final_bytes);
            self.media_fabric.release_endpoint(endpoint.id()).await;
            return Err(UacError::Rejected { status, reason });
        }

        let remote_tag = extract_to_tag(&final_bytes)
            .ok_or_else(|| UacError::Internal("200 OK missing To-tag".into()))?;
        let answer_body = extract_body(&final_bytes);
        let remote_rtp = self.negotiator.parse_remote_rtp(&answer_body);

        // Send the end-to-end ACK for the 2xx. Uses a NEW branch
        // per RFC 3261 §17.1.1.3 (ACK-for-2xx is its own transaction).
        let ack_branch = fresh_branch();
        let ack = build_ack_2xx(AckFields {
            request_uri: &target_uri,
            via_sent_by: self.local_sip_addr,
            branch: &ack_branch,
            from_uri: &self.contact,
            from_tag: &local_tag,
            to_uri: &target_uri,
            to_tag: &remote_tag,
            call_id: &call_id,
            cseq,
        });
        if let Err(e) = self.transport.send(Bytes::from(ack), peer).await {
            warn!(%peer, ?e, "UAC → ACK failed (call will likely drop)");
        }

        let dialog = Arc::new(UacDialog {
            call_id: call_id.clone(),
            local_tag,
            remote_tag,
            target_uri,
            peer,
            local_sip_addr: self.local_sip_addr,
            next_cseq: AtomicU32::new(cseq + 1),
            endpoint_id: endpoint.id(),
        });
        self.dialogs.insert(call_id.clone(), dialog);

        let _ = self.bus.publish(Event::Sip(SipEvent::DialogCreated {
            call_id: call_id.clone(),
            from_uri: None,
            media_endpoint: Some(endpoint.id()),
            remote_rtp,
        }));
        info!(%call_id, %peer, "UAC dialog established");

        Ok(call_id)
    }

    /// Tear down an outbound dialog with a BYE.
    ///
    /// Since **v0.17.0** this runs through the RFC 3261 §17.1.2
    /// client non-INVITE transaction FSM via
    /// [`crate::txn::TransactionDriver`]. The behavioural upshot vs
    /// the prior ad-hoc path: if the BYE's first send is lost on
    /// UDP, the driver retransmits at T1=500 ms, 1 s, 2 s, 4 s, …
    /// up to the overall 30 s budget instead of giving up silently.
    #[instrument(skip(self), fields(%call_id))]
    pub async fn hangup(&self, call_id: &str) -> Result<(), UacError> {
        let dialog = self
            .dialogs
            .get(call_id)
            .map(|e| e.value().clone())
            .ok_or_else(|| UacError::NotFound(call_id.to_owned()))?;

        let cseq = dialog.next_cseq.fetch_add(1, Ordering::Relaxed);
        let branch = fresh_branch();
        let bye = build_bye(ByeFields {
            request_uri: &dialog.target_uri,
            via_sent_by: dialog.local_sip_addr,
            branch: &branch,
            from_uri: &format!("<sip:smiths@{}>", dialog.local_sip_addr),
            from_tag: &dialog.local_tag,
            to_uri: &dialog.target_uri,
            to_tag: &dialog.remote_tag,
            call_id,
            cseq,
        });
        self.metrics
            .sip_requests
            .get_or_create(&SipMethodLabel {
                method: "BYE".into(),
            })
            .inc();

        // Drive the BYE through the non-INVITE FSM. The driver owns
        // the retransmit timer schedule + routes the peer's final
        // response through the TU channel.
        let txn = crate::txn::ClientNonInviteTxn::new(branch.clone(), "BYE", Bytes::from(bye));
        let mut tu_rx = self.txn_driver.start_client(Box::new(txn), dialog.peer);
        debug!(%dialog.peer, branch, "UAC → BYE (via FSM driver)");

        // Read TU events until we see a final (≥200) response or the
        // overall deadline expires. Provisionals (1xx) are logged and
        // skipped. `TuEvent::Terminated` before any final means the
        // FSM's own timer F fired — surface as a timeout to the caller.
        let final_status = match tokio::time::timeout(self.deadline, async {
            loop {
                match tu_rx.recv().await {
                    Some(crate::txn::TuEvent::Response { status, .. })
                        if (100..200).contains(&status) =>
                    {
                        debug!(status, "provisional during BYE; continuing");
                    }
                    Some(crate::txn::TuEvent::Response { status, bytes }) => {
                        return Ok::<_, UacError>((status, bytes));
                    }
                    Some(crate::txn::TuEvent::Terminated) | None => {
                        return Err(UacError::Timeout { millis: u64::MAX });
                    }
                }
            }
        })
        .await
        {
            Ok(Ok(ok)) => ok,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(UacError::Timeout {
                    millis: u64::try_from(self.deadline.as_millis()).unwrap_or(u64::MAX),
                });
            }
        };

        let (status, bytes) = final_status;
        if status != 200 {
            let reason = parse_reason(&bytes);
            return Err(UacError::Rejected { status, reason });
        }

        self.dialogs.remove(call_id);
        self.media_fabric.release_endpoint(dialog.endpoint_id).await;
        let _ = self.bus.publish(Event::Sip(SipEvent::DialogTerminated {
            call_id: call_id.to_owned(),
        }));
        info!(%call_id, "UAC dialog torn down");
        Ok(())
    }
}

#[async_trait]
impl<T: Transport> CallOriginator for UacClient<T> {
    async fn place_call(&self, target: &str) -> Result<String, CallError> {
        Self::place_call(self, target).await.map_err(Into::into)
    }
    async fn hangup(&self, call_id: &str) -> Result<(), CallError> {
        Self::hangup(self, call_id).await.map_err(Into::into)
    }
}

// ---------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------

struct InviteFields<'a> {
    request_uri: &'a str,
    via_sent_by: SocketAddr,
    branch: &'a str,
    from_uri: &'a str,
    from_tag: &'a str,
    to_uri: &'a str,
    call_id: &'a str,
    cseq: u32,
    contact: &'a str,
    sdp: &'a str,
}

fn build_invite(f: InviteFields<'_>) -> Vec<u8> {
    let mut out = String::new();
    let _ = write!(out, "INVITE {} SIP/2.0\r\n", f.request_uri);
    let _ = write!(
        out,
        "Via: SIP/2.0/UDP {};branch={};rport\r\n",
        f.via_sent_by, f.branch
    );
    let _ = write!(
        out,
        "From: {};tag={}\r\nTo: {}\r\n",
        f.from_uri, f.from_tag, f.to_uri
    );
    let _ = write!(out, "Call-ID: {}\r\n", f.call_id);
    let _ = write!(out, "CSeq: {} INVITE\r\n", f.cseq);
    let _ = write!(
        out,
        "Max-Forwards: 70\r\nContact: {}\r\nContent-Type: application/sdp\r\n",
        f.contact
    );
    let _ = write!(out, "Content-Length: {}\r\n\r\n", f.sdp.len());
    out.push_str(f.sdp);
    out.into_bytes()
}

struct AckFields<'a> {
    request_uri: &'a str,
    via_sent_by: SocketAddr,
    branch: &'a str,
    from_uri: &'a str,
    from_tag: &'a str,
    to_uri: &'a str,
    to_tag: &'a str,
    call_id: &'a str,
    cseq: u32,
}

fn build_ack_2xx(f: AckFields<'_>) -> Vec<u8> {
    let mut out = String::new();
    let _ = write!(out, "ACK {} SIP/2.0\r\n", f.request_uri);
    let _ = write!(
        out,
        "Via: SIP/2.0/UDP {};branch={};rport\r\n",
        f.via_sent_by, f.branch
    );
    let _ = write!(
        out,
        "From: {};tag={}\r\nTo: {};tag={}\r\n",
        f.from_uri, f.from_tag, f.to_uri, f.to_tag
    );
    let _ = write!(out, "Call-ID: {}\r\n", f.call_id);
    let _ = write!(out, "CSeq: {} ACK\r\n", f.cseq);
    out.push_str("Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n");
    out.into_bytes()
}

struct ByeFields<'a> {
    request_uri: &'a str,
    via_sent_by: SocketAddr,
    branch: &'a str,
    from_uri: &'a str,
    from_tag: &'a str,
    to_uri: &'a str,
    to_tag: &'a str,
    call_id: &'a str,
    cseq: u32,
}

fn build_bye(f: ByeFields<'_>) -> Vec<u8> {
    let mut out = String::new();
    let _ = write!(out, "BYE {} SIP/2.0\r\n", f.request_uri);
    let _ = write!(
        out,
        "Via: SIP/2.0/UDP {};branch={};rport\r\n",
        f.via_sent_by, f.branch
    );
    let _ = write!(
        out,
        "From: {};tag={}\r\nTo: {};tag={}\r\n",
        f.from_uri, f.from_tag, f.to_uri, f.to_tag
    );
    let _ = write!(out, "Call-ID: {}\r\n", f.call_id);
    let _ = write!(out, "CSeq: {} BYE\r\n", f.cseq);
    out.push_str("Max-Forwards: 70\r\nContent-Length: 0\r\n\r\n");
    out.into_bytes()
}

// ---------------------------------------------------------------------
// Response parsing (minimal — just enough for the two transactions we run)
// ---------------------------------------------------------------------

/// Wait for a non-provisional response on `branch`. Resubscribes on
/// the same branch when a 1xx arrives so the next response lands on
/// a fresh oneshot.
fn parse_reason(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).unwrap_or("");
    let line = text.lines().next().unwrap_or("");
    line.splitn(3, ' ').nth(2).unwrap_or("").to_owned()
}

fn extract_to_tag(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.split("\r\n") {
        let lower = line.to_ascii_lowercase();
        if (lower.starts_with("to:") || lower.starts_with("t:"))
            && let Some(idx) = lower.find(";tag=")
        {
            let after = &line[idx + ";tag=".len()..];
            let end = after
                .find(|c: char| c == ';' || c.is_whitespace())
                .unwrap_or(after.len());
            return Some(after[..end].to_owned());
        }
    }
    None
}

fn extract_body(bytes: &[u8]) -> String {
    let text = std::str::from_utf8(bytes).unwrap_or("");
    if let Some(idx) = text.find("\r\n\r\n") {
        text[idx + 4..].to_owned()
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------

/// Parse a SIP URI like `sip:alice@host:5060` into
/// `(request_uri_to_send, host:port)`. Falls back to port 5060 when
/// the URI omits an explicit port.
fn parse_target(target: &str) -> Result<(String, String), UacError> {
    let raw = target.trim();
    let inner = raw
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(raw);
    let body = inner
        .strip_prefix("sip:")
        .ok_or_else(|| UacError::InvalidTarget(format!("expected sip: scheme, got `{inner}`")))?;
    let host_port = match body.rsplit_once('@') {
        Some((_user, hp)) => hp.to_owned(),
        None => body.to_owned(),
    };
    let host_port = if host_port.contains(':') {
        host_port
    } else {
        format!("{host_port}:5060")
    };
    Ok((inner.to_owned(), host_port))
}

fn fresh_call_id(local: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{nanos:08x}{c:04x}@{local}")
}

fn fresh_tag(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{prefix}-{nanos:08x}{c:04x}")
}

fn fresh_branch() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    format!("z9hG4bK-{nanos:016x}{c:04x}")
}

/// Ask the kernel which local IP it would use to reach `peer` when our
/// configured bind IP is a wildcard. Mirrors the helper in `uas.rs`.
async fn resolve_local_ip_for(bind_ip: IpAddr, peer: SocketAddr) -> IpAddr {
    if !bind_ip.is_unspecified() {
        return bind_ip;
    }
    match tokio::net::UdpSocket::bind(SocketAddr::new(bind_ip, 0)).await {
        Ok(s) => match s.connect(peer).await.and_then(|()| s.local_addr()) {
            Ok(a) => a.ip(),
            Err(_) => bind_ip,
        },
        Err(_) => bind_ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_strips_angle_and_defaults_port() {
        let (uri, hp) = parse_target("<sip:alice@127.0.0.1>").unwrap();
        assert_eq!(uri, "sip:alice@127.0.0.1");
        assert_eq!(hp, "127.0.0.1:5060");
        let (uri, hp) = parse_target("sip:bob@10.0.0.1:5061").unwrap();
        assert_eq!(uri, "sip:bob@10.0.0.1:5061");
        assert_eq!(hp, "10.0.0.1:5061");
    }

    #[test]
    fn parse_target_rejects_non_sip() {
        let err = parse_target("tel:+1234").unwrap_err();
        assert!(matches!(err, UacError::InvalidTarget(_)));
    }

    #[test]
    fn extract_to_tag_from_200() {
        let bytes = b"SIP/2.0 200 OK\r\nTo: <sip:a@b>;tag=ttt\r\n\r\n";
        assert_eq!(extract_to_tag(bytes).as_deref(), Some("ttt"));
    }

    #[test]
    fn extract_body_after_double_crlf() {
        let bytes = b"SIP/2.0 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(extract_body(bytes), "hello");
    }
}
