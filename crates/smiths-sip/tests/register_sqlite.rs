//! Integration: REGISTER flow backed by `SqliteAuthStore`.
//!
//! Slice 2.1 (v0.33.0) acceptance:
//!
//! 1. Store opens against a tempfile DB, auto-migrates the v1 schema.
//! 2. Operator seeds `(realm, user, password)` via `upsert_user`.
//! 3. Unauthenticated REGISTER → 401 with challenge.
//! 4. Authenticated REGISTER → 200 OK **and** the `Contact:` binding
//!    lands in the `contacts` table — `store.snapshot()` returns it.
//! 5. REGISTER with `Expires: 0` → 200 OK + the binding is removed.

#![cfg(feature = "auth-sqlite")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::auth::digest::{Algorithm, Registrar, ha1, ha2, response_qop_auth};
use smiths_sip::auth::sqlite_store::SqliteAuthStore;
use smiths_sip::auth::{CredentialStore, Credentials, RegistrationStore};
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn spawn_uas_with_sqlite_store(store: Arc<SqliteAuthStore>) -> SocketAddr {
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

    // Same store backs the Registrar's credential lookups *and* the
    // UAS's contact-binding persistence — one Arc, two trait objects.
    let cred_store: Arc<dyn CredentialStore> = store.clone();
    let reg_store: Arc<dyn RegistrationStore> = store.clone();
    let registrar = Registrar::new("smiths.test", cred_store);

    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator)
        .unwrap()
        .with_registrar(registrar)
        .with_registration_store(reg_store);
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

fn param(header: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = header.find(&needle)? + needle.len();
    let end = header[start..].find('"')? + start;
    Some(header[start..end].to_owned())
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // full REGISTER flow is genuinely this many lines
async fn sqlite_backed_register_persists_contact_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("auth.db");
    let store = Arc::new(SqliteAuthStore::open(&db_path).unwrap());

    // Operator provisions the user out-of-band.
    store
        .upsert_user(&Credentials::new("alice", "smiths.test", "s3cret"))
        .unwrap();

    let uas_addr = spawn_uas_with_sqlite_store(store.clone()).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let ca = client.local_addr().unwrap();
    let ruri = format!("sip:smiths.test@{uas_addr}");
    let contact_uri = format!("sip:alice@{ca}");

    // --- 1. Challenge ---
    let reg_req = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-sql-1;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: sql-reg-cid@{ca}\r\n",
            "CSeq: 1 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <{contact}>\r\n",
            "Expires: 3600\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        contact = contact_uri,
    );
    client.send_to(reg_req.as_bytes(), uas_addr).await.unwrap();

    let resp1 = recv_str(&client).await;
    assert!(
        resp1.starts_with("SIP/2.0 401 Unauthorized\r\n"),
        "first response:\n{resp1}"
    );
    let nonce = param(&resp1, "nonce").expect("challenge carries a nonce");

    // --- 2. Authenticated REGISTER ---
    let h1 = ha1(Algorithm::Md5, "alice", "smiths.test", "s3cret");
    let h2 = ha2(Algorithm::Md5, "REGISTER", &ruri);
    let digest_resp = response_qop_auth(Algorithm::Md5, &h1, &nonce, "00000001", "cn-1", &h2);

    let reg_authed = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-sql-2;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: sql-reg-cid@{ca}\r\n",
            "CSeq: 2 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <{contact}>\r\n",
            "Expires: 3600\r\n",
            "Authorization: Digest username=\"alice\", realm=\"smiths.test\", \
             nonce=\"{nonce}\", uri=\"{ruri}\", response=\"{dr}\", \
             algorithm=MD5, qop=auth, nc=00000001, cnonce=\"cn-1\"\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        nonce = nonce,
        dr = digest_resp,
        contact = contact_uri,
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

    // UDP send → UAS → async bind: give the runtime a beat so the
    // DB write finishes before we snapshot. 100 ms on localhost is
    // generous even under CI load.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let bindings = RegistrationStore::snapshot(&*store).unwrap();
    assert_eq!(bindings.len(), 1, "one contact must be persisted");
    assert_eq!(bindings[0].aor, "sip:alice@smiths.test");
    assert_eq!(bindings[0].contact, contact_uri);
    assert!(bindings[0].expires_at_unix > 0);

    // --- 3. REGISTER with Expires: 0 unbinds ---
    let h2_unbind = ha2(Algorithm::Md5, "REGISTER", &ruri);
    let digest_unbind =
        response_qop_auth(Algorithm::Md5, &h1, &nonce, "00000002", "cn-2", &h2_unbind);
    let reg_unbind = format!(
        concat!(
            "REGISTER {ruri} SIP/2.0\r\n",
            "Via: SIP/2.0/UDP {ca};branch=z9hG4bK-sql-3;rport\r\n",
            "From: Alice <sip:alice@smiths.test>;tag=alice\r\n",
            "To: Alice <sip:alice@smiths.test>\r\n",
            "Call-ID: sql-reg-cid@{ca}\r\n",
            "CSeq: 3 REGISTER\r\n",
            "Max-Forwards: 70\r\n",
            "Contact: <{contact}>\r\n",
            "Expires: 0\r\n",
            "Authorization: Digest username=\"alice\", realm=\"smiths.test\", \
             nonce=\"{nonce}\", uri=\"{ruri}\", response=\"{dr}\", \
             algorithm=MD5, qop=auth, nc=00000002, cnonce=\"cn-2\"\r\n",
            "Content-Length: 0\r\n\r\n",
        ),
        ruri = ruri,
        ca = ca,
        nonce = nonce,
        dr = digest_unbind,
        contact = contact_uri,
    );
    client
        .send_to(reg_unbind.as_bytes(), uas_addr)
        .await
        .unwrap();
    let resp3 = recv_str(&client).await;
    assert!(
        resp3.starts_with("SIP/2.0 200 OK\r\n"),
        "unbind resp:\n{resp3}"
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = RegistrationStore::snapshot(&*store).unwrap();
    assert!(
        after.is_empty(),
        "Expires: 0 must remove the contact; got {after:?}"
    );
}
