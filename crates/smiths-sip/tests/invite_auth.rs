//! Integration: INVITE under digest auth.
//!
//! An engine running with a registrar must challenge every
//! unauthenticated INVITE with a `401 Unauthorized` (carrying a
//! fresh `WWW-Authenticate` digest header) and must not allocate a
//! dialog. A follow-up `BYE` purporting to end that "call" must come
//! back with `481 Call/Transaction Does Not Exist`, proving no
//! dialog state leaked from the rejected transaction.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::auth::digest::Registrar;
use smiths_sip::auth::{Credentials, InMemoryCredentialStore};
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas_with_registrar() -> SocketAddr {
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

    let store = Arc::new(InMemoryCredentialStore::new());
    store.insert(Credentials::new("alice", "smiths.test", "s3cret"));
    let registrar = Registrar::new("smiths.test", store);

    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_registrar(registrar);
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

fn to_tag_of(msg: &str) -> Option<String> {
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

#[tokio::test(flavor = "multi_thread")]
async fn invite_401_cancel() {
    let uas = spawn_uas_with_registrar().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let ruri = format!("sip:alice@{uas}");
    let call_id = format!("inv-auth-1@{ca}");
    let branch = "z9hG4bK-inv-401";

    // --- 1. Unauthenticated INVITE ---
    let invite = format!(
        concat!(
            "INVITE {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch={branch};rport\r\n",
            "From: Caller <sip:bob@{ca}>;tag=bob\r\n",
            "To: Alice <sip:alice@{uas}>\r\n",
            "Call-ID: {cid}\r\n",
            "CSeq: 1 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:bob@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        uas = uas,
        branch = branch,
        cid = call_id,
    );
    client.send_to(invite.as_bytes(), uas).await.unwrap();

    // Skip any `100 Trying`; assert the final is `401 Unauthorized`.
    // (When auth is configured, the engine rejects before 100 Trying,
    //  but tolerating the provisional keeps the test resilient.)
    let resp = loop {
        let r = recv_str(&client).await;
        if r.starts_with("SIP/2.0 1") {
            continue;
        }
        break r;
    };
    assert!(
        resp.starts_with("SIP/2.0 401 Unauthorized\r\n"),
        "expected 401, got:\n{resp}"
    );
    assert!(
        resp.contains("WWW-Authenticate: Digest "),
        "missing challenge:\n{resp}"
    );
    assert!(resp.contains("realm=\"smiths.test\""));
    let engine_tag = to_tag_of(&resp).expect("401 response carries a To-tag");

    // --- 2. ACK the 401 (hop-by-hop; same branch as the INVITE per §17.1.1.3) ---
    let ack = format!(
        concat!(
            "ACK {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch={branch};rport\r\n",
            "From: Caller <sip:bob@{ca}>;tag=bob\r\n",
            "To: Alice <sip:alice@{uas}>;tag={ttag}\r\n",
            "Call-ID: {cid}\r\n",
            "CSeq: 1 ACK\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        uas = uas,
        branch = branch,
        ttag = engine_tag,
        cid = call_id,
    );
    client.send_to(ack.as_bytes(), uas).await.unwrap();

    // --- 3. BYE pretending a dialog was established — must be 481 ---
    let bye = format!(
        concat!(
            "BYE {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-inv-bye;rport\r\n",
            "From: Caller <sip:bob@{ca}>;tag=bob\r\n",
            "To: Alice <sip:alice@{uas}>;tag={ttag}\r\n",
            "Call-ID: {cid}\r\n",
            "CSeq: 2 BYE\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        uas = uas,
        ttag = engine_tag,
        cid = call_id,
    );
    client.send_to(bye.as_bytes(), uas).await.unwrap();

    let bye_resp = recv_str(&client).await;
    assert!(
        bye_resp.starts_with("SIP/2.0 481"),
        "expected 481 (no such dialog), got:\n{bye_resp}"
    );
}
