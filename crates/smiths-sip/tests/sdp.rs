//! Integration: INVITE with an SDP offer returns `200 OK` carrying an
//! SDP answer with an engine-allocated port and an intersected codec.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::{MediaKind, Negotiator, SessionDescription};
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

async fn recv(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    buf[..n].to_vec()
}

/// Split `headers\r\n\r\nbody` out of a raw SIP response.
fn split_sip(raw: &[u8]) -> (String, Vec<u8>) {
    let needle = b"\r\n\r\n";
    let idx = raw
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or(raw.len());
    let headers = String::from_utf8_lossy(&raw[..idx]).to_string();
    let body = raw[idx.saturating_add(4).min(raw.len())..].to_vec();
    (headers, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_with_sdp_offer_gets_sdp_answer() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let offer = concat!(
        "v=0\r\n",
        "o=bob 111 222 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/AVP 0 8 111\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "a=rtpmap:111 opus/48000/2\r\n",
        "a=sendrecv\r\n",
    );

    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-sdp-1;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: sdp-cid-1@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Type: application/sdp\r\n",
            "Content-Length: {clen}\r\n",
            "\r\n",
            "{body}",
        ),
        ca = ca,
        clen = offer.len(),
        body = offer,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();

    // 100 Trying first.
    let trying = recv(&client).await;
    assert!(trying.starts_with(b"SIP/2.0 100 Trying\r\n"));

    // Then 200 OK with body.
    let ok = recv(&client).await;
    let (headers, body) = split_sip(&ok);
    assert!(headers.starts_with("SIP/2.0 200 OK\r\n"), "{headers}");
    assert!(headers.contains("Content-Type: application/sdp\r\n"));
    let body_str = std::str::from_utf8(&body).expect("SDP body is ASCII");

    let answer = SessionDescription::parse(body_str).expect("answer SDP parses");
    assert_eq!(answer.media.len(), 1);
    let m = &answer.media[0];
    assert_eq!(m.kind, MediaKind::Audio);
    assert_ne!(m.port, 0, "engine must allocate a real media port");
    // First offered codec was PCMU; we expect PCMU in the answer.
    assert_eq!(m.formats, vec![0]);
    assert_eq!(m.rtpmap.len(), 1);
    assert_eq!(m.rtpmap[0].codec.to_ascii_uppercase(), "PCMU");
    assert_eq!(m.rtpmap[0].clock_rate, 8_000);
    // c= must point at something reachable; for loopback we expect 127.0.0.1.
    assert_eq!(answer.connection.unwrap().address.to_string(), "127.0.0.1");
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_with_only_unknown_codecs_returns_488() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let offer = concat!(
        "v=0\r\n",
        "o=bob 1 1 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/AVP 96\r\n",
        "a=rtpmap:96 telephone-event/8000\r\n",
        "a=sendrecv\r\n",
    );
    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-sdp-nope;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: sdp-nope@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Type: application/sdp\r\n",
            "Content-Length: {clen}\r\n",
            "\r\n",
            "{body}",
        ),
        ca = ca,
        clen = offer.len(),
        body = offer,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();

    // 100 Trying, then 488.
    let trying = recv(&client).await;
    assert!(trying.starts_with(b"SIP/2.0 100 Trying\r\n"));
    let resp = recv(&client).await;
    assert!(
        resp.starts_with(b"SIP/2.0 488 Not Acceptable Here\r\n"),
        "got: {}",
        String::from_utf8_lossy(&resp)
    );
}
