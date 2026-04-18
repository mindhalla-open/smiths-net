//! Tiny UAC for integration tests.
//!
//! Emits INVITE with an SDP offer, reads `100` + `200`, ACKs, exposes
//! the peer's media endpoint (from the SDP answer), and later sends
//! `BYE`. Purpose-built for two-party audio-bridging tests — not a
//! general-purpose SIP client.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use smiths_sdp::SessionDescription;
use tokio::net::UdpSocket;
use tokio::time::timeout;

const RECV_TIMEOUT: Duration = Duration::from_secs(3);

/// Minimal fake UAC used by the audio-bridge test harness.
pub struct FakeUac {
    /// UDP socket for SIP signaling.
    pub sip: UdpSocket,
    /// UDP socket that will carry RTP in this UA's direction.
    pub rtp: UdpSocket,
    /// Engine signaling address.
    pub engine: SocketAddr,
    /// Call-ID for the in-flight dialog.
    pub call_id: String,
    /// Local From-tag for the in-flight dialog.
    pub from_tag: String,
    /// Remote To-tag learned from the 200 OK.
    pub to_tag: Option<String>,
    /// Engine's media endpoint parsed from the SDP answer. `None`
    /// until `invite` completes.
    pub engine_rtp: Option<SocketAddr>,
    cseq: u32,
}

impl FakeUac {
    /// Bind loopback sockets and capture the engine's SIP address.
    pub async fn bind(engine: SocketAddr) -> std::io::Result<Self> {
        let sip = UdpSocket::bind("127.0.0.1:0").await?;
        let rtp = UdpSocket::bind("127.0.0.1:0").await?;
        let call_id = format!("testkit-{}@127.0.0.1", unique_u32());
        let from_tag = format!("tk-{}", unique_u32());
        Ok(Self {
            sip,
            rtp,
            engine,
            call_id,
            from_tag,
            to_tag: None,
            engine_rtp: None,
            cseq: 0,
        })
    }

    /// Local SIP socket address.
    pub fn sip_addr(&self) -> std::io::Result<SocketAddr> {
        self.sip.local_addr()
    }

    /// Local RTP socket address — put this in the SDP offer.
    pub fn rtp_addr(&self) -> std::io::Result<SocketAddr> {
        self.rtp.local_addr()
    }

    /// INVITE `sip:<rendezvous>@<engine>` with a PCMU-only SDP offer.
    /// Returns once `200 OK` is received. Sends the ACK.
    pub async fn invite(&mut self, rendezvous: &str) -> std::io::Result<()> {
        self.cseq += 1;
        let cseq = self.cseq;
        let local_sip = self.sip_addr()?;
        let local_rtp = self.rtp_addr()?;
        let offer = offer_sdp_pcmu(&local_rtp);
        let invite = format!(
            "INVITE sip:{rv}@{eng} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {sip};branch=z9hG4bK-{branch};rport\r\n\
             From: Tester <sip:tester@{sip}>;tag={ftag}\r\n\
             To: Target <sip:{rv}@{eng}>\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Max-Forwards: 70\r\n\
             Contact: <sip:tester@{sip}>\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {clen}\r\n\
             \r\n\
             {offer}",
            rv = rendezvous,
            eng = self.engine,
            sip = local_sip,
            branch = unique_u32(),
            ftag = self.from_tag,
            cid = self.call_id,
            cseq = cseq,
            clen = offer.len(),
            offer = offer,
        );
        self.sip.send_to(invite.as_bytes(), self.engine).await?;

        // Read responses until we see the 200 OK. 100 Trying comes first.
        let mut buf = vec![0u8; 8192];
        loop {
            let (n, _) = timeout(RECV_TIMEOUT, self.sip.recv_from(&mut buf))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "INVITE timeout")
                })??;
            let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
            if msg.starts_with("SIP/2.0 1") {
                continue; // provisional (100 Trying) — wait for final
            }
            if !msg.starts_with("SIP/2.0 200") {
                return Err(std::io::Error::other(format!(
                    "INVITE rejected: {}",
                    first_line(&msg)
                )));
            }
            // Final 200 OK: extract To-tag, parse SDP answer.
            self.to_tag = extract_to_tag(&msg);
            let (_, body) = split_sip(&msg);
            if let Ok(sdp) = SessionDescription::parse(body)
                && let Some(addr) = first_audio_endpoint(&sdp)
            {
                self.engine_rtp = Some(addr);
            }
            break;
        }

        // Ack (end-to-end ACK for 2xx INVITE carries its own CSeq).
        let ack = format!(
            "ACK sip:{rv}@{eng} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {sip};branch=z9hG4bK-{branch};rport\r\n\
             From: Tester <sip:tester@{sip}>;tag={ftag}\r\n\
             To: Target <sip:{rv}@{eng}>;tag={ttag}\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} ACK\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n",
            rv = rendezvous,
            eng = self.engine,
            sip = local_sip,
            branch = unique_u32(),
            ftag = self.from_tag,
            ttag = self.to_tag.as_deref().unwrap_or(""),
            cid = self.call_id,
            cseq = cseq,
        );
        self.sip.send_to(ack.as_bytes(), self.engine).await?;
        Ok(())
    }

    /// INVITE `sip:<rendezvous>@<engine>` with a PCMU-only SDP offer,
    /// expecting a non-2xx final response (e.g. `401 Unauthorized`,
    /// `488 Not Acceptable`, `404 Not Found`). ACKs the response
    /// hop-by-hop with the same `Via` branch per RFC 3261 §17.1.1.3
    /// and returns the raw response text.
    ///
    /// Returns `Err` if the engine answered with a 2xx instead — the
    /// caller expected rejection, and a dialog would be leaked.
    pub async fn invite_expect_rejection(&mut self, rendezvous: &str) -> std::io::Result<String> {
        self.cseq += 1;
        let cseq = self.cseq;
        let branch = format!("z9hG4bK-{}", unique_u32());
        let local_sip = self.sip_addr()?;
        let local_rtp = self.rtp_addr()?;
        let offer = offer_sdp_pcmu(&local_rtp);
        let invite = format!(
            "INVITE sip:{rv}@{eng} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {sip};branch={branch};rport\r\n\
             From: Tester <sip:tester@{sip}>;tag={ftag}\r\n\
             To: Target <sip:{rv}@{eng}>\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Max-Forwards: 70\r\n\
             Contact: <sip:tester@{sip}>\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: {clen}\r\n\
             \r\n\
             {offer}",
            rv = rendezvous,
            eng = self.engine,
            sip = local_sip,
            branch = branch,
            ftag = self.from_tag,
            cid = self.call_id,
            cseq = cseq,
            clen = offer.len(),
            offer = offer,
        );
        self.sip.send_to(invite.as_bytes(), self.engine).await?;

        let mut buf = vec![0u8; 8192];
        let msg = loop {
            let (n, _) = timeout(RECV_TIMEOUT, self.sip.recv_from(&mut buf))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "INVITE timeout")
                })??;
            let m = String::from_utf8_lossy(&buf[..n]).into_owned();
            if m.starts_with("SIP/2.0 1") {
                continue; // provisional — wait for final
            }
            break m;
        };

        if msg.starts_with("SIP/2.0 2") {
            return Err(std::io::Error::other(format!(
                "expected rejection, got 2xx: {}",
                first_line(&msg)
            )));
        }

        // Pull the To-tag the engine attached to the error response and
        // ACK the INVITE transaction so the UAS stops retransmitting.
        self.to_tag = extract_to_tag(&msg);
        let ack = format!(
            "ACK sip:{rv}@{eng} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {sip};branch={branch};rport\r\n\
             From: Tester <sip:tester@{sip}>;tag={ftag}\r\n\
             To: Target <sip:{rv}@{eng}>;tag={ttag}\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} ACK\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n",
            rv = rendezvous,
            eng = self.engine,
            sip = local_sip,
            branch = branch,
            ftag = self.from_tag,
            ttag = self.to_tag.as_deref().unwrap_or(""),
            cid = self.call_id,
            cseq = cseq,
        );
        self.sip.send_to(ack.as_bytes(), self.engine).await?;
        Ok(msg)
    }

    /// Send BYE and wait for `200 OK`.
    pub async fn bye(&mut self, rendezvous: &str) -> std::io::Result<()> {
        self.cseq += 1;
        let cseq = self.cseq;
        let local_sip = self.sip_addr()?;
        let bye = format!(
            "BYE sip:{rv}@{eng} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {sip};branch=z9hG4bK-{branch};rport\r\n\
             From: Tester <sip:tester@{sip}>;tag={ftag}\r\n\
             To: Target <sip:{rv}@{eng}>;tag={ttag}\r\n\
             Call-ID: {cid}\r\n\
             CSeq: {cseq} BYE\r\n\
             Max-Forwards: 70\r\n\
             Content-Length: 0\r\n\r\n",
            rv = rendezvous,
            eng = self.engine,
            sip = local_sip,
            branch = unique_u32(),
            ftag = self.from_tag,
            ttag = self.to_tag.as_deref().unwrap_or(""),
            cid = self.call_id,
            cseq = cseq,
        );
        self.sip.send_to(bye.as_bytes(), self.engine).await?;

        let mut buf = vec![0u8; 4096];
        let (n, _) = timeout(RECV_TIMEOUT, self.sip.recv_from(&mut buf))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "BYE timeout"))??;
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
        if !msg.starts_with("SIP/2.0 200") {
            return Err(std::io::Error::other(format!(
                "BYE not acked: {}",
                first_line(&msg)
            )));
        }
        Ok(())
    }
}

fn offer_sdp_pcmu(local_rtp: &SocketAddr) -> String {
    format!(
        "v=0\r\n\
         o=tester {sess} 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/AVP 0\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=sendrecv\r\n",
        sess = unique_u32(),
        ip = local_rtp.ip(),
        port = local_rtp.port(),
    )
}

fn unique_u32() -> u32 {
    static C: AtomicU32 = AtomicU32::new(1);
    C.fetch_add(1, Ordering::Relaxed)
}

fn first_line(msg: &str) -> &str {
    msg.lines().next().unwrap_or(msg)
}

fn split_sip(msg: &str) -> (&str, &str) {
    if let Some(i) = msg.find("\r\n\r\n") {
        (&msg[..i], &msg[i + 4..])
    } else {
        (msg, "")
    }
}

fn extract_to_tag(msg: &str) -> Option<String> {
    for line in msg.split("\r\n") {
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

fn first_audio_endpoint(sdp: &SessionDescription) -> Option<SocketAddr> {
    let audio = sdp
        .media
        .iter()
        .find(|m| m.kind == smiths_sdp::MediaKind::Audio)?;
    let conn = audio.connection.as_ref().or(sdp.connection.as_ref())?;
    Some(SocketAddr::new(conn.address, audio.port))
}
