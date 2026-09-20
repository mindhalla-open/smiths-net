//! Shared harness for the `uas_*` integration tests: boot a UAS on
//! loopback, build raw SIP requests, and pick responses apart.

#![allow(dead_code)] // each test binary uses a different subset of these helpers

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

pub(crate) type Uas = UasServer<UdpTransport>;

/// Boot a UAS on a loopback UDP port with `fabric` as its media
/// plane; `configure` customises the builder before `run()`.
pub(crate) async fn spawn_uas_with(
    fabric: Arc<dyn MediaFabric>,
    configure: impl FnOnce(Uas) -> Uas,
) -> SocketAddr {
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let local = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    transport.spawn_reader(tx, cancel.clone());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server =
        configure(UasServer::new(Arc::clone(&transport), bus, fabric, negotiator).unwrap());
    tokio::spawn(server.run(rx, cancel));
    local
}

/// [`spawn_uas_with`] on a plain [`UdpMediaFabric`].
pub(crate) async fn spawn_uas(configure: impl FnOnce(Uas) -> Uas) -> SocketAddr {
    spawn_uas_with(Arc::new(UdpMediaFabric::new()), configure).await
}

/// Receive one datagram as text, failing after `budget`.
pub(crate) async fn recv_str_within(sock: &UdpSocket, budget: Duration) -> String {
    let mut buf = vec![0u8; 8192];
    let (n, _) = timeout(budget, sock.recv_from(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("no datagram within {budget:?}"))
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

/// Receive one datagram as text within 2 s.
pub(crate) async fn recv_str(sock: &UdpSocket) -> String {
    recv_str_within(sock, Duration::from_secs(2)).await
}

/// Receive datagrams until one satisfies `pred`; fails after `budget`
/// overall. Everything else (e.g. a stray 2xx retransmit) is skipped.
pub(crate) async fn recv_matching(
    sock: &UdpSocket,
    budget: Duration,
    pred: impl Fn(&str) -> bool,
) -> String {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "no matching datagram within {budget:?}"
        );
        let msg = recv_str_within(sock, remaining).await;
        if pred(&msg) {
            return msg;
        }
    }
}

/// Assert that nothing arrives on `sock` for `budget`.
pub(crate) async fn expect_silence(sock: &UdpSocket, budget: Duration) {
    let mut buf = vec![0u8; 8192];
    if let Ok(Ok((n, _))) = timeout(budget, sock.recv_from(&mut buf)).await {
        panic!(
            "unexpected datagram within {budget:?}:\n{}",
            String::from_utf8_lossy(&buf[..n])
        );
    }
}

/// Status code of a response.
pub(crate) fn status_of(msg: &str) -> u16 {
    msg.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("not a response: {msg}"))
}

/// First header value (case-insensitive name), trimmed.
pub(crate) fn header(msg: &str, name: &str) -> Option<String> {
    let (headers, _) = msg.split_once("\r\n\r\n").unwrap_or((msg, ""));
    headers.split("\r\n").skip(1).find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.trim()
            .eq_ignore_ascii_case(name)
            .then(|| v.trim().to_owned())
    })
}

/// Every value of a header, in order.
pub(crate) fn headers(msg: &str, name: &str) -> Vec<String> {
    let (hdrs, _) = msg.split_once("\r\n\r\n").unwrap_or((msg, ""));
    hdrs.split("\r\n")
        .skip(1)
        .filter_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_owned())
        })
        .collect()
}

/// `;tag=` parameter of the `To` header.
pub(crate) fn to_tag_of(msg: &str) -> Option<String> {
    tag_of(&header(msg, "To")?)
}

/// `;tag=` parameter of the `From` header.
pub(crate) fn from_tag_of(msg: &str) -> Option<String> {
    tag_of(&header(msg, "From")?)
}

fn tag_of(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let idx = lower.find(";tag=")?;
    let after = &value[idx + ";tag=".len()..];
    let end = after
        .find(|c: char| c == ';' || c.is_whitespace())
        .unwrap_or(after.len());
    Some(after[..end].to_owned())
}

/// `branch=` parameter of the top `Via`.
pub(crate) fn branch_of(msg: &str) -> Option<String> {
    let via = header(msg, "Via")?;
    let lower = via.to_ascii_lowercase();
    let idx = lower.find(";branch=")?;
    let after = &via[idx + ";branch=".len()..];
    let end = after
        .find(|c: char| c == ';' || c.is_whitespace())
        .unwrap_or(after.len());
    Some(after[..end].to_owned())
}

/// Message body (after the blank line).
pub(crate) fn body_of(msg: &str) -> String {
    msg.split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default()
}

/// Build a `200 OK` (or other) response to a raw request by echoing
/// its `Via` / `From` / `To` / `Call-ID` / `CSeq`. `cseq_override` replaces the
/// `CSeq` line verbatim (used to forge a mismatching method).
pub(crate) fn response_for(
    request: &str,
    status: u16,
    reason: &str,
    cseq_override: Option<&str>,
) -> String {
    let mut out = format!("SIP/2.0 {status} {reason}\r\n");
    let (hdrs, _) = request.split_once("\r\n\r\n").unwrap_or((request, ""));
    for line in hdrs.split("\r\n").skip(1) {
        let Some((n, _)) = line.split_once(':') else {
            continue;
        };
        let n = n.trim().to_ascii_lowercase();
        match n.as_str() {
            "via" | "from" | "to" | "call-id" => {
                out.push_str(line);
                out.push_str("\r\n");
            }
            "cseq" => {
                out.push_str(cseq_override.unwrap_or(line));
                out.push_str("\r\n");
            }
            _ => {}
        }
    }
    out.push_str("Content-Length: 0\r\n\r\n");
    out
}

static BRANCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Fresh, unique Via branch.
pub(crate) fn fresh_branch(label: &str) -> String {
    let n = BRANCH_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("z9hG4bK-{label}-{n}-{}", std::process::id())
}

/// Chainable raw-request builder with sane defaults for a UA at `via`.
#[derive(Clone, Debug)]
pub(crate) struct Msg {
    pub(crate) method: String,
    pub(crate) ruri: String,
    pub(crate) via: SocketAddr,
    pub(crate) branch: String,
    pub(crate) from_uri: String,
    pub(crate) from_tag: String,
    pub(crate) to_uri: String,
    pub(crate) to_tag: Option<String>,
    pub(crate) call_id: String,
    pub(crate) cseq: u32,
    pub(crate) extra: Vec<(String, String)>,
    pub(crate) body: Option<(String, String)>,
}

impl Msg {
    pub(crate) fn new(method: &str, via: SocketAddr, call_id: &str) -> Self {
        Self {
            method: method.to_owned(),
            ruri: "sip:alice@127.0.0.1".to_owned(),
            via,
            branch: fresh_branch(&method.to_ascii_lowercase()),
            from_uri: "sip:bob@127.0.0.1".to_owned(),
            from_tag: "bob-tag".to_owned(),
            to_uri: "sip:alice@127.0.0.1".to_owned(),
            to_tag: None,
            call_id: call_id.to_owned(),
            cseq: 1,
            extra: Vec::new(),
            body: None,
        }
    }

    pub(crate) fn ruri(mut self, ruri: &str) -> Self {
        ruri.clone_into(&mut self.ruri);
        self
    }

    /// Request-URI + To URI for a rendezvous room user-part.
    pub(crate) fn room(self, user: &str) -> Self {
        let uri = format!("sip:{user}@127.0.0.1");
        self.ruri(&uri).with_to_uri(&uri)
    }

    pub(crate) fn branch(mut self, branch: &str) -> Self {
        branch.clone_into(&mut self.branch);
        self
    }

    pub(crate) fn with_from_uri(mut self, uri: &str) -> Self {
        uri.clone_into(&mut self.from_uri);
        self
    }

    pub(crate) fn with_from_tag(mut self, tag: &str) -> Self {
        tag.clone_into(&mut self.from_tag);
        self
    }

    pub(crate) fn with_to_uri(mut self, uri: &str) -> Self {
        uri.clone_into(&mut self.to_uri);
        self
    }

    pub(crate) fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_owned());
        self
    }

    pub(crate) fn cseq(mut self, cseq: u32) -> Self {
        self.cseq = cseq;
        self
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.extra.push((name.to_owned(), value.to_owned()));
        self
    }

    pub(crate) fn sdp(mut self, body: &str) -> Self {
        self.body = Some(("application/sdp".to_owned(), body.to_owned()));
        self
    }

    pub(crate) fn build(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{} {} SIP/2.0\r\n", self.method, self.ruri);
        let _ = write!(
            out,
            "Via: SIP/2.0/UDP {};branch={};rport\r\n",
            self.via, self.branch
        );
        let _ = write!(
            out,
            "From: Caller <{}>;tag={}\r\n",
            self.from_uri, self.from_tag
        );
        if let Some(t) = &self.to_tag {
            let _ = write!(out, "To: Callee <{}>;tag={t}\r\n", self.to_uri);
        } else {
            let _ = write!(out, "To: Callee <{}>\r\n", self.to_uri);
        }
        let _ = write!(out, "Call-ID: {}\r\n", self.call_id);
        let _ = write!(out, "CSeq: {} {}\r\n", self.cseq, self.method);
        out.push_str("Max-Forwards: 70\r\n");
        let has_contact = self
            .extra
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("contact"));
        if self.method == "INVITE" && !has_contact {
            let _ = write!(out, "Contact: <sip:bob@{}>\r\n", self.via);
        }
        for (n, v) in &self.extra {
            let _ = write!(out, "{n}: {v}\r\n");
        }
        if let Some((ct, body)) = &self.body {
            let _ = write!(out, "Content-Type: {ct}\r\n");
            let _ = write!(out, "Content-Length: {}\r\n\r\n{body}", body.len());
        } else {
            out.push_str("Content-Length: 0\r\n\r\n");
        }
        out
    }

    pub(crate) async fn send(&self, sock: &UdpSocket, uas: SocketAddr) {
        sock.send_to(self.build().as_bytes(), uas).await.unwrap();
    }
}

/// Minimal audio offer with the given payload types and direction.
pub(crate) fn sdp_offer(port: u16, codecs: &[(u8, &str)], direction: &str) -> String {
    use std::fmt::Write as _;
    let fmts: Vec<String> = codecs.iter().map(|(pt, _)| pt.to_string()).collect();
    let mut s = format!(
        "v=0\r\no=caller 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio {port} RTP/AVP {}\r\n",
        fmts.join(" ")
    );
    for (pt, name) in codecs {
        let _ = write!(s, "a=rtpmap:{pt} {name}\r\n");
    }
    let _ = write!(s, "a={direction}\r\n");
    s
}

pub(crate) fn pcmu_offer(port: u16) -> String {
    sdp_offer(port, &[(0, "PCMU/8000")], "sendrecv")
}

pub(crate) fn pcma_offer(port: u16) -> String {
    sdp_offer(port, &[(8, "PCMA/8000")], "sendrecv")
}

pub(crate) fn opus_offer(port: u16) -> String {
    sdp_offer(port, &[(111, "opus/48000/2")], "sendrecv")
}

/// Drive a full `INVITE → 100 → 200 → ACK` on `sock`; returns the
/// UAS's To-tag. `invite` must already carry branch / call-id.
pub(crate) async fn establish(sock: &UdpSocket, uas: SocketAddr, invite: &Msg) -> String {
    invite.send(sock, uas).await;
    let ok = recv_matching(sock, Duration::from_secs(2), |m| status_of(m) >= 200).await;
    assert_eq!(status_of(&ok), 200, "INVITE final: {ok}");
    let tag = to_tag_of(&ok).expect("2xx carries a To-tag");
    Msg::new("ACK", invite.via, &invite.call_id)
        .ruri(&invite.ruri)
        .with_from_tag(&invite.from_tag)
        .with_to_uri(&invite.to_uri)
        .with_to_tag(&tag)
        .cseq(invite.cseq)
        .send(sock, uas)
        .await;
    tag
}
