//! Integration: an RFC 4733 telephone-event stream driven at leg A
//! of a live bridge produces exactly one `DtmfKeypress` via the
//! configured `DtmfSink`.
//!
//! Slice 2.4 (v0.36.0) acceptance — closes the "engine sees DTMF"
//! half of P7. Plugin-side emission is a thin follow-on that wires
//! the same sink into the WASM tier.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use smiths_core::media::BridgeId;
use smiths_core::{DtmfKeypress, RtpPacket};
use smiths_media::bridge::{Bridge, BridgeConfig, DtmfSink, Leg};
use smiths_media::port_allocator::{DEFAULT_MAX_ATTEMPTS, allocate_rtp_rtcp_pair};
use tokio::net::UdpSocket;

#[derive(Default, Clone)]
struct Collect {
    presses: Arc<Mutex<Vec<(String, DtmfKeypress)>>>,
}

impl DtmfSink for Collect {
    fn deliver(&self, leg: &'static str, keypress: DtmfKeypress) {
        self.presses
            .lock()
            .unwrap()
            .push((leg.to_owned(), keypress));
    }
}

async fn peer_socket() -> (Arc<UdpSocket>, std::net::SocketAddr) {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let a = s.local_addr().unwrap();
    (Arc::new(s), a)
}

#[tokio::test(flavor = "multi_thread")]
async fn dtmf_stream_on_leg_a_emits_one_keypress() {
    // Engine's two RTP endpoints. `PortPair` returns the raw
    // `UdpSocket` values — wrap in `Arc` for the bridge API.
    let engine_a = allocate_rtp_rtcp_pair("127.0.0.1".parse().unwrap(), DEFAULT_MAX_ATTEMPTS)
        .await
        .unwrap();
    let engine_b = allocate_rtp_rtcp_pair("127.0.0.1".parse().unwrap(), DEFAULT_MAX_ATTEMPTS)
        .await
        .unwrap();
    let engine_a_addr = engine_a.rtp_addr;
    let engine_a_rtp = Arc::new(engine_a.rtp);
    let engine_side_b = Arc::new(engine_b.rtp);

    // Fake peers on each side.
    let (peer_a_sock, leg_a_addr) = peer_socket().await;
    let (_peer_b_sock, leg_beta_addr) = peer_socket().await;

    let sink = Collect::default();
    let bridge = Bridge::spawn_with(
        BridgeId(42),
        &Leg {
            socket: Arc::clone(&engine_a_rtp),
            peer: leg_a_addr,
            rtcp: None,
            srtp: None,
        },
        &Leg {
            socket: Arc::clone(&engine_side_b),
            peer: leg_beta_addr,
            rtcp: None,
            srtp: None,
        },
        &BridgeConfig {
            rtcp_interval: None,
            metrics: None,
            dtmf_sink: Some(Arc::new(sink.clone())),
            inband_dtmf: false,
        },
    );

    // Generate a 160 ms '7' keypress and shovel it at engine-A's RTP
    // socket from peer A. Each packet pacing 20 ms.
    let packets: Vec<RtpPacket> =
        smiths_core::dtmf::generate_keypress('7', 160, 0xABCD_EF01, 50, 5_000);
    for pkt in &packets {
        let bytes = pkt.encode();
        peer_a_sock.send_to(&bytes, engine_a_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Let the bridge forwarder drain the last packet.
    tokio::time::sleep(Duration::from_millis(50)).await;

    bridge.shutdown().await;

    let presses = sink.presses.lock().unwrap().clone();
    assert_eq!(presses.len(), 1, "one press expected, got {presses:?}");
    let (dir, press) = &presses[0];
    assert_eq!(dir, "a->b", "press arrived on leg A → bridged toward B");
    assert_eq!(press.digit, '7');
    assert_eq!(press.duration_ms, 160);
}
