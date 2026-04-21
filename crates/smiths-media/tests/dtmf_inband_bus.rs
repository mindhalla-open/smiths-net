//! Integration: PSTN-style inband DTMF tones embedded in PCMU RTP
//! audio go through the bridge's Goertzel detector and land on the
//! event bus — same contract as the RFC 4733 path but opt-in.
//!
//! Slice 2.5 (v0.37.0) acceptance: legs that never negotiated RFC
//! 4733 still emit DTMF events to MCP subscribers.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use smiths_core::media::{BridgeLeg, MediaFabric};
use smiths_core::{
    BusDtmfSink, Event, EventBus, RtpPacket, SipEvent, pcm16_to_pcmu, synthesize_dtmf_tone,
};
use smiths_media::UdpMediaFabric;
use tokio::net::UdpSocket;

#[tokio::test(flavor = "multi_thread")]
async fn inband_tones_surface_on_the_bus() {
    let bus = EventBus::new(32);
    let mut rx = bus.subscribe();

    let fabric = Arc::new(
        UdpMediaFabric::new()
            .with_dtmf_sink(Arc::new(BusDtmfSink::for_call(bus.clone(), "call-inband")))
            .with_inband_dtmf(true),
    );
    let ep_a = fabric
        .allocate(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .await
        .unwrap();
    let ep_b = fabric
        .allocate(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .await
        .unwrap();

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

    // Synthesize a 120 ms '3' tone followed by 80 ms silence, all
    // PCMU, packetized into 20 ms RTP frames (160 samples each).
    let tone_pcm = synthesize_dtmf_tone('3', 120, 8_000);
    let silence_pcm = vec![0i16; 8_000 * 80 / 1_000];
    let mut combined = tone_pcm;
    combined.extend_from_slice(&silence_pcm);
    let pcmu = pcm16_to_pcmu(&combined);

    let mut seq: u16 = 1000;
    let mut ts: u32 = 0;
    for chunk in pcmu.chunks(160) {
        let pkt = RtpPacket {
            marker: seq == 1000,
            payload_type: 0, // PCMU
            sequence: seq,
            timestamp: ts,
            ssrc: 0xCAFE_F00D,
            payload: chunk.to_vec(),
        };
        peer_a
            .send_to(&pkt.encode(), ep_a.local_addr())
            .await
            .unwrap();
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(160);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let press = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(ev) = rx.recv().await {
            if let Event::Sip(SipEvent::Dtmf { call_id, keypress }) = ev {
                return (call_id, keypress);
            }
        }
        panic!("bus closed before inband DTMF event");
    })
    .await
    .expect("no inband DTMF event on the bus within 2 s");

    assert_eq!(press.0.as_deref(), Some("call-inband"));
    assert_eq!(press.1.digit, '3');
    // Duration is measured in whole 20 ms frames — at least 100 ms
    // (5 frames of tone) detected, no more than the full tone + a
    // few frames of slack.
    assert!(
        press.1.duration_ms >= 100 && press.1.duration_ms <= 200,
        "duration_ms out of range: {}",
        press.1.duration_ms
    );

    fabric.release_bridge(bridge_id).await;
}
