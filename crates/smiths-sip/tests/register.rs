//! Integration: REGISTER → 401 (with WWW-Authenticate) → REGISTER with
//! valid digest → 200 OK. Uses MD5 per RFC 2617 + `qop=auth`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::auth::digest::{Algorithm, Registrar, ha1, ha2, response_qop_auth};
use smiths_sip::auth::{Credentials, InMemoryCredentialStore};
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas_with_registrar() -> (SocketAddr, Arc<InMemoryCredentialStore>) {
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
    store.insert(Credentials {
        username: "alice".into(),
        realm: "smiths.test".into(),
        password: "s3cret".into(),
    });
    let registrar = Registrar::new("smiths.test", store.clone());

    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_registrar(registrar);
    tokio::spawn(server.run(rx, cancel));
    (local, store)
}

async fn recv_str(sock: &UdpSocket) -> String {
    let mut buf = vec![0u8; 8192];
    let (n, _) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
        .await
        .expect("no reply within 2 s")
        .unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

fn param(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = header.find(&needle)? + needle.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn register_challenge_then_authenticate() {
    let (uas_addr, _store) = spawn_uas_with_registrar().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let ruri = format!("sip:smiths.test@{uas_addr}");

    // --- 1. Unauthenticated REGISTER → 401 with challenge ---
    let reg_req = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-reg-1;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: reg-cid-1@{ca}\r\n",
            "CSeq: 1 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:alice@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
    );
    client.send_to(reg_req.as_bytes(), uas_addr).await.unwrap();

    let resp1 = recv_str(&client).await;
    assert!(
        resp1.starts_with("SIP/2.0 401 Unauthorized\r\n"),
        "first response:\n{resp1}"
    );
    assert!(
        resp1.contains("WWW-Authenticate: Digest "),
        "missing challenge:\n{resp1}"
    );
    assert!(resp1.contains("realm=\"smiths.test\""));
    let nonce = param(&resp1, "nonce").expect("challenge carries a nonce");

    // --- 2. Compute digest response and re-send ---
    let h1 = ha1(Algorithm::Md5, "alice", "smiths.test", "s3cret");
    let h2 = ha2(Algorithm::Md5, "REGISTER", &ruri);
    let resp = response_qop_auth(Algorithm::Md5, &h1, &nonce, "00000001", "cn-1", &h2);
    let reg_authed = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-reg-2;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: reg-cid-1@{ca}\r\n",
            "CSeq: 2 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:alice@{ca}>\r\n",
            "Authorization: Digest username=\"alice\", realm=\"smiths.test\", \
             nonce=\"{nonce}\", uri=\"{ruri}\", response=\"{resp}\", \
             algorithm=MD5, qop=auth, nc=00000001, cnonce=\"cn-1\"\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        nonce = nonce,
        resp = resp,
    );
    client
        .send_to(reg_authed.as_bytes(), uas_addr)
        .await
        .unwrap();

    let resp2 = recv_str(&client).await;
    assert!(
        resp2.starts_with("SIP/2.0 200 OK\r\n"),
        "authenticated response:\n{resp2}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn register_wrong_password_re_challenges() {
    let (uas_addr, _store) = spawn_uas_with_registrar().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let ruri = format!("sip:smiths.test@{uas_addr}");

    // Fresh nonce.
    let reg_req = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-bad-1;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: bad-cid@{ca}\r\n",
            "CSeq: 1 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:alice@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
    );
    client.send_to(reg_req.as_bytes(), uas_addr).await.unwrap();
    let challenge = recv_str(&client).await;
    let nonce = param(&challenge, "nonce").unwrap();

    // Digest computed with the WRONG password.
    let h1 = ha1(Algorithm::Md5, "alice", "smiths.test", "WRONG");
    let h2 = ha2(Algorithm::Md5, "REGISTER", &ruri);
    let resp = response_qop_auth(Algorithm::Md5, &h1, &nonce, "00000001", "cn", &h2);
    let reg_bad = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-bad-2;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: bad-cid@{ca}\r\n",
            "CSeq: 2 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:alice@{ca}>\r\n",
            "Authorization: Digest username=\"alice\", realm=\"smiths.test\", \
             nonce=\"{nonce}\", uri=\"{ruri}\", response=\"{resp}\", \
             algorithm=MD5, qop=auth, nc=00000001, cnonce=\"cn\"\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        nonce = nonce,
        resp = resp,
    );
    client.send_to(reg_bad.as_bytes(), uas_addr).await.unwrap();

    let resp2 = recv_str(&client).await;
    assert!(
        resp2.starts_with("SIP/2.0 401 Unauthorized\r\n"),
        "bad-password response should re-challenge:\n{resp2}"
    );
    // New nonce must be issued on rejection.
    let new_nonce = param(&resp2, "nonce").unwrap();
    assert_ne!(new_nonce, nonce, "each 401 should carry a fresh nonce");
}

#[tokio::test(flavor = "multi_thread")]
async fn register_without_registrar_is_accepted_blindly() {
    // Minimal UAS with no registrar attached — REGISTER should 200 OK
    // (dev mode).
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let local = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(8);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    transport.spawn_reader(tx, cancel.clone());
    let fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(local.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator).unwrap();
    tokio::spawn(server.run(rx, cancel));

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let reg = format!(
        concat!(
            "REGISTER sip:smiths.test SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-noauth;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: noauth-cid@{ca}\r\n",
            "CSeq: 1 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <sip:alice@{ca}>\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ca = ca,
    );
    client.send_to(reg.as_bytes(), local).await.unwrap();
    let resp = recv_str(&client).await;
    assert!(resp.starts_with("SIP/2.0 200 OK\r\n"), "got:\n{resp}");
}
