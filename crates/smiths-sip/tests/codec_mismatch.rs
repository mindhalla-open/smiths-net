//! Integration: diverse SDP shapes the UAS must reject with
//! `488 Not Acceptable Here`.
//!
//! The basic single-codec mismatch lives in `sdp.rs`; this file
//! exercises the rougher edges the negotiator has to get right:
//!
//! - Multiple unknown codecs at different clock rates in one offer.
//! - Video-only offer (engine is audio-only today).
//! - `RTP/SAVP` with a supported codec but **no** `a=crypto:` line —
//!   per RFC 4568 §5.1.2 the responder must not downgrade to plaintext,
//!   so this is a mismatch too.
//! - `RTP/SAVP` with `a=crypto:` advertising a suite we don't know.
//!
//! All four paths share the same `488` outcome. Each test also
//! confirms we emit `100 Trying` first so provisional behavior
//! doesn't regress under the negotiator refactor.

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

async fn recv(sock: &UdpSocket) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    buf[..n].to_vec()
}

/// Build an INVITE carrying `offer`; emits unique Call-ID / branch per
/// call so repeat tests don't collide on the dedupe cache.
fn build_invite(ca: SocketAddr, uas: SocketAddr, offer: &str, tag: u32) -> String {
    format!(
        concat!(
            "INVITE sip:alice@{uas_ip} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-mm-{tag};rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-{tag}\r\n",
            "To: Alice <sip:alice@{uas_ip}>\r\n",
            "Call-ID: mm-{tag}@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Type: application/sdp\r\n",
            "Content-Length: {clen}\r\n",
            "\r\n",
            "{offer}",
        ),
        uas_ip = uas.ip(),
        ca = ca,
        tag = tag,
        clen = offer.len(),
        offer = offer,
    )
}

/// Send `invite`, expect a `100 Trying`, then a `488`.
async fn expect_trying_then_488(uas: SocketAddr, client: &UdpSocket, invite: &str) {
    client.send_to(invite.as_bytes(), uas).await.unwrap();
    let trying = recv(client).await;
    assert!(
        trying.starts_with(b"SIP/2.0 100 Trying\r\n"),
        "missing 100 Trying: {}",
        String::from_utf8_lossy(&trying)
    );
    let final_resp = recv(client).await;
    assert!(
        final_resp.starts_with(b"SIP/2.0 488 Not Acceptable Here\r\n"),
        "wrong final response: {}",
        String::from_utf8_lossy(&final_resp)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_unknown_codecs_all_at_different_rates_are_488() {
    let uas = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    // Four exotic codecs spanning three clock rates — none we speak.
    // This used to trip an early-return bug where the first matching
    // PT number was accepted regardless of codec name.
    let offer = concat!(
        "v=0\r\n",
        "o=bob 1 1 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/AVP 96 97 98 99\r\n",
        "a=rtpmap:96 telephone-event/8000\r\n",
        "a=rtpmap:97 red/16000\r\n",
        "a=rtpmap:98 CN/32000\r\n",
        "a=rtpmap:99 vorbis/48000/2\r\n",
        "a=sendrecv\r\n",
    );
    let invite = build_invite(ca, uas, offer, 10);
    expect_trying_then_488(uas, &client, &invite).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn video_only_offer_is_488() {
    let uas = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    // Video-only. Engine is an audio negotiator — no `m=audio` means
    // there is nothing to answer.
    let offer = concat!(
        "v=0\r\n",
        "o=bob 1 1 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=video 40000 RTP/AVP 96\r\n",
        "a=rtpmap:96 VP8/90000\r\n",
        "a=sendrecv\r\n",
    );
    let invite = build_invite(ca, uas, offer, 11);
    expect_trying_then_488(uas, &client, &invite).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn savp_without_crypto_is_488() {
    // RFC 4568 §5.1.2: a SAVP responder must not downgrade to
    // plaintext. A SAVP offer with a perfectly fine codec but no
    // `a=crypto:` is unacceptable.
    let uas = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let offer = concat!(
        "v=0\r\n",
        "o=bob 1 1 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/SAVP 0\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=sendrecv\r\n",
    );
    let invite = build_invite(ca, uas, offer, 12);
    expect_trying_then_488(uas, &client, &invite).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn savp_with_only_unsupported_crypto_suite_is_488() {
    // SAVP + `a=crypto:` but the suite is one we don't support. The
    // parser already rejects unknown suite names, so by the time the
    // negotiator runs it sees a SAVP offer with zero crypto lines.
    let uas = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let offer = concat!(
        "v=0\r\n",
        "o=bob 1 1 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "c=IN IP4 127.0.0.1\r\n",
        "t=0 0\r\n",
        "m=audio 40000 RTP/SAVP 0\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        // 46 base64 chars = 30 bytes, matches any SDES suite length but
        // the suite name here is deliberately unknown to the engine.
        "a=crypto:1 AES_256_GCM inline:AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAh\r\n",
        "a=sendrecv\r\n",
    );
    let invite = build_invite(ca, uas, offer, 13);
    expect_trying_then_488(uas, &client, &invite).await;
}
