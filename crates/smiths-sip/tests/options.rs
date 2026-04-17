//! Integration: send OPTIONS via UDP to a running UAS and assert 200.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::EventBus;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const OPTIONS_REQ: &[u8] = concat!(
    "OPTIONS sip:alice@127.0.0.1 SIP/2.0\r\n",
    "Via: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-itest-1;rport\r\n",
    "From: Tester <sip:tester@127.0.0.1>;tag=tst\r\n",
    "To: Target <sip:alice@127.0.0.1>\r\n",
    "Call-ID: cid-itest-1@127.0.0.1\r\n",
    "CSeq: 1 OPTIONS\r\n",
    "Max-Forwards: 70\r\n",
    "Content-Length: 0\r\n\r\n",
)
.as_bytes();

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
    let server = UasServer::new(Arc::clone(&transport), bus);
    tokio::spawn(server.run(rx, cancel));
    local
}

#[tokio::test(flavor = "multi_thread")]
async fn options_returns_200_ok() {
    let uas_addr = spawn_uas().await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(OPTIONS_REQ, uas_addr).await.unwrap();

    let mut buf = vec![0u8; 4096];
    let (n, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("no response within 2s")
        .unwrap();

    let resp = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        resp.starts_with("SIP/2.0 200 OK\r\n"),
        "status line: {resp}"
    );
    assert!(resp.contains("Via: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-itest-1;rport\r\n"));
    assert!(resp.contains("Call-ID: cid-itest-1@127.0.0.1\r\n"));
    assert!(resp.contains(";tag="));
    assert!(resp.ends_with("Content-Length: 0\r\n\r\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_method_returns_405() {
    let uas_addr = spawn_uas().await;

    let req = concat!(
        "SUBSCRIBE sip:alice@127.0.0.1 SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 127.0.0.1:0;branch=z9hG4bK-itest-2\r\n",
        "From: Tester <sip:tester@127.0.0.1>;tag=tst\r\n",
        "To: Target <sip:alice@127.0.0.1>\r\n",
        "Call-ID: cid-itest-2@127.0.0.1\r\n",
        "CSeq: 1 SUBSCRIBE\r\n",
        "Max-Forwards: 70\r\n",
        "Content-Length: 0\r\n\r\n",
    );
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(req.as_bytes(), uas_addr).await.unwrap();

    let mut buf = vec![0u8; 4096];
    let (n, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf))
        .await
        .expect("no response within 2s")
        .unwrap();

    let resp = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        resp.starts_with("SIP/2.0 405 Method Not Allowed\r\n"),
        "status line: {resp}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn retransmission_replays_cached_response() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    // Two identical OPTIONS (same branch) — expect identical responses.
    client.send_to(OPTIONS_REQ, uas_addr).await.unwrap();
    let mut buf1 = vec![0u8; 4096];
    let (n1, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf1))
        .await
        .unwrap()
        .unwrap();

    client.send_to(OPTIONS_REQ, uas_addr).await.unwrap();
    let mut buf2 = vec![0u8; 4096];
    let (n2, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buf2))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(&buf1[..n1], &buf2[..n2], "retransmission must be identical");
}
