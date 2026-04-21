//! Integration: RFC 4733 telephone-event stream → `UdpMediaFabric` →
//! event bus. Mirrors the end-to-end operator flow: DTMF presses
//! detected on the bridge land on `EventBus` as `SipEvent::Dtmf`
//! ready for MCP subscribers.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use smiths_core::media::{BridgeLeg, MediaFabric};
use smiths_core::{BusDtmfSink, Event, EventBus, RtpPacket, SipEvent};
use smiths_media::UdpMediaFabric;
use tokio::net::UdpSocket;

#[tokio::test(flavor = "multi_thread")]
async fn dtmf_through_fabric_lands_on_bus() {
    let bus = EventBus::new(32);
    let mut rx = bus.subscribe();

    let fabric = Arc::new(
        UdpMediaFabric::new()
            .with_dtmf_sink(Arc::new(BusDtmfSink::for_call(bus.clone(), "call-dtmf-42"))),
    );
    let ep_a = fabric
        .allocate(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .await
        .unwrap();
    let ep_b = fabric
        .allocate(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .await
        .unwrap();

    // Fake peers that "own" each leg's destination.
    let peer_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let leg_a_addr = peer_a.local_addr().unwrap();
    let leg_beta_addr = peer_b.local_addr().unwrap();

    let bridge_id = fabric
        .bridge(
            BridgeLeg {
                endpoint: ep_a.id(),
                peer: leg_a_addr,
                srtp: None,
            },
            BridgeLeg {
                endpoint: ep_b.id(),
                peer: leg_beta_addr,
                srtp: None,
            },
        )
        .await
        .unwrap();

    // Drive a '9' keypress onto engine's leg-A socket. The bridge
    // forwards toward B and should emit one keypress on the bus.
    let packets: Vec<RtpPacket> =
        smiths_core::dtmf::generate_keypress('9', 120, 0xDEAD_BEEF, 1, 42_000);
    for pkt in &packets {
        let bytes = pkt.encode();
        peer_a.send_to(&bytes, ep_a.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Subscriber: await the first DTMF event on the bus (within
    // 1 s — generous on loopback).
    let press = tokio::time::timeout(Duration::from_secs(1), async {
        while let Ok(ev) = rx.recv().await {
            if let Event::Sip(SipEvent::Dtmf { call_id, keypress }) = ev {
                return (call_id, keypress);
            }
        }
        panic!("bus closed before DTMF event");
    })
    .await
    .expect("no DTMF event on the bus within 1 s");

    assert_eq!(press.0.as_deref(), Some("call-dtmf-42"));
    assert_eq!(press.1.digit, '9');
    assert_eq!(press.1.duration_ms, 120);

    fabric.release_bridge(bridge_id).await;
}
