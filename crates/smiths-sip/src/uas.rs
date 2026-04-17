//! Minimal User-Agent Server.
//!
//! Phase 1 scope: answer `OPTIONS` with `200 OK`, reject every other
//! method with `405 Method Not Allowed`. Dedup by `Via` branch so a
//! UDP retransmission receives the same cached reply. Full transaction
//! FSMs with RFC 3261 timers A–K and REGISTER/INVITE handling land in
//! follow-up passes.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use dashmap::DashMap;
use smiths_core::{Event, EventBus, SipEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::transport::{Datagram, Transport};

/// Bounded cache of per-branch responses for UDP retransmission dedupe.
const DEDUPE_CAPACITY: usize = 4096;

/// Parsed request summary used internally.
struct RequestSummary {
    method: String,
    branch: Option<String>,
    call_id: Option<String>,
    /// Raw request bytes; the response builder copies header lines from
    /// them verbatim to preserve the exact `Via`, `From`, `CSeq`, etc.
    raw: Bytes,
}

/// UAS that answers `OPTIONS`. Holds an `Arc<T: Transport>` to send.
pub struct UasServer<T: Transport> {
    transport: Arc<T>,
    bus: EventBus,
    /// `branch` → cached response bytes. Bounded in size.
    dedupe: Arc<DashMap<String, Bytes>>,
}

impl<T: Transport> UasServer<T> {
    /// Build a new UAS.
    #[must_use]
    pub fn new(transport: Arc<T>, bus: EventBus) -> Self {
        Self {
            transport,
            bus,
            dedupe: Arc::new(DashMap::new()),
        }
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

        // Retransmission dedupe.
        if let Some(branch) = req.branch.as_deref()
            && let Some(cached) = self.dedupe.get(branch)
        {
            debug!(%peer, branch, "replaying cached response");
            let _ = self.transport.send(cached.clone(), peer).await;
            return;
        }

        let (status, reason) = match req.method.as_str() {
            "OPTIONS" => (200_u16, "OK"),
            _ => (405_u16, "Method Not Allowed"),
        };

        let bytes = Bytes::from(build_response(&req.raw, status, reason));

        if let Some(branch) = req.branch.as_ref() {
            if self.dedupe.len() >= DEDUPE_CAPACITY {
                // Cheap eviction — pop one arbitrary entry.
                if let Some(entry) = self.dedupe.iter().next() {
                    let k = entry.key().clone();
                    drop(entry);
                    self.dedupe.remove(&k);
                }
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
            call_id: req.call_id,
        }));
    }
}

/// Pull out the minimum routing info we need from a request's raw bytes.
///
/// We stay at the byte / line level rather than re-walking the typed
/// `rsip::Request` tree because response construction copies header
/// lines verbatim anyway — this is the simplest way to preserve exotic
/// parameter ordering that a proxy upstream may depend on.
fn summarize_request(raw: &Bytes) -> RequestSummary {
    let text = String::from_utf8_lossy(raw);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();

    let mut branch = None;
    let mut call_id = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if branch.is_none() && (lower.starts_with("via:") || lower.starts_with("v:")) {
            // Grab `;branch=...` if present.
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
        }
    }

    RequestSummary {
        method,
        branch,
        call_id,
        raw: raw.clone(),
    }
}

/// Build a response by copying the mandatory headers from `request`.
///
/// Per RFC 3261 §8.2.6, the response must copy `Via`, `From`, `To`,
/// `Call-ID`, and `CSeq`. We additionally ensure `To` carries a `tag`
/// (required for all 2xx–6xx and most 1xx — we always add one).
fn build_response(request: &Bytes, status: u16, reason: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(request);
    let mut out = String::with_capacity(request.len() + 64);
    // `write!` on String is infallible; the unwrap only suppresses a lint.
    let _ = write!(out, "SIP/2.0 {status} {reason}\r\n");

    let mut lines = text.split("\r\n");
    // Skip the request line.
    let _ = lines.next();

    for line in lines {
        if line.is_empty() {
            break;
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
            if lower.contains(";tag=") {
                out.push_str(line);
            } else {
                out.push_str(line);
                out.push_str(";tag=");
                out.push_str(&next_tag());
            }
            out.push_str("\r\n");
        }
    }

    out.push_str("Content-Length: 0\r\n\r\n");
    out.into_bytes()
}

/// Monotonic, process-unique tag suitable for `To` / `From`.
fn next_tag() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Low 64 bits of the wall-clock nanos are enough: tags only need
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

    #[test]
    fn summary_extracts_branch_and_call_id() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let s = summarize_request(&raw);
        assert_eq!(s.method, "OPTIONS");
        assert_eq!(s.branch.as_deref(), Some("z9hG4bK-abc123"));
        assert_eq!(s.call_id.as_deref(), Some("cid-xyz-42@10.0.0.1"));
    }

    #[test]
    fn response_copies_required_headers_and_adds_to_tag() {
        let raw = Bytes::copy_from_slice(SAMPLE_OPTIONS.as_bytes());
        let resp = build_response(&raw, 200, "OK");
        let s = std::str::from_utf8(&resp).unwrap();
        assert!(s.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(s.contains("Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-abc123;rport\r\n"));
        assert!(s.contains("From: Bob <sip:bob@smiths.local>;tag=314\r\n"));
        assert!(s.contains("Call-ID: cid-xyz-42@10.0.0.1\r\n"));
        assert!(s.contains("CSeq: 1 OPTIONS\r\n"));
        assert!(s.contains(";tag=smiths-"));
        assert!(s.ends_with("Content-Length: 0\r\n\r\n"));
        // Must not echo back headers we don't own.
        assert!(!s.contains("User-Agent:"));
        assert!(!s.contains("Max-Forwards:"));
    }

    #[test]
    fn tags_are_unique_across_calls() {
        let a = next_tag();
        let b = next_tag();
        assert_ne!(a, b);
    }
}
