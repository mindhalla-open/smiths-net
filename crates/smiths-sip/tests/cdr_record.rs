//! Integration: INVITE → 200 → ACK → BYE produces one CDR row in
//! the wired `CdrStore`.
//!
//! Slice 2.3 (v0.35.0) acceptance — exercises the full UAS plumbing:
//! Contact / From / To parse on INVITE, side-table capture at 200 OK
//! INVITE, emit-on-BYE through the store.

#![cfg(feature = "auth-sqlite")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::storage::{CdrFilter, CdrStore};
use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::auth::sqlite_store::SqliteAuthStore;
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas_with_cdr(store: Arc<SqliteAuthStore>) -> SocketAddr {
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

    let cdr_store: Arc<dyn CdrStore> = store;
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_cdr_store(cdr_store);
    tokio::spawn(server.run(rx, cancel));
    local
}

async fn recv_str(sock: &UdpSocket) -> String {
    let mut buf = vec![0u8; 8192];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

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

#[tokio::test(flavor = "multi_thread")]
async fn invite_ack_bye_produces_one_cdr_row() {
    let store = Arc::new(SqliteAuthStore::open_in_memory().unwrap());
    let uas_addr = spawn_uas_with_cdr(store.clone()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();

    // INVITE carries an SDP offer so the UAS establishes a real
    // dialog (the CDR side-table keys off dialog establishment).
    let sdp_offer = format!(
        concat!(
            "v=0\r\n",
            "o=bob 1 1 IN IP4 127.0.0.1\r\n",
            "s=-\r\n",
            "c=IN IP4 127.0.0.1\r\n",
            "t=0 0\r\n",
            "m=audio {ca_port} RTP/AVP 0\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=sendrecv\r\n",
        ),
        ca_port = ca.port(),
    );
    let invite = format!(
        concat!(
            "INVITE sip:alice@{uas_ip} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-cdr-1;rport\r\n",
            "From: Bob <sip:bob@smiths.local>;tag=bob-cdr\r\n",
            "To: Alice <sip:alice@smiths.local>\r\n",
            "Call-ID: cdr-record-test@{ca}\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Type: application/sdp\r\n",
            "Content-Length: {clen}\r\n",
            "\r\n",
            "{body}",
        ),
        uas_ip = uas_addr.ip(),
        ca = ca,
        clen = sdp_offer.len(),
        body = sdp_offer,
    );
    client.send_to(invite.as_bytes(), uas_addr).await.unwrap();

    // 100 Trying, then 200 OK with an SDP answer.
    let trying = recv_str(&client).await;
    assert!(
        trying.starts_with("SIP/2.0 100 Trying\r\n"),
        "trying: {trying}"
    );
    let ok = recv_str(&client).await;
    assert!(ok.starts_with("SIP/2.0 200 OK\r\n"), "200 OK: {ok}");
    let tag = to_tag_of(&ok).expect("2xx must carry a to-tag");

    // ACK to confirm the dialog.
    let ack = format!(
        concat!(
            "ACK sip:alice@{uas_ip} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-cdr-2\r\n",
            "From: Bob <sip:bob@smiths.local>;tag=bob-cdr\r\n",
            "To: Alice <sip:alice@smiths.local>;tag={tag}\r\n",
            "Call-ID: cdr-record-test@{ca}\r\n",
            "CSeq: 1 ACK\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        uas_ip = uas_addr.ip(),
        ca = ca,
        tag = tag,
    );
    client.send_to(ack.as_bytes(), uas_addr).await.unwrap();

    // Let the ACK settle so the 2xx retransmit loop cancels cleanly.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // BYE: UAS should emit a CDR after the 200 OK lands.
    let bye = format!(
        concat!(
            "BYE sip:alice@{uas_ip} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-cdr-3\r\n",
            "From: Bob <sip:bob@smiths.local>;tag=bob-cdr\r\n",
            "To: Alice <sip:alice@smiths.local>;tag={tag}\r\n",
            "Call-ID: cdr-record-test@{ca}\r\n",
            "CSeq: 2 BYE\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        uas_ip = uas_addr.ip(),
        ca = ca,
        tag = tag,
    );
    client.send_to(bye.as_bytes(), uas_addr).await.unwrap();

    // Drain responses until we see the BYE's 200 (possibly preceded
    // by a stray 200 OK retransmit from the §13.3.1.4 loop).
    let bye_ok = loop {
        let resp = recv_str(&client).await;
        if resp.contains("CSeq: 2 BYE\r\n") {
            break resp;
        }
    };
    assert!(bye_ok.starts_with("SIP/2.0 200 OK\r\n"));

    // Give the async CDR write a beat to land.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let rows = CdrStore::list(&*store, &CdrFilter::new()).unwrap();
    assert_eq!(rows.len(), 1, "exactly one CDR row expected; got {rows:?}");
    let cdr = &rows[0];
    assert_eq!(cdr.call_id, format!("cdr-record-test@{ca}"));
    assert!(cdr.from_uri.contains("bob@smiths.local"));
    assert!(cdr.to_uri.contains("alice@smiths.local"));
    assert_eq!(cdr.result, "answered");
    assert!(cdr.started_at_unix > 0);
    assert!(cdr.ended_at_unix >= cdr.started_at_unix);
}
