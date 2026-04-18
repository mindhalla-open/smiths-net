//! End-to-end: PCMU bridge preserves payload **and** rewrites SSRC.
//!
//! Two UACs INVITE the engine with the same rendezvous key. UA-A sends
//! a short μ-law RTP stream with a known SSRC. UA-B receives through
//! the engine and we verify:
//!
//! 1. Payload bytes are preserved byte-for-byte.
//! 2. The SSRC seen by UA-B is **different** from the SSRC UA-A set
//!    (guardrail against accidentally going back to the byte-opaque
//!    bridge).
//! 3. The engine-rewritten SSRC is stable across packets in one leg
//!    (no packet-by-packet churn).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::Transport as _;
use smiths_sip::{UasServer, UdpTransport};
use smiths_testkit::FakeUac;
use smiths_testkit::codec::pcm16_to_pcmu;
use smiths_testkit::rtp::RtpPacket;
use smiths_testkit::signal::sine_wave;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

const PT_PCMU: u8 = 0;
const SAMPLES_PER_FRAME: usize = 160; // 20 ms at 8 kHz
const FRAMES: usize = 25; // half a second is enough for SSRC checks
const RENDEZVOUS: &str = "g711-ssrc";
const UA_A_SSRC: u32 = 0x1234_5678;

async fn start_engine() -> (SocketAddr, CancellationToken, JoinHandle<()>) {
    let t = UdpTransport::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = t.local_addr().unwrap();
    let transport = Arc::new(t);
    let bus = EventBus::new(32);
    let cancel = CancellationToken::new();
    let (tx, rx) = mpsc::channel(64);
    transport.spawn_reader(tx, cancel.clone());
    let fabric: Arc<dyn MediaFabric> = Arc::new(UdpMediaFabric::new());
    let negotiator: Arc<dyn SdpNegotiator> = Arc::new(Negotiator::with_default_codecs(addr.ip()));
    let server = UasServer::new(Arc::clone(&transport), bus, fabric, negotiator).unwrap();
    let handle = tokio::spawn(server.run(rx, cancel.clone()));
    (addr, cancel, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn g711_bridge_preserves_payload_and_rewrites_ssrc() {
    let source_pcm = sine_wave(1_000.0, 0.5, 8_000, 10_000);
    let wire_bytes = pcm16_to_pcmu(&source_pcm);
    assert_eq!(wire_bytes.len(), SAMPLES_PER_FRAME * FRAMES);

    let (engine_addr, cancel, engine_task) = start_engine().await;

    let mut ua_a = FakeUac::bind(engine_addr).await.unwrap();
    let mut ua_b = FakeUac::bind(engine_addr).await.unwrap();

    let (inv_a, inv_b) = tokio::join!(ua_a.invite(RENDEZVOUS), ua_b.invite(RENDEZVOUS));
    inv_a.expect("UA-A INVITE");
    inv_b.expect("UA-B INVITE");
    let rtp_target_a = ua_a.engine_rtp.expect("UA-A got SDP answer");

    // Give the bridge a moment to be fully up on both sides.
    sleep(Duration::from_millis(50)).await;

    // UA-B collector — captures RTP packets and the SSRCs observed.
    let collector = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut packets: Vec<RtpPacket> = Vec::with_capacity(FRAMES);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut quiet_deadline: Option<Instant> = None;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let next_deadline = quiet_deadline.unwrap_or(deadline).min(deadline);
            let remaining = next_deadline.saturating_duration_since(now);
            match timeout(remaining, ua_b.rtp.recv_from(&mut buf)).await {
                Ok(Ok((n, _src))) => {
                    if let Some(pkt) = RtpPacket::decode(&buf[..n]) {
                        packets.push(pkt);
                    }
                    quiet_deadline = Some(Instant::now() + Duration::from_millis(200));
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        (packets, ua_b)
    });

    // UA-A sender: `FRAMES` frames of μ-law with UA_A_SSRC.
    let mut seq: u16 = 1000;
    let mut ts: u32 = 0;
    for (i, chunk) in wire_bytes.chunks(SAMPLES_PER_FRAME).enumerate() {
        let pkt = RtpPacket {
            marker: i == 0,
            payload_type: PT_PCMU,
            sequence: seq,
            timestamp: ts,
            ssrc: UA_A_SSRC,
            payload: chunk.to_vec(),
        };
        ua_a.rtp.send_to(&pkt.encode(), rtp_target_a).await.unwrap();
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(u32::try_from(SAMPLES_PER_FRAME).unwrap());
        sleep(Duration::from_millis(20)).await;
    }

    let (packets, mut ua_b) = collector.await.expect("collector joined");
    assert!(
        packets.len() >= FRAMES - 2,
        "UA-B received only {} of {FRAMES} RTP frames",
        packets.len()
    );

    // Guardrail 1: every packet UA-B sees has an SSRC DIFFERENT from
    // what UA-A sent. This is the invariant that catches a byte-
    // transparent bridge regression.
    for (i, pkt) in packets.iter().enumerate() {
        assert_ne!(
            pkt.ssrc, UA_A_SSRC,
            "packet {i}: SSRC leaked through the bridge unchanged"
        );
    }

    // Guardrail 2: SSRC is stable across the whole leg. The engine
    // must pick one SSRC per outbound direction and stick to it.
    let engine_ssrc = packets[0].ssrc;
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(
            pkt.ssrc, engine_ssrc,
            "packet {i}: SSRC changed mid-stream (was {engine_ssrc:#x}, got {:#x})",
            pkt.ssrc
        );
    }

    // Guardrail 3: payload bytes round-trip. Strip two frames on each
    // end to absorb any startup/shutdown packets the collector may
    // have missed.
    let received: Vec<u8> = packets.into_iter().flat_map(|p| p.payload).collect();
    assert!(received.len() >= SAMPLES_PER_FRAME * (FRAMES - 2));
    let tail_len = SAMPLES_PER_FRAME * (FRAMES - 4);
    let rx_tail = &received[received.len() - tail_len..];
    let tx_tail = &wire_bytes[wire_bytes.len() - tail_len..];
    assert_eq!(rx_tail, tx_tail, "μ-law payload tail differs after bridge");

    ua_a.bye(RENDEZVOUS).await.expect("UA-A BYE");
    ua_b.bye(RENDEZVOUS).await.expect("UA-B BYE");
    cancel.cancel();
    let _ = timeout(Duration::from_secs(2), engine_task).await;
}
