//! End-to-end reference test for the T.38 relay.
//!
//! Simulates the happy path that slice 5.4's acceptance criterion
//! describes:
//!
//! 1. Two peers (acting as fax terminals) bind UDP sockets and
//!    advertise them through SDP.
//! 2. The engine constructs a `UdptlSession` relaying between them.
//! 3. Peer A emits a 20-packet UDPTL stream shaped like what spandsp
//!    would send for a real fax page (primary IFP bytes + 2-deep
//!    redundancy copies of the previous two primaries).
//! 4. Peer B receives every packet byte-identity; sequence
//!    numbers arrive strictly increasing; redundancy copies are
//!    preserved so peer B could reconstruct a dropped primary.
//!
//! Why no spandsp FFI: it'd import `unsafe` into a workspace that
//! forbids it, and the shape of the bytes peer-B actually receives
//! doesn't depend on whether the fax state machine produced them or
//! a fixture script did. The relay is a bytes-mover; that's what
//! this test exercises.

#![allow(clippy::similar_names)] // leg_a / leg_b / peer_a / peer_b are load-bearing

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::media::MediaSession;
use smiths_fax::{UdptlPacket, UdptlSession, UdptlSessionConfig};
use tokio::net::UdpSocket;
use tokio::time::timeout;

/// Builds a synthetic T.38 stream shaped like a real fax page.
/// Primary payload changes per packet; secondary carries the
/// previous two primaries so a single loss is recoverable.
fn synthetic_page(count: u16) -> Vec<UdptlPacket> {
    let mut out = Vec::with_capacity(count as usize);
    let mut prev: Vec<Vec<u8>> = Vec::new();
    for seq in 0..count {
        let primary = format!("IFP#{seq}: V.17 page data {seq:04}").into_bytes();
        let secondary = prev.iter().rev().take(2).cloned().collect();
        out.push(UdptlPacket {
            sequence: seq,
            primary: primary.clone(),
            secondary,
        });
        prev.push(primary);
    }
    out
}

async fn bind() -> (Arc<UdpSocket>, SocketAddr) {
    let s = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = s.local_addr().unwrap();
    (s, addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_page_relays_byte_identity_with_sequence_preserved() {
    let (leg_a, leg_a_addr) = bind().await;
    let (leg_b, leg_b_addr) = bind().await;
    let (peer_a, peer_a_addr) = bind().await;
    let (peer_b, peer_b_addr) = bind().await;

    let session = UdptlSession::spawn(
        UdptlSessionConfig {
            trace_sequence: true,
            ..UdptlSessionConfig::default()
        },
        (Arc::clone(&leg_a), peer_a_addr),
        (Arc::clone(&leg_b), peer_b_addr),
    );

    let page = synthetic_page(20);

    // Peer A emits the whole page in order, with a small pacing
    // gap so the single-threaded recv loop drains between sends.
    for pkt in &page {
        let wire = pkt.encode().unwrap();
        peer_a.send_to(&wire, leg_a_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Peer B collects every packet and reparses.
    let mut received: Vec<UdptlPacket> = Vec::with_capacity(page.len());
    let mut buf = [0_u8; 1500];
    for _ in 0..page.len() {
        let (n, from) = timeout(Duration::from_secs(2), peer_b.recv_from(&mut buf))
            .await
            .expect("recv timed out")
            .unwrap();
        assert_eq!(from, leg_b_addr, "datagram came from the wrong leg");
        received.push(UdptlPacket::parse(&buf[..n]).unwrap());
    }

    // Byte-identity: every parsed packet round-trips through
    // encode() to exactly what peer A sent.
    for (got, expected) in received.iter().zip(page.iter()) {
        assert_eq!(got, expected, "udptl packet mutated on the wire");
    }

    // Sequence strictly increasing.
    for window in received.windows(2) {
        assert_eq!(window[1].sequence, window[0].sequence + 1);
    }

    // Redundancy preserved: the last packet's secondary should
    // still contain copies of the two prior primaries.
    let last = received.last().unwrap();
    assert_eq!(last.secondary.len(), 2);
    assert_eq!(last.secondary[0], page[page.len() - 2].primary);
    assert_eq!(last.secondary[1], page[page.len() - 3].primary);

    session.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reverse_direction_also_relays() {
    // Fax protocols are bidirectional (TCF training + MCF ack fly
    // the other way). Verify B→A also passes through.
    let (leg_a, leg_a_addr) = bind().await;
    let (leg_b, leg_b_addr) = bind().await;
    let (peer_a, peer_a_addr) = bind().await;
    let (peer_b, peer_b_addr) = bind().await;

    let session = UdptlSession::spawn(
        UdptlSessionConfig::default(),
        (Arc::clone(&leg_a), peer_a_addr),
        (Arc::clone(&leg_b), peer_b_addr),
    );

    let mcf = UdptlPacket {
        sequence: 1,
        primary: b"MCF: message confirmation".to_vec(),
        secondary: vec![],
    };
    peer_b
        .send_to(&mcf.encode().unwrap(), leg_b_addr)
        .await
        .unwrap();

    let mut buf = [0_u8; 1500];
    let (n, from) = timeout(Duration::from_secs(1), peer_a.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(from, leg_a_addr);
    assert_eq!(UdptlPacket::parse(&buf[..n]).unwrap(), mcf);

    session.stop().await;
}
