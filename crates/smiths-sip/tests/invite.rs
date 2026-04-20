//! Integration: INVITE → 100 Trying → 200 OK → ACK → BYE → 200 OK.
//!
//! No SDP / media yet — the UAS answers with an empty body and a
//! Contact header. Dialog state is tracked in memory.

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

/// Extract the `;tag=...` parameter from the `To:` header of a response.
fn to_tag_of(message: &str) -> Option<String> {
    for line in message.split("\r\n") {
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

async fn recv_str(sock: &UdpSocket) -> String {
    let mut buf = vec![0u8; 4096];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_establishes_dialog_ack_then_bye() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    // ---- INVITE ----
    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-invtest-1;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-tag\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: invtest-cid-1@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();

    // Expect 100 Trying, then 200 OK (order guaranteed on loopback).
    let trying = recv_str(&client).await;
    assert!(
        trying.starts_with("SIP/2.0 100 Trying\r\n"),
        "first response: {trying}"
    );
    assert!(trying.contains("CSeq: 1 INVITE\r\n"));

    let ok = recv_str(&client).await;
    assert!(
        ok.starts_with("SIP/2.0 200 OK\r\n"),
        "second response: {ok}"
    );
    assert!(ok.contains("Call-ID: invtest-cid-1@127.0.0.1\r\n"));
    assert!(ok.contains("CSeq: 1 INVITE\r\n"));
    assert!(ok.contains("Contact: <sip:smiths@"));
    let to_tag = to_tag_of(&ok).expect("2xx must include To-tag");

    // ---- ACK (no response expected) ----
    let ack = format!(
        concat!(
            "ACK sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-invtest-ack\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-tag\r\n",
            "To: Alice <sip:alice@127.0.0.1>;tag={tt}\r\n",
            "Call-ID: invtest-cid-1@127.0.0.1\r\n",
            "CSeq: 1 ACK\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
        tt = to_tag,
    );
    client.send_to(ack.as_bytes(), uas_addr).await.unwrap();

    // Give the UAS a moment to process the ACK (which cancels the
    // §13.3.1.4 2xx retransmit loop). Under heavy test parallelism
    // the first retransmit can race the ACK; the BYE-response loop
    // below filters any stray INVITE 200s so the test stays
    // deterministic.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ---- BYE → 200 OK ----
    let bye = format!(
        concat!(
            "BYE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-invtest-bye\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob-tag\r\n",
            "To: Alice <sip:alice@127.0.0.1>;tag={tt}\r\n",
            "Call-ID: invtest-cid-1@127.0.0.1\r\n",
            "CSeq: 2 BYE\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
        tt = to_tag,
    );
    client.send_to(bye.as_bytes(), uas_addr).await.unwrap();

    // Drain responses until we find the BYE reply. Any late INVITE 2xx
    // retransmit that slipped past the ACK cancellation is discarded —
    // the retransmit contract is verified explicitly in
    // `invite_2xx_is_retransmitted_on_lost_ack`.
    let bye_ok = loop {
        let resp = recv_str(&client).await;
        if resp.contains("CSeq: 2 BYE\r\n") {
            break resp;
        }
    };
    assert!(
        bye_ok.starts_with("SIP/2.0 200 OK\r\n"),
        "BYE reply: {bye_ok}"
    );
    assert!(
        bye_ok.contains(&format!("To: Alice <sip:alice@127.0.0.1>;tag={to_tag}")),
        "BYE reply To-tag must match dialog tag; got: {bye_ok}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bye_without_dialog_returns_481() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let bye = format!(
        concat!(
            "BYE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-noinv-1\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob\r\n",
            "To: Alice <sip:alice@127.0.0.1>;tag=unknown\r\n",
            "Call-ID: nodialog-cid@127.0.0.1\r\n",
            "CSeq: 99 BYE\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );
    client.send_to(bye.as_bytes(), uas_addr).await.unwrap();

    let resp = recv_str(&client).await;
    assert!(
        resp.starts_with("SIP/2.0 481 Call/Transaction Does Not Exist\r\n"),
        "got: {resp}"
    );
}

/// RFC 3261 §13.3.1.4: until ACK lands, the TU owns the 2xx
/// retransmit schedule. Starting at T1 (500 ms), the UAS re-sends the
/// byte-identical 200 OK; the interval doubles each time up to T2.
/// A peer that retransmits the INVITE instead of waiting gets
/// silently dropped — the UAS does **not** re-answer off-schedule.
#[tokio::test(flavor = "multi_thread")]
async fn invite_2xx_is_retransmitted_on_lost_ack() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-retx-1;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: retx-cid@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );

    // First INVITE → 100 Trying + 200 OK.
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();
    let trying = recv_str(&client).await;
    assert!(trying.starts_with("SIP/2.0 100 Trying\r\n"));
    let first_ok = recv_str(&client).await;
    assert!(first_ok.starts_with("SIP/2.0 200 OK\r\n"));

    // A retransmitted INVITE is now dropped, not answered — the TU
    // drives replay. Confirm no immediate response arrives within a
    // sub-T1 window (200 ms gives plenty of slack).
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let early = timeout(Duration::from_millis(200), client.recv_from(&mut buf)).await;
    assert!(
        early.is_err(),
        "peer-retransmitted INVITE must not trigger an off-schedule answer"
    );

    // Wait for the T1-driven retransmit. T1 = 500 ms; generous slack
    // for CI scheduling jitter.
    let second_ok = timeout(Duration::from_millis(1200), async {
        let (n, _) = client.recv_from(&mut buf).await.unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    })
    .await
    .expect("UAS must retransmit 200 at T1");
    assert_eq!(
        first_ok, second_ok,
        "retransmitted 2xx must be byte-identical to the original"
    );

    // Next retransmit fires at 2·T1 (1 s).
    let third_ok = timeout(Duration::from_millis(1800), async {
        let (n, _) = client.recv_from(&mut buf).await.unwrap();
        String::from_utf8(buf[..n].to_vec()).unwrap()
    })
    .await
    .expect("UAS must retransmit again at 2·T1 (doubling)");
    assert_eq!(first_ok, third_ok, "doubled retransmit still identical");
}

/// ACK arrival must cancel the TU-owned retransmit loop. After ACK
/// no further 2xx should land on the peer socket, even past T1.
#[tokio::test(flavor = "multi_thread")]
async fn ack_cancels_2xx_retransmit() {
    let uas_addr = spawn_uas().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    let invite = format!(
        concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-ackcancel-1;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob\r\n",
            "To: Alice <sip:alice@127.0.0.1>\r\n",
            "Call-ID: ackcancel-cid@127.0.0.1\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();
    let _trying = recv_str(&client).await;
    let ok = recv_str(&client).await;
    let local_tag = to_tag_of(&ok).expect("200 must carry our to-tag");

    let ack = format!(
        concat!(
            "ACK sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-ackcancel-2;rport\r\n",
            "From: Bob <sip:bob@127.0.0.1>;tag=bob\r\n",
            "To: Alice <sip:alice@127.0.0.1>;tag={tag}\r\n",
            "Call-ID: ackcancel-cid@127.0.0.1\r\n",
            "CSeq: 1 ACK\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
        tag = local_tag,
    );
    client.send_to(ack.as_bytes(), uas_addr).await.unwrap();

    // Well past T1 (500 ms) — no retransmitted 200 must arrive.
    let mut buf = vec![0u8; 4096];
    let extra = timeout(Duration::from_millis(800), client.recv_from(&mut buf)).await;
    assert!(
        extra.is_err(),
        "ACK must cancel the §13.3.1.4 retransmit loop"
    );
}
