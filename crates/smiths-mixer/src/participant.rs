//! [`ConferenceParticipantSession`] — bridges a UDP RTP flow
//! into a [`Conference`].
//!
//! Topology (one participant view):
//!
//! ```text
//! peer ──udp──▶ leg socket ──▶ PCMU→PCM16 ──▶ Conference::push_frame_seq (jitter buffer)
//! peer ◀─udp── leg socket ◀── PCM16→PCMU ◀── egress.recv
//! ```
//!
//! The ingress task parses the RTP header, drops anything that isn't
//! the negotiated PCMU payload type, decodes the payload and hands
//! the frame to the conference keyed by its RTP sequence number; the
//! conference's jitter buffer reorders, de-duplicates and re-paces it
//! on the mixer tick. The egress task turns every mixed frame into an
//! RTP packet the engine owns: PCMU payload type, the engine's SSRC,
//! a random initial sequence number and timestamp (RFC 3550 §5.1), a
//! timestamp that advances by one frame per packet and jumps across
//! stalls, and the marker bit on the first packet after such a gap.
//! The packet buffer is reused across frames.
//!
//! The path is PCMU-only (G.711 μ-law, 8 kHz, 160 samples / 20 ms) —
//! the one codec every engine build speaks without a feature flag.
//!
//! ## Admission + lifecycle
//!
//! - On construction: `Conference::join` mints the participant;
//!   the session owns the resulting `(ParticipantId, Receiver)`.
//! - While running: two tasks — RTP-in (recv → depayload →
//!   `push_frame_seq`) and egress-out (recv mixed frame → payload →
//!   send).
//! - On `stop`: cancellation token fires, both tasks exit, and
//!   `Conference::leave(participant_id)` closes the participant's
//!   channels.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaSession};
use smiths_core::rtp::{RTP_FIXED_HEADER_LEN, RtpHeader};
use smiths_core::{linear_to_ulaw, pcmu_to_pcm16};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::conference::{Conference, ParticipantFrame, ParticipantId};

/// PCMU static payload type (RFC 3551 §6).
const PT_PCMU: u8 = 0;

/// One conference participant as a [`MediaSession`].
pub struct ConferenceParticipantSession {
    id: BridgeId,
    cancel: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    conference: Arc<Conference>,
    participant_id: ParticipantId,
}

impl ConferenceParticipantSession {
    /// Spawn the two forwarder tasks and return the session.
    ///
    /// - `id` — stable session id surfaced as `MediaSession::id`.
    /// - `conference` — target mixer the participant joins.
    /// - `participant_id` — minted by `Conference::join`.
    /// - `egress` — receiver half from the same `join` call;
    ///   drives outbound RTP.
    /// - `socket` — engine-owned UDP socket for this leg.
    /// - `peer` — remote RTP endpoint.
    /// - `ssrc` — SSRC the engine uses on outbound packets.
    #[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
    #[must_use]
    pub fn spawn(
        id: BridgeId,
        conference: Arc<Conference>,
        participant_id: ParticipantId,
        egress: tokio::sync::mpsc::Receiver<ParticipantFrame>,
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
        ssrc: u32,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let samples_per_frame = conference.samples_per_frame();
        let frame_interval = conference.frame_interval();

        let in_task = spawn_ingress(
            Arc::clone(&conference),
            participant_id,
            Arc::clone(&socket),
            samples_per_frame,
            cancel.clone(),
        );
        let out_task = spawn_egress(
            egress,
            Arc::clone(&socket),
            peer,
            ssrc,
            frame_interval,
            cancel.clone(),
        );

        Arc::new(Self {
            id,
            cancel,
            tasks: Mutex::new(vec![in_task, out_task]),
            conference,
            participant_id,
        })
    }
}

#[async_trait]
impl MediaSession for ConferenceParticipantSession {
    fn id(&self) -> BridgeId {
        self.id
    }

    async fn stop(&self) {
        self.cancel.cancel();
        let mut handles = self.tasks.lock().await;
        for h in handles.drain(..) {
            let _ = h.await;
        }
        // Drop the participant from the conference so its slot
        // doesn't linger. Errors here are benign — a racing
        // `leave` from elsewhere (MCP tool, shutdown) finds
        // nothing to remove.
        let _ = self.conference.leave(self.participant_id).await;
    }
}

fn spawn_ingress(
    conference: Arc<Conference>,
    participant: ParticipantId,
    socket: Arc<UdpSocket>,
    samples_per_frame: usize,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0_u8; 1500];
        loop {
            tokio::select! {
                           biased;
                           () = cancel.cancelled() => {
                               debug!(%participant, "participant ingress cancelled");
                               return;
                           }
                           recv = socket.recv_from(&mut buf) => {
                               let n = match recv {
                                   Ok((n, _from)) => n,
                                   Err(e) => {
                                       warn!(%participant, error = %e, "participant recv_from failed");
                                       continue;
                                   }
                               };
                               let Some(hdr) = RtpHeader::parse(&buf[..n]) else {
                                   continue;
                               };
                               if hdr.payload_type != PT_PCMU {
            // Telephone-event, comfort noise or a codec the
            // leg didn't negotiate: nothing to mix.
                                   continue;
                               }
                               let payload = hdr.payload(&buf[..n]);
                               // The conference wants frames of exactly
            // `samples_per_frame`; anything else is dropped
            // and shows up on the `frame_size` drop counter.
                               if payload.len() != samples_per_frame {
                                   continue;
                               }
                               let pcm: Vec<i16> = pcmu_to_pcm16(payload);
                               let _ = conference.push_frame_seq(participant, hdr.sequence, pcm).await;
                           }
                       }
        }
    })
}

fn spawn_egress(
    mut egress: tokio::sync::mpsc::Receiver<ParticipantFrame>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    ssrc: u32,
    frame_interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // RFC 3550 §5.1: random initial sequence number and timestamp
        // so a new stream can't be mistaken for the tail of an old one.
        let mut seq: u16 = rand::random();
        let mut ts: u32 = rand::random();
        let mut last_send: Option<Instant> = None;
        let mut packet: Vec<u8> = Vec::with_capacity(RTP_FIXED_HEADER_LEN + 320);
        loop {
            tokio::select! {
                           biased;
                           () = cancel.cancelled() => {
                               debug!("participant egress cancelled");
                               return;
                           }
                           frame = egress.recv() => {
                               let Some(frame) = frame else {
                                   debug!("participant egress channel closed");
                                   return;
                               };
            // One timestamp unit per sample at 8 kHz
            // (RFC 3551 §4.5); the frame length is the step.
                               let step = u32::try_from(frame.samples.len()).unwrap_or(0);
            // A stall longer than a frame means the peer saw a
            // gap: advance the clock across it and mark the
            // packet as the start of a new talkspurt.
                               let now = Instant::now();
                               let missed = last_send.map_or(0, |t| {
                                   let elapsed = now.saturating_duration_since(t);
                                   u32::try_from(elapsed.as_micros() / frame_interval.as_micros().max(1))
                                       .unwrap_or(u32::MAX)
                                       .saturating_sub(1)
                               });
                               let marker = last_send.is_none() || missed > 0;
                               ts = ts.wrapping_add(missed.wrapping_mul(step));

                               packet.clear();
                               packet.push(0x80); // V=2, P=0, X=0, CC=0
                               packet.push((u8::from(marker) << 7) | PT_PCMU);
                               packet.extend_from_slice(&seq.to_be_bytes());
                               packet.extend_from_slice(&ts.to_be_bytes());
                               packet.extend_from_slice(&ssrc.to_be_bytes());
                               packet.extend(frame.samples.iter().copied().map(linear_to_ulaw));
                               if let Err(e) = socket.send_to(&packet, peer).await {
                                   warn!(error = %e, "participant send_to failed");
                               }
                               last_send = Some(now);
                               seq = seq.wrapping_add(1);
                               ts = ts.wrapping_add(step);
                           }
                       }
        }
    })
}

#[cfg(test)]
#[allow(clippy::similar_names)] // participant A/B naming is load-bearing
mod tests {
    use super::*;
    use crate::conference::{ConferenceConfig, ConferenceId};
    use smiths_core::ulaw_to_linear;
    use std::time::Duration;
    use tokio::time::timeout;

    fn rtp_packet(seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(RTP_FIXED_HEADER_LEN + payload.len());
        p.push(0x80);
        p.push(0);
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    struct Frame {
        marker: bool,
        seq: u16,
        ts: u32,
        ssrc: u32,
        pcm: Vec<i16>,
    }

    async fn recv_frame(sock: &UdpSocket) -> Frame {
        let mut buf = [0_u8; 1500];
        let (n, _from) = timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("peer timed out")
            .unwrap();
        let hdr = RtpHeader::parse(&buf[..n]).expect("valid RTP");
        assert_eq!(hdr.payload_type, PT_PCMU);
        assert_eq!(hdr.payload_end - hdr.payload_start, 160);
        Frame {
            marker: hdr.marker,
            seq: hdr.sequence,
            ts: hdr.timestamp,
            ssrc: hdr.ssrc,
            pcm: pcmu_to_pcm16(hdr.payload(&buf[..n])),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_participants_hear_each_other_via_udp_sessions() {
        // Topology: two "peer A / peer B" UDP sockets + two
        // engine legs hosting participant sessions. A's voice
        // should surface on B's egress (as B hears A) and vice
        // versa.
        let cfg = ConferenceConfig {
            mixer: crate::mixer::MixerConfig {
                samples_per_frame: 160,
            },
            agc: crate::agc::AgcConfig {
                target_rms: u32::MAX, // bypass
                attack: 1.0,
                release: 1.0,
                max_gain: 1.0,
            },
            frame_interval: Duration::from_millis(20),
            vad_threshold: crate::vad::VadScore::DEFAULT_SPEECH,
            jitter: crate::conference::default_jitter_config(),
        };
        let conf = Conference::spawn(ConferenceId(1), cfg);

        let leg_a = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let leg_b = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer_a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let leg_a_addr = leg_a.local_addr().unwrap();
        let leg_b_addr = leg_b.local_addr().unwrap();
        let peer_a_addr = peer_a.local_addr().unwrap();
        let peer_b_addr = peer_b.local_addr().unwrap();

        let (pa, egress_a) = conf.join().await;
        let (pb, egress_b) = conf.join().await;

        let _sa = ConferenceParticipantSession::spawn(
            BridgeId(1),
            Arc::clone(&conf),
            pa,
            egress_a,
            Arc::clone(&leg_a),
            peer_a_addr,
            0xAAAA_AAAA,
        );
        let _sb = ConferenceParticipantSession::spawn(
            BridgeId(2),
            Arc::clone(&conf),
            pb,
            egress_b,
            Arc::clone(&leg_b),
            peer_b_addr,
            0xBBBB_BBBB,
        );

        // Peer A sends μ-law 0x80 (a loud positive DC, +32 124); peer
        // B sends 0xFF (exactly 0). With AGC bypassed and only two
        // participants, B must hear A's DC and A must hear silence.
        let loud = ulaw_to_linear(0x80);
        assert_eq!(ulaw_to_linear(0xFF), 0);
        let a_in = vec![0x80_u8; 160];
        let b_in = vec![0xFF_u8; 160];
        for seq in 0..25u16 {
            peer_a
                .send_to(&rtp_packet(seq, u32::from(seq) * 160, 1, &a_in), leg_a_addr)
                .await
                .unwrap();
            peer_b
                .send_to(&rtp_packet(seq, u32::from(seq) * 160, 2, &b_in), leg_b_addr)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Peer B's ear = peer A's voice.
        let first = recv_frame(&peer_b).await;
        assert!(first.marker, "first packet of the stream is marked");
        assert_eq!(first.ssrc, 0xBBBB_BBBB, "engine SSRC toward B");
        let mut frames_b = vec![first];
        for _ in 0..7 {
            frames_b.push(recv_frame(&peer_b).await);
        }
        for pair in frames_b.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1), "consecutive seq");
            assert_eq!(
                pair[1].ts.wrapping_sub(pair[0].ts),
                160,
                "160 ticks per frame"
            );
            assert!(!pair[1].marker, "only the first packet is marked");
        }
        // Concealment frames (a tick that beat the sender) are faded
        // copies, so every sample sits in [0, loud] and at least one
        // frame is A's DC exactly.
        assert!(
            frames_b
                .iter()
                .all(|f| f.pcm.iter().all(|&s| (0..=loud).contains(&s))),
            "B hears only A's positive DC (possibly faded)"
        );
        assert!(
            frames_b.iter().any(|f| f.pcm.iter().all(|&s| s == loud)),
            "at least one frame carries A's DC exactly"
        );

        // Peer A's ear = peer B's voice = silence, frame after frame.
        for _ in 0..4 {
            let f = recv_frame(&peer_a).await;
            assert_eq!(f.ssrc, 0xAAAA_AAAA);
            assert!(
                f.pcm.iter().all(|&s| s == 0),
                "A hears exact silence, got {:?}",
                &f.pcm[..4]
            );
        }

        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_pcmu_and_short_packets_never_reach_the_mixer() {
        let conf = Conference::spawn(
            ConferenceId(3),
            ConferenceConfig {
                frame_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        let leg = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let leg_addr = leg.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (pid, egress) = conf.join().await;
        let session = ConferenceParticipantSession::spawn(
            BridgeId(8),
            Arc::clone(&conf),
            pid,
            egress,
            leg,
            peer.local_addr().unwrap(),
            1,
        );
        // Telephone-event (PT 101), a 100-sample PCMU frame, garbage.
        let mut dtmf = rtp_packet(1, 0, 1, &[0x37, 0x8A, 0x01, 0x40]);
        dtmf[1] = 101;
        peer.send_to(&dtmf, leg_addr).await.unwrap();
        peer.send_to(&rtp_packet(2, 160, 1, &[0xFF; 100]), leg_addr)
            .await
            .unwrap();
        peer.send_to(b"garbage", leg_addr).await.unwrap();
        // One good frame so we can prove the buffer saw exactly one.
        peer.send_to(&rtp_packet(3, 320, 1, &[0xFF; 160]), leg_addr)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let js = conf.participant_stats(pid).await.unwrap();
        assert_eq!(
            js.inserted, 1,
            "only the well-formed PCMU frame was buffered"
        );
        session.stop().await;
        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_session_drops_participant() {
        let conf = Conference::spawn(
            ConferenceId(1),
            ConferenceConfig {
                frame_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        let leg = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let peer_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let (pid, egress) = conf.join().await;
        let session = ConferenceParticipantSession::spawn(
            BridgeId(7),
            Arc::clone(&conf),
            pid,
            egress,
            leg,
            peer_addr,
            1,
        );
        // Stats show one participant.
        assert_eq!(conf.stats().await.participants, 1);
        session.stop().await;
        assert_eq!(conf.stats().await.participants, 0);
        conf.shutdown().await;
    }
}
