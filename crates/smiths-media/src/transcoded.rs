//! Transcoded two-leg session — the [`MediaSession`] variant the
//! UAS installs when leg A speaks one codec and leg B speaks another.
//!
//! Topology:
//!
//! ```text
//! peer A ──udp──▶ leg A socket ──▶ A.decoder ──▶ jitter (A→B) ──┐
//! │ 20 ms tick
//! peer B ◀─udp── leg B socket ◀── B.encoder ◀── frame ◀────────┘
//!
//! (and the mirror image for B → A)
//! ```
//!
//! Four tasks. Per direction, an **ingress** task receives on one
//! leg, checks the payload type, decodes the payload with that leg's
//! decoder and drops the PCM frame into a [`JitterBuffer`] keyed by
//! the RTP sequence number; an **egress** task ticks at the frame
//! interval, pulls the next in-order (or concealed) frame out of the
//! buffer, encodes it with the other leg's encoder and sends it with
//! a header the engine owns: the egress leg's negotiated payload
//! type, the engine's own SSRC, a sequence number that counts every
//! frame the engine emits, a timestamp that advances at the egress
//! leg's RTP clock rate, and the marker bit on the first frame of a
//! talkspurt. Because the engine re-paces the audio on its own clock
//! it is the RTP source for the egress peer, so none of the ingress
//! header rides through.
//!
//! Each leg owns its decoder and its encoder outright, so the two
//! directions never share codec state and no lock sits on the packet
//! path; the only shared object per direction is the jitter buffer,
//! touched under a `std::sync::Mutex` that is never held across an
//! await. Steady-state cost per packet is one allocation — the
//! decoded frame the buffer stores — and no other heap traffic: the
//! egress task reuses its packet buffer across frames.
//!
//! Not covered here: SRTP and RTCP on transcoded legs. The plain
//! [`crate::Bridge`] handles both for passthrough calls; a
//! transcoded call that needs them has to wait for the same
//! per-leg transform pair to be threaded into this session.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaSession};
use smiths_core::rtp::{RTP_FIXED_HEADER_LEN, RtpHeader};
use smiths_transcode::{
    Codec, CodecKind, CpuClock, G711Codec, G711Variant, TranscodeLease, TranscodeMetrics,
};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::bridge::fresh_ssrc;
use crate::jitter::{JitterBuffer, JitterConfig, JitterStats};

/// One leg of a transcoded session: the socket, the peer, the codec
/// pair for this leg and the RTP parameters negotiated for it.
pub struct TranscodedLeg {
    /// Engine-owned RTP socket for this leg.
    pub socket: Arc<UdpSocket>,
    /// Where to send transcoded RTP for this leg's peer.
    pub peer: SocketAddr,
    /// Decodes what this peer sends.
    pub decoder: Box<dyn Codec>,
    /// Encodes what the engine sends to this peer.
    pub encoder: Box<dyn Codec>,
    /// Payload type negotiated on this leg. Stamped on egress;
    /// ingress packets carrying any other payload type are dropped.
    pub payload_type: u8,
    /// RTP clock rate of this leg's codec, in Hz. Egress timestamps
    /// advance by `clock_rate × frame_interval` per frame.
    pub clock_rate: u32,
}

impl TranscodedLeg {
    /// Leg speaking one of the G.711 variants with its RFC 3551
    /// static payload type and 8 kHz clock.
    #[must_use]
    pub fn g711(socket: Arc<UdpSocket>, peer: SocketAddr, variant: G711Variant) -> Self {
        let kind = variant.kind();
        Self {
            socket,
            peer,
            decoder: Box::new(G711Codec::new(variant)),
            encoder: Box::new(G711Codec::new(variant)),
            payload_type: kind.static_payload_type().unwrap_or(0),
            clock_rate: kind.rtp_clock_rate(),
        }
    }

    /// Wire-side codec of this leg (from the encoder).
    #[must_use]
    pub fn codec(&self) -> CodecKind {
        self.encoder.kind()
    }
}

impl std::fmt::Debug for TranscodedLeg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranscodedLeg")
            .field("peer", &self.peer)
            .field("codec", &self.codec())
            .field("payload_type", &self.payload_type)
            .field("clock_rate", &self.clock_rate)
            .finish_non_exhaustive()
    }
}

/// Tunables for a [`TranscodedSession`].
#[derive(Clone, Debug)]
pub struct TranscodedConfig {
    /// Frame cadence of the egress clocks. 20 ms is the RFC 3551
    /// default for G.711 and the Opus frame size the engine uses.
    pub frame_interval: Duration,
    /// Jitter-buffer depth policy applied to both directions.
    pub jitter: JitterConfig,
}

impl Default for TranscodedConfig {
    fn default() -> Self {
        Self {
            frame_interval: Duration::from_millis(20),
            jitter: JitterConfig::default(),
        }
    }
}

/// Jitter-buffer counters for both directions of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscodedStats {
    /// Frames received from A and played toward B.
    pub a_to_b: JitterStats,
    /// Frames received from B and played toward A.
    pub b_to_a: JitterStats,
}

/// Two-leg session that decodes, re-paces and re-encodes audio
/// between legs with different codecs.
pub struct TranscodedSession {
    id: BridgeId,
    cancel: CancellationToken,
    tasks: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
    jitter_a_to_b: Arc<Mutex<JitterBuffer>>,
    jitter_b_to_a: Arc<Mutex<JitterBuffer>>,
    /// The admission lease is owned for the session's lifetime —
    /// dropped on `Self::drop`, which decrements the budget so a
    /// crashed handler can't leak a slot.
    _lease: TranscodeLease,
}

impl TranscodedSession {
    /// Spawn the four forwarder tasks with default tunables and
    /// return the session handle. Each leg's codecs move into the
    /// tasks that use them; the `lease` is owned for the session's
    /// lifetime and released when it drops.
    #[must_use]
    pub fn spawn(
        id: BridgeId,
        leg_a: TranscodedLeg,
        leg_b: TranscodedLeg,
        metrics: Arc<TranscodeMetrics>,
        lease: TranscodeLease,
    ) -> Arc<Self> {
        Self::spawn_with(
            id,
            leg_a,
            leg_b,
            metrics,
            lease,
            &TranscodedConfig::default(),
        )
    }

    /// [`Self::spawn`] with explicit tunables.
    #[must_use]
    pub fn spawn_with(
        id: BridgeId,
        leg_a: TranscodedLeg,
        leg_b: TranscodedLeg,
        metrics: Arc<TranscodeMetrics>,
        lease: TranscodeLease,
        cfg: &TranscodedConfig,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        // Frame size in samples at the decoder's PCM rate is only
        // needed for silence before the first frame; both G.711 and
        // the 8 kHz Opus configuration decode 20 ms to 160 samples.
        let frame_samples = usize::try_from(
            u128::from(CodecKind::Pcmu.rtp_clock_rate()) * cfg.frame_interval.as_millis() / 1_000,
        )
        .unwrap_or(160);
        let jitter_a_to_b = Arc::new(Mutex::new(JitterBuffer::with_config(
            frame_samples,
            cfg.jitter,
        )));
        let jitter_b_to_a = Arc::new(Mutex::new(JitterBuffer::with_config(
            frame_samples,
            cfg.jitter,
        )));

        let TranscodedLeg {
            socket: sock_a,
            peer: peer_a,
            decoder: dec_a,
            encoder: enc_a,
            payload_type: pt_a,
            clock_rate: clock_a,
        } = leg_a;
        let TranscodedLeg {
            socket: sock_b,
            peer: peer_b,
            decoder: dec_b,
            encoder: enc_b,
            payload_type: pt_b,
            clock_rate: clock_b,
        } = leg_b;

        let tasks = vec![
            spawn_ingress(Ingress {
                label: "a_to_b",
                socket: Arc::clone(&sock_a),
                decoder: dec_a,
                payload_type: pt_a,
                jitter: Arc::clone(&jitter_a_to_b),
                clock: CpuClock::new(Arc::clone(&metrics)),
                cancel: cancel.clone(),
            }),
            spawn_egress(Egress {
                label: "a_to_b",
                socket: Arc::clone(&sock_b),
                peer: peer_b,
                encoder: enc_b,
                payload_type: pt_b,
                clock_rate: clock_b,
                jitter: Arc::clone(&jitter_a_to_b),
                frame_interval: cfg.frame_interval,
                clock: CpuClock::new(Arc::clone(&metrics)),
                cancel: cancel.clone(),
            }),
            spawn_ingress(Ingress {
                label: "b_to_a",
                socket: sock_b,
                decoder: dec_b,
                payload_type: pt_b,
                jitter: Arc::clone(&jitter_b_to_a),
                clock: CpuClock::new(Arc::clone(&metrics)),
                cancel: cancel.clone(),
            }),
            spawn_egress(Egress {
                label: "b_to_a",
                socket: sock_a,
                peer: peer_a,
                encoder: enc_a,
                payload_type: pt_a,
                clock_rate: clock_a,
                jitter: Arc::clone(&jitter_b_to_a),
                frame_interval: cfg.frame_interval,
                clock: CpuClock::new(metrics),
                cancel: cancel.clone(),
            }),
        ];

        Arc::new(Self {
            id,
            cancel,
            tasks: tokio::sync::Mutex::new(tasks),
            jitter_a_to_b,
            jitter_b_to_a,
            _lease: lease,
        })
    }

    /// Jitter-buffer counters for both directions.
    #[must_use]
    pub fn stats(&self) -> TranscodedStats {
        TranscodedStats {
            a_to_b: self
                .jitter_a_to_b
                .lock()
                .map_or_else(|_| JitterStats::default(), |j| j.stats()),
            b_to_a: self
                .jitter_b_to_a
                .lock()
                .map_or_else(|_| JitterStats::default(), |j| j.stats()),
        }
    }
}

#[async_trait]
impl MediaSession for TranscodedSession {
    fn id(&self) -> BridgeId {
        self.id
    }

    async fn stop(&self) {
        self.cancel.cancel();
        let mut handles = self.tasks.lock().await;
        for h in handles.drain(..) {
            let _ = h.await;
        }
    }
}

/// State of one ingress task: receive, decode, buffer.
struct Ingress {
    label: &'static str,
    socket: Arc<UdpSocket>,
    decoder: Box<dyn Codec>,
    payload_type: u8,
    jitter: Arc<Mutex<JitterBuffer>>,
    clock: CpuClock,
    cancel: CancellationToken,
}

fn spawn_ingress(mut i: Ingress) -> JoinHandle<()> {
    tokio::spawn(async move {
        let label = i.label;
        // Payloads are ≤ a few hundred bytes for 20 ms frames; 1500
        // covers any sane MTU.
        let mut buf = vec![0_u8; 1500];
        loop {
            tokio::select! {
                           biased;
                           () = i.cancel.cancelled() => {
                               debug!(label, "transcoded ingress cancelled");
                               return;
                           }
                           recv = i.socket.recv_from(&mut buf) => {
                               let n = match recv {
                                   Ok((n, _from)) => n,
                                   Err(e) => {
                                       warn!(label, error = %e, "transcoded recv_from failed");
                                       continue;
                                   }
                               };
                               let Some(hdr) = RtpHeader::parse(&buf[..n]) else {
                                   continue;
                               };
                               if hdr.payload_type != i.payload_type {
            // Comfort noise, telephone-event or a codec the
            // leg didn't negotiate: nothing to decode.
                                   continue;
                               }
                               let payload = hdr.payload(&buf[..n]);
                               let Ingress { decoder, clock, .. } = &mut i;
                               let pcm = match clock.timed(decoder.kind(), || decoder.decode(payload)) {
                                   Ok(pcm) => pcm,
                                   Err(e) => {
                                       warn!(label, error = %e, "decoder rejected frame; dropping");
                                       continue;
                                   }
                               };
                               if let Ok(mut jb) = i.jitter.lock() {
                                   jb.insert(hdr.sequence, pcm);
                               }
                           }
                       }
        }
    })
}

/// State of one egress task: tick, pull, encode, send.
struct Egress {
    label: &'static str,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    encoder: Box<dyn Codec>,
    payload_type: u8,
    clock_rate: u32,
    jitter: Arc<Mutex<JitterBuffer>>,
    frame_interval: Duration,
    clock: CpuClock,
    cancel: CancellationToken,
}

fn spawn_egress(mut e: Egress) -> JoinHandle<()> {
    tokio::spawn(async move {
        let label = e.label;
        let ssrc = fresh_ssrc();
        // RFC 3550 §5.1: initial sequence number and timestamp are
        // random so a new stream can't be confused with the tail of
        // an old one on the same address.
        let mut seq = u16::try_from(fresh_ssrc() & 0xFFFF).unwrap_or(0);
        let mut ts = fresh_ssrc();
        // Timestamp advance per frame at the egress leg's clock.
        let ts_step =
            u32::try_from(u128::from(e.clock_rate) * e.frame_interval.as_millis() / 1_000)
                .unwrap_or(160);
        // Marker goes on the first packet of a talkspurt: the first
        // frame ever and the first after the buffer re-primes.
        let mut talkspurt_start = true;
        // Both buffers are reused across frames; after the first few
        // frames no allocation happens on this path.
        let mut packet: Vec<u8> = Vec::with_capacity(RTP_FIXED_HEADER_LEN + 320);
        let mut payload: Vec<u8> = Vec::with_capacity(320);
        let mut ticker = tokio::time::interval(e.frame_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                           biased;
                           () = e.cancel.cancelled() => {
                               debug!(label, "transcoded egress cancelled");
                               return;
                           }
                           _ = ticker.tick() => {
                               let frame = match e.jitter.lock() {
                                   Ok(mut jb) => jb.tick(),
                                   Err(_) => None,
                               };
                               let Some(pcm) = frame else {
            // Priming (or nothing has arrived yet): the
            // next frame out starts a talkspurt.
                                   talkspurt_start = true;
                                   continue;
                               };
                               packet.clear();
                               packet.push(0x80); // V=2, P=0, X=0, CC=0
                               packet.push((u8::from(talkspurt_start) << 7) | (e.payload_type & 0x7F));
                               packet.extend_from_slice(&seq.to_be_bytes());
                               packet.extend_from_slice(&ts.to_be_bytes());
                               packet.extend_from_slice(&ssrc.to_be_bytes());
                               let Egress { encoder, clock, .. } = &mut e;
                               let encoded = clock.timed(encoder.kind(), || {
                                   encoder.encode_into(&pcm, &mut payload)
                               });
                               if let Err(err) = encoded {
                                   warn!(label, error = %err, "encoder rejected frame; dropping");
                                   continue;
                               }
                               packet.extend_from_slice(&payload);
                               if let Err(err) = e.socket.send_to(&packet, e.peer).await {
                                   warn!(label, error = %err, "transcoded send_to failed");
                               }
                               talkspurt_start = false;
                               seq = seq.wrapping_add(1);
                               ts = ts.wrapping_add(ts_step);
                           }
                       }
        }
    })
}

#[cfg(test)]
#[allow(clippy::similar_names)] // leg_a / leg_b / peer_a / peer_b are load-bearing
mod tests {
    use super::*;
    use smiths_core::media::BridgeId;
    use smiths_core::{pcmu_to_pcm16, ulaw_to_linear};
    use smiths_transcode::{CpuBudget, CpuBudgetConfig, pcm16_to_pcma, pcma_to_pcm16};
    use std::time::Duration;
    use tokio::time::timeout;

    fn budget() -> CpuBudget {
        CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: 4,
                cpu_budget_ms_per_call: 100,
            },
            TranscodeMetrics::noop(),
        )
    }

    async fn bind() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    /// Build a 12-byte RTP header (V=2, marker, PT, seq/ts/SSRC) +
    /// `payload`. Used by tests as the bytes a "peer" sends.
    fn rtp_packet(pt: u8, seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(12 + payload.len());
        p.push(0x80); // V=2, no padding/extension/CC
        p.push(pt);
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    struct Received {
        marker: bool,
        pt: u8,
        seq: u16,
        ts: u32,
        ssrc: u32,
        payload: Vec<u8>,
    }

    async fn recv_frame(sock: &UdpSocket, expect_from: SocketAddr) -> Received {
        let mut buf = vec![0_u8; 1500];
        let (n, from) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("peer never received a transcoded frame")
            .unwrap();
        assert_eq!(from, expect_from, "frame came from wrong leg");
        let hdr = RtpHeader::parse(&buf[..n]).expect("valid RTP header");
        Received {
            marker: hdr.marker,
            pt: hdr.payload_type,
            seq: hdr.sequence,
            ts: hdr.timestamp,
            ssrc: hdr.ssrc,
            payload: hdr.payload(&buf[..n]).to_vec(),
        }
    }

    /// A→B PCMU→PCMA session over four loopback sockets.
    struct Harness {
        session: Arc<TranscodedSession>,
        leg_a_addr: SocketAddr,
        leg_b_addr: SocketAddr,
        peer_a: Arc<UdpSocket>,
        peer_b: Arc<UdpSocket>,
        _budget: CpuBudget,
    }

    async fn harness(clock_b: u32) -> Harness {
        let leg_a = bind().await;
        let leg_b = bind().await;
        let peer_a = bind().await;
        let peer_b = bind().await;
        let leg_a_addr = leg_a.local_addr().unwrap();
        let leg_b_addr = leg_b.local_addr().unwrap();
        let b = budget();
        let lease = b.try_admit().unwrap();
        let mut leg_b = TranscodedLeg::g711(leg_b, peer_b.local_addr().unwrap(), G711Variant::Pcma);
        leg_b.clock_rate = clock_b;
        let session = TranscodedSession::spawn(
            BridgeId(42),
            TranscodedLeg::g711(leg_a, peer_a.local_addr().unwrap(), G711Variant::Pcmu),
            leg_b,
            TranscodeMetrics::noop(),
            lease,
        );
        Harness {
            session,
            leg_a_addr,
            leg_b_addr,
            peer_a,
            peer_b,
            _budget: b,
        }
    }

    /// PCMU frame whose every sample decodes to the same value, so a
    /// frame is identifiable after transcoding by its first byte.
    fn pcmu_frame(byte: u8) -> Vec<u8> {
        vec![byte; 160]
    }

    /// A per-sequence payload byte, so each frame in a run is
    /// distinguishable from its neighbours.
    fn seq_byte(seq: u16) -> u8 {
        u8::try_from(seq).expect("short test run") * 0x11
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pcmu_to_pcma_rewrites_pt_and_owns_the_egress_header() {
        let h = harness(8_000).await;

        // Six frames back-to-back so the jitter buffer primes and then
        // plays them in order without starving.
        let bytes = [0x10u8, 0x20, 0x30, 0x40, 0x50, 0x60];
        for (i, b) in bytes.iter().enumerate() {
            let i = u16::try_from(i).expect("six frames");
            let pkt = rtp_packet(0, 100 + i, u32::from(i) * 160, 0xCAFE_F00D, &pcmu_frame(*b));
            h.peer_a.send_to(&pkt, h.leg_a_addr).await.unwrap();
        }

        let first = recv_frame(&h.peer_b, h.leg_b_addr).await;
        assert_eq!(first.pt, 8, "egress carries the PCMA payload type");
        assert!(first.marker, "first frame of the talkspurt is marked");
        assert_ne!(first.ssrc, 0xCAFE_F00D, "engine is the RTP source toward B");
        assert_eq!(first.payload.len(), 160);
        // Payload is the A-law encoding of the decoded μ-law input.
        let expected = pcm16_to_pcma(&pcmu_to_pcm16(&pcmu_frame(bytes[0])));
        assert_eq!(first.payload, expected);

        let mut prev = first;
        for b in &bytes[1..] {
            let next = recv_frame(&h.peer_b, h.leg_b_addr).await;
            assert_eq!(next.pt, 8);
            assert!(!next.marker, "only the talkspurt start is marked");
            assert_eq!(next.ssrc, prev.ssrc, "SSRC is stable for the session");
            assert_eq!(next.seq, prev.seq.wrapping_add(1), "engine-owned sequence");
            assert_eq!(
                next.ts.wrapping_sub(prev.ts),
                160,
                "8 kHz × 20 ms per frame"
            );
            assert_eq!(next.payload, pcm16_to_pcma(&pcmu_to_pcm16(&pcmu_frame(*b))));
            prev = next;
        }

        // Reverse direction: PCMA in from B, PCMU out to A.
        let pcma: Vec<u8> = vec![0xD5; 160]; // A-law 0xD5 = +8 (near silence)
        for i in 0..6u16 {
            let pkt = rtp_packet(8, 500 + i, u32::from(i) * 160, 0xDEAD_BEEF, &pcma);
            h.peer_b.send_to(&pkt, h.leg_b_addr).await.unwrap();
        }
        let back = recv_frame(&h.peer_a, h.leg_a_addr).await;
        assert_eq!(back.pt, 0, "egress toward A carries PCMU");
        assert!(back.marker);
        let decoded_back = pcmu_to_pcm16(&back.payload);
        let expected_back = pcma_to_pcm16(&pcma);
        for (got, want) in decoded_back.iter().zip(&expected_back) {
            assert!(
                (got - want).abs() <= 8,
                "μ-law re-quantization stays within a step"
            );
        }

        let stats = h.session.stats();
        assert!(stats.a_to_b.played >= 6);
        assert_eq!(stats.a_to_b.concealed_loss, 0);
        h.session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingress_is_reordered_and_other_payload_types_are_ignored() {
        let h = harness(8_000).await;
        // Sequence 4 and 3 swapped on the wire; plus a telephone-event
        // packet (PT 101) that must not reach the decoder.
        let order = [1u16, 2, 4, 3, 5, 6];
        for seq in order {
            let pkt = rtp_packet(0, seq, u32::from(seq) * 160, 7, &pcmu_frame(seq_byte(seq)));
            h.peer_a.send_to(&pkt, h.leg_a_addr).await.unwrap();
            if seq == 2 {
                h.peer_a
                    .send_to(
                        &rtp_packet(101, 200, 0, 7, &[0x37, 0x8A, 0x01, 0x40]),
                        h.leg_a_addr,
                    )
                    .await
                    .unwrap();
            }
        }
        for seq in 1..=6u16 {
            let got = recv_frame(&h.peer_b, h.leg_b_addr).await;
            let want = pcm16_to_pcma(&pcmu_to_pcm16(&pcmu_frame(seq_byte(seq))));
            assert_eq!(got.payload, want, "frame {seq} played in sequence order");
            assert_eq!(got.pt, 8);
        }
        let stats = h.session.stats();
        assert_eq!(
            stats.a_to_b.inserted, 6,
            "the PT 101 packet never reached the buffer"
        );
        assert_eq!(stats.a_to_b.played, 6);
        h.session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timestamps_advance_at_the_egress_clock_rate() {
        // Leg B claims a 16 kHz RTP clock: 20 ms frames must step the
        // timestamp by 320 even though the PCM in the middle is 8 kHz.
        let h = harness(16_000).await;
        for seq in 0..5u16 {
            let pkt = rtp_packet(0, seq, u32::from(seq) * 160, 9, &pcmu_frame(0x7F));
            h.peer_a.send_to(&pkt, h.leg_a_addr).await.unwrap();
        }
        let first = recv_frame(&h.peer_b, h.leg_b_addr).await;
        let second = recv_frame(&h.peer_b, h.leg_b_addr).await;
        assert_eq!(second.ts.wrapping_sub(first.ts), 320);
        assert_eq!(second.seq, first.seq.wrapping_add(1));
        h.session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn silence_is_decoded_to_silence() {
        // μ-law 0xFF is +0: after decode → A-law encode it must come
        // out as the A-law zero code (0xD5), proving the pipeline
        // doesn't inject bias.
        let h = harness(8_000).await;
        for seq in 0..4u16 {
            let pkt = rtp_packet(0, seq, 0, 1, &pcmu_frame(0xFF));
            h.peer_a.send_to(&pkt, h.leg_a_addr).await.unwrap();
        }
        let got = recv_frame(&h.peer_b, h.leg_b_addr).await;
        assert!(
            got.payload.iter().all(|&b| b == 0xD5),
            "A-law silence expected"
        );
        assert_eq!(ulaw_to_linear(0xFF), 0);
        h.session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_cancels_forwarders() {
        let leg_a = bind().await;
        let leg_b = bind().await;
        let b = budget();
        let lease = b.try_admit().unwrap();
        let session = TranscodedSession::spawn(
            BridgeId(1),
            TranscodedLeg::g711(leg_a, "127.0.0.1:1".parse().unwrap(), G711Variant::Pcmu),
            TranscodedLeg::g711(leg_b, "127.0.0.1:2".parse().unwrap(), G711Variant::Pcma),
            TranscodeMetrics::noop(),
            lease,
        );
        timeout(Duration::from_secs(2), session.stop())
            .await
            .expect("stop hung");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_session_releases_admission_lease() {
        let leg_a = bind().await;
        let leg_b = bind().await;
        // Cap at 1: the session holds the only slot while alive.
        let b = CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: 1,
                cpu_budget_ms_per_call: 50,
            },
            TranscodeMetrics::noop(),
        );
        let lease = b.try_admit().unwrap();
        assert!(b.try_admit().is_err(), "budget should be at cap");

        let session = TranscodedSession::spawn(
            BridgeId(7),
            TranscodedLeg::g711(leg_a, "127.0.0.1:1".parse().unwrap(), G711Variant::Pcmu),
            TranscodedLeg::g711(leg_b, "127.0.0.1:2".parse().unwrap(), G711Variant::Pcma),
            TranscodeMetrics::noop(),
            lease,
        );
        session.stop().await;
        assert!(
            b.try_admit().is_err(),
            "slot is held until the session drops"
        );
        drop(session);
        let next = b.try_admit().expect("freed slot should be reusable");
        drop(next);
    }
}
