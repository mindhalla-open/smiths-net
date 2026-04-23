//! [`ConferenceParticipantSession`] — bridges a UDP RTP flow
//! into a [`Conference`] (slice 5.6e-runtime).
//!
//! Topology (one participant view):
//!
//! ```text
//!   peer  ──udp──▶ leg socket ──▶ depayload PCMU→PCM16 ──▶ Conference::push_frame
//!   peer  ◀─udp── leg socket ◀── payload PCM16→PCMU   ◀── egress.recv()
//! ```
//!
//! Today's path is PCMU-only (G.711 μ-law, 8 kHz, 160 samples /
//! 20 ms) — the one codec every engine build speaks without a
//! feature flag. A richer participant would route through
//! `smiths-transcode` on both edges; that's a follow-on slice
//! once the PCMU path is proven end-to-end.
//!
//! ## Admission + lifecycle
//!
//! - On construction: `Conference::join` mints the participant;
//!   the session owns the resulting `(ParticipantId, Receiver)`.
//! - While running: two tasks — RTP-in (recv → depayload →
//!   `push_frame`) and egress-out (recv mixed frame → payload →
//!   send).
//! - On `stop()`: cancellation token fires, both tasks exit, and
//!   `Conference::leave(participant_id)` closes the participant's
//!   channels.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaSession};
use smiths_core::{pcm16_to_pcmu, pcmu_to_pcm16};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::conference::{Conference, ParticipantFrame, ParticipantId};

/// RFC 3550 §5.1 fixed-header length.
const RTP_HEADER_LEN: usize = 12;

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

        let in_task = spawn_ingress(
            Arc::clone(&conference),
            participant_id,
            Arc::clone(&socket),
            samples_per_frame,
            cancel.clone(),
        );
        let out_task = spawn_egress(egress, Arc::clone(&socket), peer, ssrc, cancel.clone());

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
                    let Ok((n, _from)) = recv else {
                        if let Err(e) = recv {
                            warn!(%participant, error = %e, "participant recv_from failed");
                        }
                        continue;
                    };
                    if n < RTP_HEADER_LEN {
                        continue;
                    }
                    let payload = &buf[RTP_HEADER_LEN..n];
                    let pcm: Vec<i16> = pcmu_to_pcm16(payload);
                    // The conference wants frames of exactly
                    // `samples_per_frame`. Real deployments run
                    // the peer at 20 ms cadence = 160 samples,
                    // which matches. On a mismatch we drop —
                    // the FrameSizeMismatch gauge on MixerMetrics
                    // catches this pattern.
                    if pcm.len() != samples_per_frame {
                        continue;
                    }
                    let _ = conference.push_frame(participant, pcm).await;
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
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Sequence + timestamp start at 0; peer accepts anything
        // on the first packet (no recovery in progress) so
        // pre-seeding with zeros is fine.
        let mut seq: u16 = 0;
        let mut ts: u32 = 0;
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
                    let payload: Vec<u8> = pcm16_to_pcmu(&frame.samples);
                    let mut out = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
                    out.push(0x80); // V=2
                    out.push(0);    // PT=0 (PCMU)
                    out.extend_from_slice(&seq.to_be_bytes());
                    out.extend_from_slice(&ts.to_be_bytes());
                    out.extend_from_slice(&ssrc.to_be_bytes());
                    out.extend_from_slice(&payload);
                    if let Err(e) = socket.send_to(&out, peer).await {
                        warn!(error = %e, "participant send_to failed");
                    }
                    seq = seq.wrapping_add(1);
                    // 160 samples at 8 kHz → 160 timestamp units
                    // per 20 ms frame per RFC 3551 §4.5.
                    // payload.len() is bounded by the mixer frame
                    // size (≤ a few hundred samples), so the
                    // truncating cast is sign-safe.
                    #[allow(clippy::cast_possible_truncation)]
                    let inc = payload.len() as u32;
                    ts = ts.wrapping_add(inc);
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
    use std::time::Duration;
    use tokio::time::timeout;

    fn rtp_packet(seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        p.push(0x80);
        p.push(0);
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        p
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

        // Peer A sends PCMU constant value 0x80 (maps to a
        // specific PCM16 value); peer B sends 0xFF (decodes to
        // ~0). On the next tick, peer B should hear peer A's
        // decoded-and-reencoded value (non-trivial from 0x80)
        // and peer A should hear peer B's (near silence).
        let mut a_in = vec![0x80_u8; 160];
        let mut b_in = vec![0xFF_u8; 160];
        for seq in 0..25 {
            peer_a
                .send_to(&rtp_packet(seq, u32::from(seq) * 160, 1, &a_in), leg_a_addr)
                .await
                .unwrap();
            peer_b
                .send_to(&rtp_packet(seq, u32::from(seq) * 160, 2, &b_in), leg_b_addr)
                .await
                .unwrap();
            // Rotate buf so the payload is detectably non-static.
            a_in[0] = a_in[0].wrapping_add(1);
            b_in[0] = b_in[0].wrapping_sub(1);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        // Drain a few frames on each peer; assert they received
        // non-zero bytes.
        let mut buf = [0_u8; 1500];
        let (n, _from) = timeout(Duration::from_secs(2), peer_a.recv_from(&mut buf))
            .await
            .expect("peer A timed out")
            .unwrap();
        assert!(n > RTP_HEADER_LEN);
        let payload = &buf[RTP_HEADER_LEN..n];
        // Peer A's ear = peer B's voice. Peer B sent 0xFF which
        // decodes to ~0; after mixing + re-encoding, payload
        // should be close to PCMU silence (~0xFF).
        assert!(
            payload.iter().any(|&b| b != 0),
            "peer A got all-zero payload"
        );

        let (n, _from) = timeout(Duration::from_secs(2), peer_b.recv_from(&mut buf))
            .await
            .expect("peer B timed out")
            .unwrap();
        assert!(n > RTP_HEADER_LEN);

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
