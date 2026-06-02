//! A single-dialog SIP UAC — the bare minimum to INVITE / ACK / BYE a
//! room on a smiths-net engine, mirroring
//! `examples/python-client/smiths_client.py`. Two softphones that
//! INVITE the same `room@engine` get their media bridged by the
//! engine's UAS rendezvous (`crates/smiths-sip/src/uas.rs`).
//!
//! Deliberately hand-rolled rather than a full SIP stack: one dialog,
//! UDP only, no transaction FSM. Voice MVP, not RFC-complete.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Overall budget for the INVITE round-trip / BYE ack.
const SIG_DEADLINE: Duration = Duration::from_secs(10);

/// A live (or about-to-be-live) SIP dialog against one engine.
pub(crate) struct SipUac {
    engine: SocketAddr,
    local_ip: IpAddr,
    sip: UdpSocket,
    rtp_port: u16,
    room: String,
    call_id: String,
    from_tag: String,
    to_tag: Option<String>,
    cseq: u32,
    /// Public address to advertise in SDP (from STUN). `None` =
    /// advertise the local interface address.
    advertised: Option<SocketAddr>,
}

impl SipUac {
    /// Bind a signaling socket and prepare a dialog targeting
    /// `room@engine`. `local_ip` is the interface that routes to the
    /// engine (see [`detect_local_ip`]); `rtp_port` is the local UDP
    /// port our media socket is already bound to — it goes into the
    /// SDP offer so the engine knows where to send our peer's audio.
    pub(crate) async fn connect(
        engine: SocketAddr,
        local_ip: IpAddr,
        room: &str,
        rtp_port: u16,
        advertised: Option<SocketAddr>,
    ) -> Result<Self> {
        let sip = UdpSocket::bind((local_ip, 0))
            .await
            .context("bind SIP socket")?;

        let salt = rand::random::<u32>();
        Ok(Self {
            engine,
            local_ip,
            sip,
            rtp_port,
            room: room.to_owned(),
            call_id: format!("softphone-{salt:08x}@{local_ip}"),
            from_tag: format!("sp-{:08x}", rand::random::<u32>()),
            to_tag: None,
            cseq: 0,
            advertised,
        })
    }

    /// The local SIP address (used in `Via` / `Contact`).
    fn sip_addr(&self) -> SocketAddr {
        self.sip
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::new(self.local_ip, 0))
    }

    /// Place the call. On a 200 OK, returns the engine's RTP address
    /// parsed from the SDP answer — where we send our μ-law packets
    /// and where the peer's audio arrives from.
    pub(crate) async fn invite(&mut self) -> Result<SocketAddr> {
        self.cseq += 1;
        let cseq = self.cseq;
        let offer = self.sdp_offer();
        let req = self.frame(
            "INVITE",
            cseq,
            &[
                ("Content-Type", "application/sdp"),
                ("Content-Length", &offer.len().to_string()),
            ],
            &offer,
        );
        self.send(&req).await?;

        let media_addr = timeout(SIG_DEADLINE, async {
            loop {
                let msg = self.recv().await?;
                let status =
                    status_code(&msg).ok_or_else(|| anyhow!("malformed SIP response:\n{msg}"))?;
                if (100..200).contains(&status) {
                    continue; // provisional — keep waiting for the final
                }
                if status != 200 {
                    let line = msg.lines().next().unwrap_or("").trim();
                    bail!("INVITE rejected: {line}");
                }
                self.to_tag = header_tag(&msg, "to");
                let body = msg.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                return parse_sdp_media(body)
                    .ok_or_else(|| anyhow!("200 OK had no usable audio media line"));
            }
        })
        .await
        .context("INVITE timed out")??;

        // ACK reuses the INVITE CSeq per RFC 3261.
        let ack = self.frame("ACK", cseq, &[], "");
        self.send(&ack).await?;
        Ok(media_addr)
    }

    /// Tear the dialog down with a BYE and wait for its 200.
    pub(crate) async fn bye(&mut self) -> Result<()> {
        self.cseq += 1;
        let req = self.frame("BYE", self.cseq, &[], "");
        self.send(&req).await?;
        let _ = timeout(SIG_DEADLINE, self.recv()).await; // best-effort
        Ok(())
    }

    // ----- plumbing -----

    async fn send(&self, msg: &str) -> Result<()> {
        self.sip
            .send_to(msg.as_bytes(), self.engine)
            .await
            .context("SIP send")?;
        Ok(())
    }

    async fn recv(&self) -> Result<String> {
        let mut buf = vec![0u8; 8192];
        let (n, _) = self.sip.recv_from(&mut buf).await.context("SIP recv")?;
        Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    fn sdp_offer(&self) -> String {
        // Advertise the STUN-discovered public address when we have one
        // so the engine's return RTP traverses NAT; otherwise the local
        // interface address (LAN / loopback).
        let (ip, port) = match self.advertised {
            Some(a) => (a.ip(), a.port()),
            None => (self.local_ip, self.rtp_port),
        };
        let sess = rand::random::<u32>();
        format!(
            "v=0\r\n\
             o=smiths-softphone {sess} 1 IN IP4 {ip}\r\n\
             s=-\r\n\
             c=IN IP4 {ip}\r\n\
             t=0 0\r\n\
             m=audio {port} RTP/AVP 0\r\n\
             a=rtpmap:0 PCMU/8000\r\n\
             a=sendrecv\r\n",
        )
    }

    fn frame(&self, method: &str, cseq: u32, extra: &[(&str, &str)], body: &str) -> String {
        let sip = self.sip_addr();
        let (eng_host, eng_port) = (self.engine.ip(), self.engine.port());
        let branch = format!("z9hG4bK-{:x}", rand::random::<u64>());
        let to = match &self.to_tag {
            Some(tag) => format!(
                "To: <sip:{room}@{eng_host}:{eng_port}>;tag={tag}",
                room = self.room
            ),
            None => format!("To: <sip:{room}@{eng_host}:{eng_port}>", room = self.room),
        };
        let mut lines = vec![
            format!("{method} sip:{}@{eng_host}:{eng_port} SIP/2.0", self.room),
            format!("Via: SIP/2.0/UDP {sip};branch={branch};rport"),
            format!("From: <sip:softphone@{sip}>;tag={}", self.from_tag),
            to,
            format!("Call-ID: {}", self.call_id),
            format!("CSeq: {cseq} {method}"),
            "Max-Forwards: 70".to_owned(),
            format!("Contact: <sip:softphone@{sip}>"),
        ];
        let mut has_clen = false;
        for (k, v) in extra {
            if k.eq_ignore_ascii_case("content-length") {
                has_clen = true;
            }
            lines.push(format!("{k}: {v}"));
        }
        if !has_clen {
            lines.push(format!("Content-Length: {}", body.len()));
        }
        format!("{}\r\n\r\n{body}", lines.join("\r\n"))
    }
}

/// Discover which local interface routes to `engine` by letting the
/// kernel pick a source address for that destination (no packets are
/// sent). Works for loopback and LAN alike.
pub(crate) fn detect_local_ip(engine: SocketAddr) -> Result<IpAddr> {
    let probe =
        std::net::UdpSocket::bind((IpAddr::from([0, 0, 0, 0]), 0)).context("bind probe socket")?;
    probe.connect(engine).context("probe connect")?;
    Ok(probe.local_addr().context("probe local_addr")?.ip())
}

/// Parse the numeric status from a SIP response's start line.
fn status_code(msg: &str) -> Option<u16> {
    let line = msg.lines().next()?;
    let mut parts = line.split_whitespace();
    let _version = parts.next()?;
    parts.next()?.parse().ok()
}

/// Extract the `tag=` parameter from a named header (case-insensitive).
fn header_tag(msg: &str, header: &str) -> Option<String> {
    for line in msg.lines() {
        // Skip the status/request line and any header without a colon.
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(header) {
            for param in rest.split(';').skip(1) {
                if let Some(tag) = param.trim().strip_prefix("tag=") {
                    return Some(tag.trim().to_owned());
                }
            }
        }
    }
    None
}

/// Pull the connection IP (`c=IN IP4 ...`) and audio port
/// (`m=audio <port> ...`) out of an SDP body.
fn parse_sdp_media(body: &str) -> Option<SocketAddr> {
    let mut ip: Option<IpAddr> = None;
    let mut port: Option<u16> = None;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
            ip = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("m=audio ") {
            port = rest.split_whitespace().next().and_then(|p| p.parse().ok());
        }
    }
    Some(SocketAddr::new(ip?, port?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status() {
        assert_eq!(status_code("SIP/2.0 200 OK\r\n"), Some(200));
        assert_eq!(status_code("SIP/2.0 100 Trying\r\n"), Some(100));
        assert_eq!(status_code("garbage"), None);
    }

    #[test]
    fn parses_to_tag() {
        let msg = "SIP/2.0 200 OK\r\nTo: <sip:room@h>;tag=abc123\r\n\r\n";
        assert_eq!(header_tag(msg, "to").as_deref(), Some("abc123"));
    }

    #[test]
    fn parses_sdp() {
        let body = "v=0\r\nc=IN IP4 10.0.0.5\r\nm=audio 40000 RTP/AVP 0\r\n";
        let addr = parse_sdp_media(body).expect("addr");
        assert_eq!(addr.to_string(), "10.0.0.5:40000");
    }
}
