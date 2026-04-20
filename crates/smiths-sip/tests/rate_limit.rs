//! Integration: UAS drops over-limit SIP datagrams from a single source.
//!
//! Hostile flood protection. The limiter bucket is sized so that a
//! handful of requests get through and the rest are dropped before
//! reaching the parser. We send 20 OPTIONS from the same source in a
//! tight loop; with `per_sec=5, burst=5` only the first 5 should be
//! answered. Dropped datagrams produce no response at all (silent
//! drop — intentional, see `rate_limit.rs`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator, SipRateLimit};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{SipRateLimiter, Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas(limit: SipRateLimit) -> SocketAddr {
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let local = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(128);
    transport.spawn_reader(tx, cancel.clone());
    let fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_rate_limit(SipRateLimiter::new(limit));
    tokio::spawn(server.run(rx, cancel));
    local
}

fn options(ca: SocketAddr, cid: &str) -> Vec<u8> {
    format!(
        concat!(
            "OPTIONS sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-rl-{cid};rport\r\n",
            "From: T <sip:t@127.0.0.1>;tag=t\r\n",
            "To: A <sip:a@127.0.0.1>\r\n",
            "Call-ID: {cid}\r\n",
            "CSeq: 1 OPTIONS\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
        cid = cid,
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limiter_caps_responses_at_burst_depth() {
    // 5 tokens/s, burst 5. Bucket starts full; first 5 requests go
    // through, the rest are dropped silently.
    let uas = spawn_uas(SipRateLimit {
        per_sec: 5,
        burst: 5,
    })
    .await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    for i in 0..20 {
        let cid = format!("rl-cid-{i}");
        client.send_to(&options(ca, &cid), uas).await.unwrap();
    }

    let mut ok = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    let mut buf = [0u8; 2048];
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, client.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) if buf[..n].starts_with(b"SIP/2.0 200 OK\r\n") => ok += 1,
            _ => break,
        }
    }
    assert_eq!(
        ok, 5,
        "exactly burst=5 responses should come back, got {ok}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_limiter_lets_everything_through() {
    // per_sec=0 → limiter disabled. All 10 requests must be answered.
    let uas = spawn_uas(SipRateLimit {
        per_sec: 0,
        burst: 0,
    })
    .await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    for i in 0..10 {
        let cid = format!("nolim-cid-{i}");
        client.send_to(&options(ca, &cid), uas).await.unwrap();
    }

    let mut ok = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let mut buf = [0u8; 2048];
    while tokio::time::Instant::now() < deadline && ok < 10 {
        let remaining = deadline - tokio::time::Instant::now();
        match timeout(remaining, client.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) if buf[..n].starts_with(b"SIP/2.0 200 OK\r\n") => ok += 1,
            _ => break,
        }
    }
    assert_eq!(ok, 10, "disabled limiter must answer all 10, got {ok}");
}
