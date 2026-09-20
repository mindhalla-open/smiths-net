// Integration-test scope: the test composes an RFC 8489 STUN
// message by hand + pokes the wire fields directly. Pedantic
// truncation / lifetime / too-many-lines lints all fire on the
// intentionally byte-oriented code. Suppress at file scope;
// `unused_variables` only bites because two recv returns are
// bound for doc-clarity.
#![allow(
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::duration_suboptimal_units,
    clippy::implicit_clone,
    clippy::needless_lifetimes,
    clippy::same_item_push,
    clippy::doc_markdown,
    unused_variables
)]

//! End-to-end integration test for the embedded TURN server
//! (slice 5.11-turn Small 2).
//!
//! Drives the Allocate (401 → auth) / CreatePermission /
//! ChannelBind / ChannelData / Data-indication flow against a
//! real UDP peer, proving that data tunnels through the relay
//! socket and the metric labels land correctly.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use smiths_core::Metrics;
use smiths_core::metrics::TurnAllocationOutcomeLabel;
use smiths_ice::turn::{
    ATTR_CHANNEL_NUMBER, ATTR_DATA, ATTR_LIFETIME, ATTR_MESSAGE_INTEGRITY, ATTR_NONCE, ATTR_REALM,
    ATTR_REQUESTED_TRANSPORT, ATTR_USERNAME, ATTR_XOR_PEER_ADDRESS, ATTR_XOR_RELAYED_ADDRESS,
    METHOD_ALLOCATE, METHOD_CHANNEL_BIND, METHOD_CREATE_PERMISSION, METHOD_SEND,
};
use smiths_ice::{LongTermCredential, TurnServer, TurnServerConfig};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// Encode a STUN request with the given method + attributes.
/// Pads attribute values to 4-byte boundaries.
fn stun_request(method: u16, txid: [u8; 12], attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let type_field: u16 = ((method & 0x0F00) << 2) | ((method & 0x0070) << 1) | (method & 0x000F);
    let mut body = Vec::new();
    for (kind, value) in attrs {
        body.extend_from_slice(&kind.to_be_bytes());
        body.extend_from_slice(&(value.len() as u16).to_be_bytes());
        body.extend_from_slice(value);
        let pad = (4 - (value.len() % 4)) % 4;
        for _ in 0..pad {
            body.push(0);
        }
    }
    let mut msg = Vec::with_capacity(20 + body.len());
    msg.extend_from_slice(&type_field.to_be_bytes());
    msg.extend_from_slice(&(body.len() as u16).to_be_bytes());
    msg.extend_from_slice(&0x2112_A442u32.to_be_bytes());
    msg.extend_from_slice(&txid);
    msg.extend_from_slice(&body);
    msg
}

/// Append a MESSAGE-INTEGRITY attribute over the whole message
/// so far, using the same HMAC-SHA-1 the server expects.
fn add_message_integrity(msg: &mut Vec<u8>, key: &[u8; 16]) {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    // Length field in the header must reflect the body with the
    // about-to-be-added MESSAGE-INTEGRITY attribute (4-byte
    // header + 20-byte HMAC = 24 bytes).
    let final_len = (msg.len() + 4 + 20 - 20) as u16;
    msg[2] = (final_len >> 8) as u8;
    msg[3] = (final_len & 0xFF) as u8;
    let mut mac = Hmac::<Sha1>::new_from_slice(key).unwrap();
    mac.update(msg);
    let hmac = mac.finalize().into_bytes();
    msg.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
    msg.extend_from_slice(&20u16.to_be_bytes());
    msg.extend_from_slice(&hmac);
}

/// XOR-PEER-ADDRESS / XOR-RELAYED-ADDRESS encode for IPv4.
fn xor_addr_attr_v4(addr: SocketAddr) -> Vec<u8> {
    let SocketAddr::V4(v4) = addr else {
        unreachable!("ipv4-only test");
    };
    let port_xor = v4.port() ^ (0x2112_A442u32 >> 16) as u16;
    let ip_xor = u32::from(*v4.ip()) ^ 0x2112_A442u32;
    let mut out = vec![0u8, 0x01];
    out.extend_from_slice(&port_xor.to_be_bytes());
    out.extend_from_slice(&ip_xor.to_be_bytes());
    out
}

fn decode_xor_addr_v4(raw: &[u8]) -> SocketAddr {
    assert_eq!(raw[1], 0x01, "IPv4 family expected");
    let port_xor = u16::from_be_bytes([raw[2], raw[3]]);
    let port = port_xor ^ (0x2112_A442u32 >> 16) as u16;
    let ip_xor = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let ip = ip_xor ^ 0x2112_A442u32;
    SocketAddr::V4(std::net::SocketAddrV4::new(Ipv4Addr::from(ip), port))
}

/// Walk a parsed STUN body looking for `kind`.
fn find_attr<'a>(body: &'a [u8], kind: u16) -> Option<&'a [u8]> {
    let mut idx = 0;
    while idx + 4 <= body.len() {
        let k = u16::from_be_bytes([body[idx], body[idx + 1]]);
        let len = u16::from_be_bytes([body[idx + 2], body[idx + 3]]) as usize;
        idx += 4;
        if idx + len > body.len() {
            return None;
        }
        if k == kind {
            return Some(&body[idx..idx + len]);
        }
        let pad = (4 - (len % 4)) % 4;
        idx += len + pad;
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn full_turn_flow_relay_then_channel_bind_then_data_echo() {
    // Boot the server on a loopback ephemeral port.
    let metrics = {
        let mut scratch = prometheus_client::registry::Registry::default();
        Metrics::register(&mut scratch)
    };
    let cred = LongTermCredential::new("alice", "smiths-turn", "open-sesame");
    let cfg = TurnServerConfig {
        bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        realm: "smiths-turn".into(),
        relay_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        allocation_lifetime: Duration::from_secs(120),
        credentials: vec![cred.clone()],
    };
    // Resolve the bind port before spawning `run` (which blocks
    // on `UdpSocket::bind`) by binding once ourselves, grabbing
    // the port, closing it, and re-using the same port in the
    // actual server. Tokio doesn't guarantee the port stays
    // free, but in practice the window is microseconds and the
    // test is single-threaded enough.
    let probe = UdpSocket::bind(cfg.bind).await.unwrap();
    let server_addr = probe.local_addr().unwrap();
    drop(probe);
    let mut cfg = cfg;
    cfg.bind = server_addr;
    let cancel = CancellationToken::new();
    let server = Arc::new(TurnServer::new(cfg).with_metrics(Arc::clone(&metrics)));
    let server_run = Arc::clone(&server);
    let cancel_task = cancel.clone();
    let server_handle = tokio::spawn(async move { server_run.run(cancel_task).await });
    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // --- Client side: open a UDP socket that talks to the server ---
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(server_addr).await.unwrap();

    // --- Step 1: unauthenticated Allocate → 401 challenge ---
    let txid = [1u8; 12];
    let attrs = vec![(ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0])];
    let msg = stun_request(METHOD_ALLOCATE, txid, &attrs);
    client.send(&msg).await.unwrap();
    let mut buf = vec![0u8; 2048];
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("challenge timed out")
        .unwrap();
    assert!(n >= 20);
    // Extract realm + nonce from the challenge body.
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let (realm, nonce) = {
        let body = &buf[20..20 + body_len];
        let realm = find_attr(body, ATTR_REALM).expect("realm attr").to_vec();
        let nonce = find_attr(body, ATTR_NONCE).expect("nonce attr").to_vec();
        (realm, nonce)
    };
    assert_eq!(realm, b"smiths-turn");
    assert!(!nonce.is_empty());

    // --- Step 2: re-Allocate with USERNAME + MESSAGE-INTEGRITY ---
    let txid = [2u8; 12];
    let mut msg = stun_request(
        METHOD_ALLOCATE,
        txid,
        &[
            (ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0]),
            (ATTR_USERNAME, b"alice".to_vec()),
            (ATTR_REALM, b"smiths-turn".to_vec()),
            (ATTR_NONCE, nonce.to_vec()),
        ],
    );
    add_message_integrity(&mut msg, &cred.long_term_key);
    client.send(&msg).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("allocate response timed out")
        .unwrap();
    assert!(n >= 20);
    // Type: success response (class 10) for method Allocate → 0x0103.
    let ty = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(ty, 0x0103, "expected Allocate Success Response");
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let (relay_addr, lifetime) = {
        let body = &buf[20..20 + body_len];
        let relayed = find_attr(body, ATTR_XOR_RELAYED_ADDRESS).expect("XOR-RELAYED-ADDRESS");
        let addr = decode_xor_addr_v4(relayed);
        let lt_raw = find_attr(body, ATTR_LIFETIME).expect("LIFETIME");
        let lt = u32::from_be_bytes([lt_raw[0], lt_raw[1], lt_raw[2], lt_raw[3]]);
        (addr, lt)
    };
    assert!(lifetime > 0);

    // --- Step 3: spin up a peer that listens on another UDP socket ---
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();

    // --- Step 4: CreatePermission for the peer ---
    let txid = [3u8; 12];
    let mut msg = stun_request(
        METHOD_CREATE_PERMISSION,
        txid,
        &[
            (ATTR_XOR_PEER_ADDRESS, xor_addr_attr_v4(peer_addr)),
            (ATTR_USERNAME, b"alice".to_vec()),
            (ATTR_REALM, b"smiths-turn".to_vec()),
            (ATTR_NONCE, nonce.to_vec()),
        ],
    );
    add_message_integrity(&mut msg, &cred.long_term_key);
    client.send(&msg).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("CreatePermission response timed out")
        .unwrap();
    let ty = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(ty, 0x0108, "expected CreatePermission success");
    let _ = n;

    // --- Step 5: Send indication → peer receives the payload ---
    let payload = b"hello turn";
    let txid = [4u8; 12];
    let send_msg = stun_request(
        METHOD_SEND,
        txid,
        &[
            (ATTR_XOR_PEER_ADDRESS, xor_addr_attr_v4(peer_addr)),
            (ATTR_DATA, payload.to_vec()),
        ],
    );
    // Set class=01 (indication) on the type field.
    let mut indication = send_msg.clone();
    indication[0] = 0x00;
    indication[1] = 0x16; // 0x0016 = Send Indication (class 01, method 006)
    client.send(&indication).await.unwrap();

    let mut peer_buf = vec![0u8; 2048];
    let (n, from) = timeout(Duration::from_secs(2), peer.recv_from(&mut peer_buf))
        .await
        .expect("peer didn't receive relayed data")
        .unwrap();
    assert_eq!(&peer_buf[..n], payload);
    // `from` is the relay address — port is dynamic but IP
    // matches our configured relay_ip.
    assert_eq!(from.ip(), relay_addr.ip());
    let relay_source = from;

    // --- Step 6: peer replies → client receives Data indication ---
    peer.send_to(b"pong", relay_source).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("client didn't receive Data indication")
        .unwrap();
    // Type: Data Indication = 0x0017 (class 01, method 0x007).
    let ty = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(ty, 0x0017);
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    {
        let body = &buf[20..20 + body_len];
        let data = find_attr(body, ATTR_DATA).expect("DATA attr");
        assert_eq!(data, b"pong");
    }

    // --- Step 7: ChannelBind + ChannelData round trip ---
    let channel: u16 = 0x4000;
    let txid = [5u8; 12];
    let mut msg = stun_request(
        METHOD_CHANNEL_BIND,
        txid,
        &[
            (
                ATTR_CHANNEL_NUMBER,
                vec![(channel >> 8) as u8, (channel & 0xFF) as u8, 0, 0],
            ),
            (ATTR_XOR_PEER_ADDRESS, xor_addr_attr_v4(peer_addr)),
            (ATTR_USERNAME, b"alice".to_vec()),
            (ATTR_REALM, b"smiths-turn".to_vec()),
            (ATTR_NONCE, nonce.to_vec()),
        ],
    );
    add_message_integrity(&mut msg, &cred.long_term_key);
    client.send(&msg).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("ChannelBind response timed out")
        .unwrap();
    let ty = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(ty, 0x0109, "expected ChannelBind success");
    let _ = n;

    // Send ChannelData frame (client → server → peer).
    let body = b"fast-path frame";
    let mut frame = Vec::new();
    frame.extend_from_slice(&channel.to_be_bytes());
    frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
    frame.extend_from_slice(body);
    client.send(&frame).await.unwrap();
    let (n, _) = timeout(Duration::from_secs(2), peer.recv_from(&mut peer_buf))
        .await
        .expect("peer didn't receive ChannelData payload")
        .unwrap();
    assert_eq!(&peer_buf[..n], body);

    // Peer replies via channel.
    peer.send_to(b"fast-path reply", relay_source)
        .await
        .unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("client didn't receive channel reply")
        .unwrap();
    // Channel-bound peer: server wraps reply as ChannelData,
    // NOT Data indication. First byte's top two bits = 0b01.
    assert_eq!(buf[0] & 0xC0, 0x40);
    let chan = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(chan, channel);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    assert_eq!(&buf[4..4 + len], b"fast-path reply");

    // Metrics sanity: the `success` counter bumped on Allocate,
    // `challenged` on the 401 round.
    let success = metrics
        .turn_allocations
        .get_or_create(&TurnAllocationOutcomeLabel {
            outcome: "success".into(),
        })
        .get();
    assert_eq!(success, 1, "exactly one successful Allocate this test");
    let challenged = metrics
        .turn_allocations
        .get_or_create(&TurnAllocationOutcomeLabel {
            outcome: "challenged".into(),
        })
        .get();
    assert_eq!(challenged, 1, "one 401 challenge before the auth retry");
    let active = metrics.turn_active_allocations.get();
    assert_eq!(active, 1, "one live allocation");

    // Shut down.
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

// ---------------------------------------------------------------------
// Expiry, error classes and the TurnClient round trip
// ---------------------------------------------------------------------

use smiths_ice::turn::{METHOD_REFRESH, Relayed};
use smiths_ice::{TurnClient, TurnClientError};

/// Boot a server on an ephemeral loopback port with the given
/// allocation lifetime; returns `(server_addr, credential, cancel,
/// metrics)`.
async fn boot_server(
    lifetime: Duration,
) -> (
    SocketAddr,
    LongTermCredential,
    CancellationToken,
    Arc<Metrics>,
    Arc<TurnServer>,
) {
    let metrics = {
        let mut scratch = prometheus_client::registry::Registry::default();
        Metrics::register(&mut scratch)
    };
    let cred = LongTermCredential::new("alice", "smiths-turn", "open-sesame");
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_addr = probe.local_addr().unwrap();
    drop(probe);
    let cfg = TurnServerConfig {
        bind: server_addr,
        realm: "smiths-turn".into(),
        relay_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        allocation_lifetime: lifetime,
        credentials: vec![cred.clone()],
    };
    let cancel = CancellationToken::new();
    let server = Arc::new(TurnServer::new(cfg).with_metrics(Arc::clone(&metrics)));
    let server_run = Arc::clone(&server);
    let cancel_task = cancel.clone();
    tokio::spawn(async move { server_run.run(cancel_task).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    (server_addr, cred, cancel, metrics, server)
}

#[tokio::test(flavor = "multi_thread")]
async fn error_replies_use_the_request_method_class() {
    let (server_addr, _cred, cancel, _metrics, _server) =
        boot_server(Duration::from_secs(60)).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(server_addr).await.unwrap();
    let mut buf = vec![0u8; 2048];

    // Refresh without an allocation: the server has to challenge
    // first (no MESSAGE-INTEGRITY) — and the challenge is typed as a
    // Refresh error response (0x0114), not an Allocate one.
    let msg = stun_request(
        METHOD_REFRESH,
        [7u8; 12],
        &[(ATTR_LIFETIME, vec![0, 0, 0, 30])],
    );
    client.send(&msg).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("refresh reply timed out")
        .unwrap();
    let ty = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(ty, 0x0114, "error response must carry the Refresh method");
    assert_eq!(&buf[8..20], &[7u8; 12], "transaction id echoed");

    // CreatePermission likewise → 0x0118.
    let msg = stun_request(METHOD_CREATE_PERMISSION, [8u8; 12], &[]);
    client.send(&msg).await.unwrap();
    let n2 = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("create-permission reply timed out")
        .unwrap();
    let ty = u16::from_be_bytes([buf[0], buf[1]]);
    assert_eq!(
        ty, 0x0118,
        "error response must carry the CreatePermission method"
    );
    let _ = (n, n2);

    // A nonce the server never issued → 438 Stale Nonce with a fresh one.
    let mut msg = stun_request(
        METHOD_ALLOCATE,
        [9u8; 12],
        &[
            (ATTR_REQUESTED_TRANSPORT, vec![17, 0, 0, 0]),
            (ATTR_USERNAME, b"alice".to_vec()),
            (ATTR_REALM, b"smiths-turn".to_vec()),
            (ATTR_NONCE, b"made-up-nonce".to_vec()),
        ],
    );
    add_message_integrity(
        &mut msg,
        &LongTermCredential::new("alice", "smiths-turn", "open-sesame").long_term_key,
    );
    client.send(&msg).await.unwrap();
    let n = timeout(Duration::from_secs(2), client.recv(&mut buf))
        .await
        .expect("stale-nonce reply timed out")
        .unwrap();
    let body_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let body = &buf[20..20 + body_len];
    let err = find_attr(body, 0x0009).expect("ERROR-CODE");
    assert_eq!((err[2], err[3]), (4, 38), "438 Stale Nonce");
    assert!(find_attr(body, ATTR_NONCE).is_some(), "fresh nonce offered");
    let _ = n;

    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_allocation_is_refused_and_reaped() {
    let (server_addr, cred, cancel, metrics, server) = boot_server(Duration::from_secs(1)).await;
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let mut client = TurnClient::allocate(
        Arc::clone(&socket),
        server_addr,
        cred,
        Duration::from_secs(2),
    )
    .await
    .expect("allocate");
    assert_eq!(
        client.lifetime(),
        Duration::from_secs(1),
        "capped by the server"
    );
    assert_eq!(server.active_allocations().await, 1);
    assert_eq!(metrics.turn_active_allocations.get(), 1);

    // Past the lifetime the allocation is gone on access...
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
    match client.create_permission(&[peer]).await {
        Err(TurnClientError::ErrorResponse { code: 437, .. }) => {}
        other => panic!("expected 437 Allocation Mismatch, got {other:?}"),
    }
    assert_eq!(server.active_allocations().await, 0);
    // ...and the sweep releases it (gauge back to zero).
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(metrics.turn_active_allocations.get(), 0);

    // A fresh Allocate on the same five-tuple succeeds again.
    let client2 = TurnClient::allocate(
        socket,
        server_addr,
        LongTermCredential::new("alice", "smiths-turn", "open-sesame"),
        Duration::from_secs(2),
    )
    .await
    .expect("re-allocate after expiry");
    assert_ne!(client2.relay_addr().port(), 0);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn client_refresh_permission_channel_round_trip() {
    let (server_addr, cred, cancel, metrics, _server) = boot_server(Duration::from_secs(120)).await;
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let mut client = TurnClient::allocate(
        Arc::clone(&socket),
        server_addr,
        cred,
        Duration::from_secs(2),
    )
    .await
    .expect("allocate");
    assert_eq!(client.relay_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(client.lifetime(), Duration::from_secs(120));

    // Refresh trims the lifetime to what we ask for.
    let granted = client
        .refresh(Duration::from_secs(30))
        .await
        .expect("refresh");
    assert_eq!(granted, Duration::from_secs(30));

    // Peer on its own socket.
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let mut peer_buf = vec![0u8; 2048];

    // Without a permission the relay drops our data.
    client.send_indication(peer_addr, b"dropped").await.unwrap();
    assert!(
        timeout(Duration::from_millis(200), peer.recv_from(&mut peer_buf))
            .await
            .is_err(),
        "no permission → nothing relayed"
    );

    client
        .create_permission(&[peer_addr])
        .await
        .expect("permission");
    client
        .send_indication(peer_addr, b"hello via relay")
        .await
        .unwrap();
    let (n, from) = timeout(Duration::from_secs(2), peer.recv_from(&mut peer_buf))
        .await
        .expect("peer never got the relayed data")
        .unwrap();
    assert_eq!(&peer_buf[..n], b"hello via relay");
    assert_eq!(from, client.relay_addr());

    // Peer → relay → client arrives as a Data indication.
    peer.send_to(b"pong", client.relay_addr()).await.unwrap();
    assert_eq!(
        client.recv().await.expect("data indication"),
        Relayed::Data {
            peer: peer_addr,
            data: b"pong".to_vec()
        }
    );

    // Channel binding switches both directions to ChannelData.
    client
        .channel_bind(0x4000, peer_addr)
        .await
        .expect("channel bind");
    client.send_channel(0x4000, b"fast path").await.unwrap();
    let (n, _) = timeout(Duration::from_secs(2), peer.recv_from(&mut peer_buf))
        .await
        .expect("peer never got the channel data")
        .unwrap();
    assert_eq!(&peer_buf[..n], b"fast path");
    peer.send_to(b"fast reply", client.relay_addr())
        .await
        .unwrap();
    assert_eq!(
        client.recv().await.expect("channel data"),
        Relayed::Channel {
            channel: 0x4000,
            data: b"fast reply".to_vec()
        }
    );
    // Rebinding the channel to another peer is refused.
    let other: SocketAddr = "127.0.0.1:65000".parse().unwrap();
    match client.channel_bind(0x4000, other).await {
        Err(TurnClientError::ErrorResponse { code: 400, .. }) => {}
        other => panic!("expected 400 on conflicting ChannelBind, got {other:?}"),
    }

    // Releasing with lifetime 0 tears the allocation down.
    let granted = client.refresh(Duration::ZERO).await.expect("release");
    assert_eq!(granted, Duration::ZERO);
    assert_eq!(metrics.turn_active_allocations.get(), 0);
    match client.create_permission(&[peer_addr]).await {
        Err(TurnClientError::ErrorResponse { code: 437, .. }) => {}
        other => panic!("expected 437 after release, got {other:?}"),
    }
    cancel.cancel();
}
