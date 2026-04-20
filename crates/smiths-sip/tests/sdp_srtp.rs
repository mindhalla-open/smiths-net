//! End-to-end: two UAs rendezvous with `RTP/SAVP` + `a=crypto:`.
//!
//! Each side offers `AES_CM_128_HMAC_SHA1_80` with its own key.
//! The engine answers each INVITE with a matching `a=crypto:` carrying
//! an engine-generated key, parks the first leg, pairs the second,
//! and wires per-direction SRTP transforms on the bridge.
//!
//! UA-A encrypts an RTP packet with the key it put in its offer; the
//! bridge decrypts on ingress, rewrites SSRC, re-encrypts with the key
//! the engine advertised to UA-B; UA-B decrypts with that key and we
//! verify the payload round-trips.
//!
//! Guardrail: confirms no SDES key material ever lands on the wire
//! encrypted with the wrong half, which was the whole point of the
//! "wire SDES through the negotiator" slice.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use smiths_core::{EventBus, MediaFabric, SdpNegotiator, SrtpTransform};
use smiths_media::UdpMediaFabric;
use smiths_media::srtp::AesCmHmacSha1_80Transform;
use smiths_sdp::{MediaKind, Negotiator, SessionDescription};
use smiths_sip::{Transport as _, UasServer, UdpTransport};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const RENDEZVOUS: &str = "srtp-room";

async fn spawn_engine() -> SocketAddr {
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

/// Build an `RTP/SAVP` PCMU offer with one `a=crypto:` line.
fn savp_offer(local_rtp: &SocketAddr, tag: u32, key_material: &[u8]) -> String {
    format!(
        "v=0\r\n\
         o=tester 1 1 IN IP4 {ip}\r\n\
         s=-\r\n\
         c=IN IP4 {ip}\r\n\
         t=0 0\r\n\
         m=audio {port} RTP/SAVP 0\r\n\
         a=rtpmap:0 PCMU/8000\r\n\
         a=crypto:{tag} AES_CM_128_HMAC_SHA1_80 inline:{km}\r\n\
         a=sendrecv\r\n",
        ip = local_rtp.ip(),
        port = local_rtp.port(),
        tag = tag,
        km = BASE64.encode(key_material),
    )
}

struct UaInvite {
    /// SIP socket kept alive for the life of the test so the engine
    /// doesn't see the dialog evaporate under it.
    _sip: UdpSocket,
    rtp: UdpSocket,
    engine_rtp: SocketAddr,
    /// Key material the engine put in its answer; used to encrypt
    /// egress toward the engine (peer side decrypts with it) OR to
    /// decrypt ingress FROM the engine (depending on direction —
    /// SDES uses the same material for the answerer's outbound).
    engine_local_tx_key: Vec<u8>,
}

/// Send INVITE + read 200 + send ACK. Returns sockets + the engine's
/// answer crypto key.
async fn invite_savp(engine: SocketAddr, offer_tag: u32, peer_tx_key: &[u8]) -> UaInvite {
    let sip = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let rtp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sip_addr = sip.local_addr().unwrap();
    let rtp_addr = rtp.local_addr().unwrap();
    let offer = savp_offer(&rtp_addr, offer_tag, peer_tx_key);
    let call_id = format!("srtp-{offer_tag}@127.0.0.1");
    let from_tag = format!("tag-{offer_tag}");
    let branch = format!("z9hG4bK-srtp-{offer_tag}");
    let invite = format!(
        "INVITE sip:{rv}@{eng} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {sip};branch={branch};rport\r\n\
         From: Tester <sip:tester@{sip}>;tag={ft}\r\n\
         To: Target <sip:{rv}@{eng}>\r\n\
         Call-ID: {cid}\r\n\
         CSeq: 1 INVITE\r\n\
         Max-Forwards: 70\r\n\
         Contact: <sip:tester@{sip}>\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {clen}\r\n\
         \r\n\
         {offer}",
        rv = RENDEZVOUS,
        eng = engine,
        sip = sip_addr,
        branch = branch,
        ft = from_tag,
        cid = call_id,
        clen = offer.len(),
        offer = offer,
    );
    sip.send_to(invite.as_bytes(), engine).await.unwrap();

    let mut buf = vec![0u8; 8192];
    let (resp, to_tag, answer_body) = loop {
        let (n, _) = timeout(Duration::from_secs(3), sip.recv_from(&mut buf))
            .await
            .expect("INVITE response timeout")
            .unwrap();
        let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
        if msg.starts_with("SIP/2.0 1") {
            continue;
        }
        assert!(
            msg.starts_with("SIP/2.0 200"),
            "engine rejected SAVP INVITE: {}",
            msg.lines().next().unwrap_or("")
        );
        let to_tag = extract_to_tag(&msg).expect("200 OK must carry To-tag");
        let body = split_body(&msg).to_owned();
        break (msg, to_tag, body);
    };

    // Parse the answer's crypto line.
    let sdp = SessionDescription::parse(&answer_body).expect("answer SDP parses");
    let m = sdp
        .media
        .iter()
        .find(|m| m.kind == MediaKind::Audio)
        .expect("answer has audio");
    assert_eq!(m.protocol, "RTP/SAVP", "answer must echo SAVP");
    assert_eq!(
        m.crypto.len(),
        1,
        "answer must carry one crypto line, got: {answer_body}",
    );
    let answer_crypto = &m.crypto[0];
    assert_eq!(answer_crypto.tag, offer_tag, "answer tag echoes offer tag");
    let engine_key = answer_crypto.key_material.clone();

    // c= + m.port = engine's RTP endpoint for this leg.
    let engine_rtp_ip = m
        .connection
        .as_ref()
        .map(|c| c.address)
        .or_else(|| sdp.connection.as_ref().map(|c| c.address))
        .expect("c= line present");
    let engine_rtp = SocketAddr::new(engine_rtp_ip, m.port);

    // ACK.
    let rv = RENDEZVOUS;
    let ack = format!(
        "ACK sip:{rv}@{engine} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {sip_addr};branch={branch}-ack;rport\r\n\
         From: Tester <sip:tester@{sip_addr}>;tag={from_tag}\r\n\
         To: Target <sip:{rv}@{engine}>;tag={to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 ACK\r\n\
         Max-Forwards: 70\r\n\
         Content-Length: 0\r\n\r\n",
    );
    sip.send_to(ack.as_bytes(), engine).await.unwrap();
    drop(resp);

    UaInvite {
        _sip: sip,
        rtp,
        engine_rtp,
        engine_local_tx_key: engine_key,
    }
}

fn extract_to_tag(msg: &str) -> Option<String> {
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

fn split_body(msg: &str) -> &str {
    if let Some(i) = msg.find("\r\n\r\n") {
        &msg[i + 4..]
    } else {
        ""
    }
}

/// Minimal RTP packet (V=2, no padding, no extension, no CSRCs).
fn rtp_packet(seq: u16, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.push(0x80); // V=2
    out.push(0); // PT=0 PCMU
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // timestamp
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn sdes_offer_answer_through_uas_encrypts_bridged_rtp() {
    // UA-A offers key 0x01..=0x1E (30 bytes).
    let offer_key_side_a: Vec<u8> = (1..=30u8).collect();
    // UA-B offers a distinct key (so we can't accidentally decrypt with
    // the wrong one and miss the bug).
    let offer_key_side_b: Vec<u8> = (60..90u8).collect();

    let engine = spawn_engine().await;

    // UA-A lands first → negotiator parks its leg.
    let ua_a = invite_savp(engine, 1, &offer_key_side_a).await;
    // UA-B lands second → bridge pairs.
    let ua_b = invite_savp(engine, 2, &offer_key_side_b).await;

    // Give the bridge forwarders a beat to spawn.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // UA-A encrypts an RTP packet with UA-A's offer key — that's what
    // it told the engine it would send under — and fires it at the
    // engine's leg-A port.
    let ua_a_tx = AesCmHmacSha1_80Transform::from_sdes(&offer_key_side_a).unwrap();
    let plaintext = rtp_packet(100, 0xABCD_1234, b"srtp-uas-test");
    let ciphertext = ua_a_tx.protect_rtp(&plaintext).unwrap();
    ua_a.rtp
        .send_to(&ciphertext, ua_a.engine_rtp)
        .await
        .unwrap();

    // UA-B should receive encrypted bytes at its RTP port. Decrypt with
    // the engine's answer key for UA-B (i.e. what the engine claimed in
    // UA-B's 200 OK).
    let mut buf = vec![0u8; 2048];
    let (n, _from) = timeout(Duration::from_secs(2), ua_b.rtp.recv_from(&mut buf))
        .await
        .expect("UA-B did not receive a packet through the SRTP bridge")
        .unwrap();
    assert!(
        n >= 12,
        "packet too short to be RTP/SRTP: {n} bytes (raw: {:02x?})",
        &buf[..n]
    );
    let received_ct = &buf[..n];

    let ua_b_rx = AesCmHmacSha1_80Transform::from_sdes(&ua_b.engine_local_tx_key).unwrap();
    let recovered = ua_b_rx
        .unprotect_rtp(received_ct)
        .expect("UA-B must decrypt the engine's re-encrypted packet");

    // Payload round-trips verbatim.
    assert_eq!(
        &recovered[12..],
        b"srtp-uas-test",
        "payload preserved end-to-end through SDES-negotiated bridge"
    );

    // SSRC was rewritten by the bridge — must differ from UA-A's original.
    let got_ssrc = u32::from_be_bytes([recovered[8], recovered[9], recovered[10], recovered[11]]);
    assert_ne!(
        got_ssrc, 0xABCD_1234,
        "bridge must rewrite SSRC (got original unchanged — SRTP leg skipped rewrite?)"
    );

    // And the engine must *not* have echoed either UA's offer key as its
    // own answer key — otherwise it would be leaking the peer's secret.
    assert_ne!(
        ua_a.engine_local_tx_key, offer_key_side_a,
        "engine answer key must not be UA-A's offer key"
    );
    assert_ne!(
        ua_b.engine_local_tx_key, offer_key_side_b,
        "engine answer key must not be UA-B's offer key"
    );
}
