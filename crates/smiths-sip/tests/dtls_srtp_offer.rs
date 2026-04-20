//! Integration: UAS recognizes (but does not terminate) DTLS-SRTP
//! offers and rejects them with `488 Not Acceptable Here` + a
//! descriptive `Warning: 399` header.
//!
//! Until slice 1.3 lands the DTLS handshake, a WebRTC-flavoured INVITE
//! arriving on the UAS is expected to:
//!
//! 1. Parse cleanly — the SDP surface (fingerprint / setup / ICE) is
//!    in place.
//! 2. Route through the negotiator, which returns
//!    `NegotiationOutcome::UnsupportedTransport`.
//! 3. Land on the peer as `488` with the RFC 3261 §20.43 warning
//!    `399 smiths-net "DTLS-SRTP not yet supported"` so the peer knows
//!    **why** we rejected rather than guessing from a naked 488.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas() -> SocketAddr {
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let local = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    transport.spawn_reader(tx, cancel.clone());
    let fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator).unwrap();
    tokio::spawn(server.run(rx, cancel));
    local
}

async fn recv(sock: &UdpSocket) -> String {
    let mut buf = vec![0u8; 8192];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

/// A minimal WebRTC-style offer: DTLS-SRTP transport, ICE candidate,
/// cert fingerprint, `setup:actpass`. Just enough of the surface that
/// slice 1.2's parser lights up.
fn dtls_srtp_offer(ca: SocketAddr) -> String {
    format!(
        concat!(
            "v=0\r\n",
            "o=- 46117317 2 IN IP4 127.0.0.1\r\n",
            "s=-\r\n",
            "c=IN IP4 127.0.0.1\r\n",
            "t=0 0\r\n",
            "m=audio {port} UDP/TLS/RTP/SAVP 111 0 8\r\n",
            "a=rtpmap:111 opus/48000/2\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=ice-ufrag:abcd\r\n",
            "a=ice-pwd:abcdabcdabcdabcdabcdabcd\r\n",
            "a=ice-options:trickle\r\n",
            "a=fingerprint:sha-256 ",
            "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:",
            "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n",
            "a=setup:actpass\r\n",
            "a=candidate:1 1 UDP 2130706431 127.0.0.1 {port} typ host\r\n",
            "a=sendrecv\r\n",
        ),
        port = ca.port(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn dtls_srtp_offer_rejected_with_488_and_warning() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let offer = dtls_srtp_offer(ca);
    let invite = format!(
        concat!(
            "INVITE sip:alice@{uas_ip} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-dtls-1;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-dtls\r\n",
            "To: Alice <sip:alice@{uas_ip}>\r\n",
            "Call-ID: dtls-reject@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Type: application/sdp\r\n",
            "Content-Length: {clen}\r\n",
            "\r\n",
            "{offer}",
        ),
        uas_ip = uas_addr.ip(),
        ca = ca,
        clen = offer.len(),
        offer = offer,
    );

    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();
    // 100 Trying lands first per the standard provisional rhythm.
    let trying = recv(&client).await;
    assert!(
        trying.starts_with("SIP/2.0 100 Trying\r\n"),
        "missing 100 Trying: {trying}"
    );

    let final_resp = recv(&client).await;
    assert!(
        final_resp.starts_with("SIP/2.0 488 Not Acceptable Here\r\n"),
        "expected 488, got: {final_resp}"
    );
    // The Warning header must carry the "DTLS-SRTP not yet supported"
    // reason so the peer can distinguish this from a codec mismatch.
    let warning_line = final_resp
        .lines()
        .find(|l| {
            l.eq_ignore_ascii_case("Warning:") || l.to_ascii_lowercase().starts_with("warning:")
        })
        .unwrap_or_else(|| panic!("response carries no Warning header:\n{final_resp}"));
    assert!(
        warning_line.contains("399"),
        "Warning must use RFC 3261 §20.43 code 399: {warning_line}"
    );
    assert!(
        warning_line.to_ascii_lowercase().contains("dtls-srtp"),
        "Warning must mention DTLS-SRTP: {warning_line}"
    );
}
