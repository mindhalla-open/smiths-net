//! Integration: graceful drain rejects new INVITEs with 503.
//!
//! Spawns a UAS with a shared [`smiths_core::Drain`], flips it before
//! sending an INVITE, and asserts the response is
//! `503 Service Unavailable` rather than the usual
//! `100 Trying` / `200 OK` pair.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{Drain, EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas_with_drain() -> (SocketAddr, Drain) {
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
    let drain = Drain::new();
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_drain(drain.clone());
    tokio::spawn(server.run(rx, cancel));
    (local, drain)
}

async fn recv_str(sock: &UdpSocket) -> String {
    let mut buf = vec![0u8; 4096];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn draining_uas_rejects_new_invite_with_503() {
    let (uas_addr, drain) = spawn_uas_with_drain().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    drain.start();

    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-draintest-1;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-tag\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: draintest-cid-1@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();

    // The draining path skips 100 Trying entirely and answers 503
    // directly — no intermediate provisional.
    let resp = recv_str(&client).await;
    assert!(
        resp.starts_with("SIP/2.0 503 Service Unavailable\r\n"),
        "expected 503, got: {resp}"
    );
    assert!(
        resp.contains("Retry-After: 0\r\n"),
        "503 must carry Retry-After: 0; got: {resp}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn non_draining_uas_still_accepts_invite() {
    // Sanity check: the drain handle is wired but not started; the
    // UAS should behave normally.
    let (uas_addr, _drain) = spawn_uas_with_drain().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-draintest-ok;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-tag\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: draintest-cid-ok@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();

    let trying = recv_str(&client).await;
    assert!(
        trying.starts_with("SIP/2.0 100 Trying\r\n"),
        "expected 100 Trying, got: {trying}"
    );
}
