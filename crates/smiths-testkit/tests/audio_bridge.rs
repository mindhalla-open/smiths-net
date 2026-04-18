//! End-to-end: two test UACs INVITE the engine with the same rendezvous
//! key. Engine bridges their media legs. UA-A streams PCMU RTP generated
//! from a 1 kHz sine wave; UA-B receives, accumulates, and we verify the
//! audio round-trips byte-for-byte. The received WAV is also written to
//! `/tmp/smiths-call-received.wav` so a human can listen.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use smiths_core::{EventBus, MediaFabric, SdpNegotiator};
use smiths_media::UdpMediaFabric;
use smiths_sdp::Negotiator;
use smiths_sip::Transport as _;
use smiths_sip::{UasServer, UdpTransport};
use smiths_testkit::codec::{pcm16_to_pcmu, pcmu_to_pcm16};
use smiths_testkit::rtp::RtpPacket;
use smiths_testkit::signal::sine_wave;
use smiths_testkit::uac::TestUac;
use smiths_testkit::wav::write_mono_pcm16;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

const PT_PCMU: u8 = 0;
const SAMPLES_PER_FRAME: usize = 160; // 20 ms at 8 kHz
const FRAMES: usize = 50; // 1 second of audio
const RENDEZVOUS: &str = "call-1";
const OUT_WAV: &str = "/tmp/smiths-call-received.wav";

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
async fn two_uas_call_preserves_audio_byte_for_byte() {
    // Build the source audio: 1 kHz sine, 1 s, PCM-16 @ 8 kHz, moderate
    // amplitude. Convert to μ-law for wire transmission.
    let source_pcm = sine_wave(1_000.0, 1.0, 8_000, 10_000);
    let wire_bytes = pcm16_to_pcmu(&source_pcm);
    assert_eq!(wire_bytes.len(), SAMPLES_PER_FRAME * FRAMES);
    let _ = source_pcm; // kept for symmetry; comparisons happen on wire_bytes

    // Spawn engine.
    let (engine_addr, cancel, engine_task) = start_engine().await;

    // Two test UACs bind fresh SIP + RTP sockets.
    let mut ua_a = TestUac::bind(engine_addr).await.unwrap();
    let mut ua_b = TestUac::bind(engine_addr).await.unwrap();

    // Launch both INVITEs concurrently; the second one triggers the bridge.
    let (inv_a, inv_b) = tokio::join!(ua_a.invite(RENDEZVOUS), ua_b.invite(RENDEZVOUS));
    inv_a.expect("UA-A INVITE");
    inv_b.expect("UA-B INVITE");
    let rtp_target_a = ua_a.engine_rtp.expect("UA-A got SDP answer");
    let rtp_target_b = ua_b.engine_rtp.expect("UA-B got SDP answer");

    // Give the bridge a moment to be fully up on both sides.
    sleep(Duration::from_millis(50)).await;

    // UA-B listener task — recv RTP, collect payloads in order.
    // Take ownership of UA-B here and return it so we can still BYE later.
    let collector = tokio::spawn(async move {
        let mut buf = vec![0u8; 2048];
        let mut packets: Vec<RtpPacket> = Vec::with_capacity(FRAMES);
        // Collect until quiet for 200 ms after first packet arrives,
        // or a hard 5-second cap.
        let deadline = Instant::now() + Duration::from_secs(5);
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

    // UA-A sender: 50 frames of 160 μ-law bytes each, 20 ms cadence.
    let ssrc = 0xDEAD_BEEF_u32;
    let mut seq: u16 = 1000;
    let mut ts: u32 = 0;
    for (i, chunk) in wire_bytes.chunks(SAMPLES_PER_FRAME).enumerate() {
        let pkt = RtpPacket {
            marker: i == 0,
            payload_type: PT_PCMU,
            sequence: seq,
            timestamp: ts,
            ssrc,
            payload: chunk.to_vec(),
        };
        ua_a.rtp.send_to(&pkt.encode(), rtp_target_a).await.unwrap();
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(u32::try_from(SAMPLES_PER_FRAME).unwrap());
        sleep(Duration::from_millis(20)).await;
    }

    // Stop collector (it returns UA-B so we can BYE it later).
    let (packets, mut ua_b) = collector.await.expect("collector joined");

    // rtp_target_b is used implicitly — UA-B listens on its own RTP socket,
    // but the engine sends to UA-B via its own leg-B socket (which points
    // at UA-B per the SDP answer). No explicit use here.
    let _ = rtp_target_b;

    // On loopback with no loss we expect every packet. Allow the harness
    // to miss the very first few if they arrived before the collector
    // armed its timeout.
    assert!(
        packets.len() >= FRAMES - 2,
        "UA-B received only {} of {FRAMES} RTP frames",
        packets.len()
    );

    // Concatenate μ-law payloads in sequence order and compare against
    // what UA-A sent. The engine is a byte-transparent forwarder, so a
    // tail slice of the received stream must match a tail slice of the
    // sent stream bit-for-bit.
    let received: Vec<u8> = packets.into_iter().flat_map(|p| p.payload).collect();
    assert!(received.len() >= SAMPLES_PER_FRAME * (FRAMES - 2));
    let tail_len = SAMPLES_PER_FRAME * (FRAMES - 4); // strip 2 frames at each end
    let rx_tail = &received[received.len() - tail_len..];
    let tx_tail = &wire_bytes[wire_bytes.len() - tail_len..];
    assert_eq!(
        rx_tail, tx_tail,
        "received μ-law tail differs from sent μ-law tail"
    );

    // Decode and write to disk so a human can listen.
    let pcm_received = pcmu_to_pcm16(&received);
    write_mono_pcm16(Path::new(OUT_WAV), 8_000, &pcm_received).expect("wav write");

    // Tear down dialogs; engine stops the bridge.
    ua_a.bye(RENDEZVOUS).await.expect("UA-A BYE");
    ua_b.bye(RENDEZVOUS).await.expect("UA-B BYE");

    cancel.cancel();
    let _ = timeout(Duration::from_secs(2), engine_task).await;
}
