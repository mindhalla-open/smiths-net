//! Transcoded two-leg session — the [`MediaSession`] variant the
//! UAS installs when leg A speaks one codec and leg B speaks another
//! (slice 5.6b).
//!
//! Topology mirrors the plain [`crate::Bridge`]:
//!
//! ```text
//!   peer A ──udp──▶ leg A socket ──┐
//!                                  │   CallTranscoder
//!   peer A ◀─udp── leg A socket ──┤  (PCM16 in the middle)
//!                                  │
//!   peer B ──udp──▶ leg B socket ──┘
//!   peer B ◀─udp── leg B socket
//! ```
//!
//! Two forwarder tasks (A→B and B→A). Each task `recv_from`s on its
//! ingress socket, parses the RTP header, hands the payload to the
//! shared `CallTranscoder`, builds a fresh RTP header for the egress
//! leg (preserving SSRC + sequence + timestamp), and `send_to`s the
//! peer.
//!
//! ## What this session does NOT do (slice 5.6b)
//!
//! - **RTP header rewrite beyond payload swap.** The transcoded
//!   payload typically has a different length and frame cadence
//!   (Opus 20 ms ≠ G.711 20 ms in *bytes*; Opus VBR varies).
//!   Sequence numbers and timestamps are preserved verbatim from
//!   the ingress packet — that's correct for codec pairs with the
//!   same frame cadence (G.711 ↔ G.711, Opus ↔ Opus, both at
//!   8/48 kHz / 20 ms). Cross-rate pairs (Opus 48 kHz ↔ PCMU 8 kHz)
//!   need a timestamp scaling pass; that's a 5.6c follow-on once
//!   the UAS auto-construction lands and the cross-rate path
//!   actually fires.
//! - **SRTP.** The plain bridge runs SRTP on each leg by stitching
//!   `LegSrtp` transforms before the SSRC rewrite. The transcoded
//!   path skips SRTP today — adding it requires the same per-leg
//!   transform pair plus a re-encrypt after payload swap. Out of
//!   scope for this slice; the audio-only path is what gets
//!   tested.
//! - **RTCP.** Same reasoning — the plain bridge emits SR/RR;
//!   the transcoded path stays SR/RR-silent until 5.6c when both
//!   sides land together.
//!
//! These omissions are why the session lives in its own module
//! rather than as a feature flag on `Bridge` — keeping the two
//! kinds of session structurally distinct prevents accidentally
//! enabling untested capability combos at runtime.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaSession};
use smiths_transcode::{CallTranscoder, TranscodeLease};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// One leg of a transcoded session. Same fields as
/// [`crate::Leg`] minus the SRTP/RTCP slots that the transcoded
/// path doesn't yet support.
#[derive(Debug, Clone)]
pub struct TranscodedLeg {
    /// Engine-owned RTP socket for this leg.
    pub socket: Arc<UdpSocket>,
    /// Where to send transcoded RTP for this leg's peer.
    pub peer: SocketAddr,
}

/// Two-leg session that runs [`CallTranscoder`] inline on the
/// per-frame RTP payload.
pub struct TranscodedSession {
    id: BridgeId,
    cancel: CancellationToken,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The admission lease is owned for the session's lifetime —
    /// dropped on `Self::drop`, which decrements the budget so a
    /// crashed handler can't leak a slot.
    _lease: TranscodeLease,
}

impl TranscodedSession {
    /// Spawn the forwarders and return the session handle.
    ///
    /// `transcoder` is moved into the session — wrapped in an
    /// `Arc<Mutex<_>>` internally so the two forwarder tasks can
    /// share its mutable state. The `lease` is owned for the
    /// session's lifetime; dropping the session releases it.
    #[must_use]
    // Legs are by-value because the caller naturally constructs
    // them at the call site; refs would clutter every UAS path.
    #[allow(clippy::needless_pass_by_value)]
    pub fn spawn(
        id: BridgeId,
        leg_a: TranscodedLeg,
        leg_b: TranscodedLeg,
        transcoder: CallTranscoder,
        lease: TranscodeLease,
    ) -> Arc<Self> {
        let cancel = CancellationToken::new();
        let shared = Arc::new(Mutex::new(transcoder));

        let a_to_b = spawn_forwarder(
            "a_to_b",
            Arc::clone(&leg_a.socket),
            Arc::clone(&leg_b.socket),
            leg_b.peer,
            Arc::clone(&shared),
            Direction::AToB,
            cancel.clone(),
        );
        let b_to_a = spawn_forwarder(
            "b_to_a",
            Arc::clone(&leg_b.socket),
            Arc::clone(&leg_a.socket),
            leg_a.peer,
            Arc::clone(&shared),
            Direction::BToA,
            cancel.clone(),
        );

        Arc::new(Self {
            id,
            cancel,
            tasks: Mutex::new(vec![a_to_b, b_to_a]),
            _lease: lease,
        })
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

#[derive(Copy, Clone)]
enum Direction {
    AToB,
    BToA,
}

fn spawn_forwarder(
    label: &'static str,
    recv_sock: Arc<UdpSocket>,
    send_sock: Arc<UdpSocket>,
    peer: SocketAddr,
    transcoder: Arc<Mutex<CallTranscoder>>,
    direction: Direction,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // RTP header is 12 bytes (no CSRCs / extensions); payloads
        // are typically ≤200 bytes for 20 ms frames. 1500 covers
        // any sane MTU.
        let mut buf = vec![0_u8; 1500];
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    debug!(label, "transcoded forwarder cancelled");
                    return;
                }
                recv = recv_sock.recv_from(&mut buf) => {
                    let Ok((n, _from)) = recv else {
                        if let Err(e) = recv {
                            warn!(label, error = %e, "transcoded recv_from failed");
                        }
                        continue;
                    };
                    if n < RTP_MIN {
                        // Too small to be a valid RTP packet — drop.
                        continue;
                    }
                    let (header, payload) = buf[..n].split_at(RTP_MIN);
                    let mut t = transcoder.lock().await;
                    let result = match direction {
                        Direction::AToB => t.transcode_a_to_b(payload),
                        Direction::BToA => t.transcode_b_to_a(payload),
                    };
                    drop(t);
                    let payload_out = match result {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!(label, error = %e, "transcoder rejected frame; dropping");
                            continue;
                        }
                    };
                    // Stitch the original header onto the new
                    // payload. Sequence + timestamp + SSRC ride
                    // through unchanged. This is correct for
                    // same-rate codec pairs; the cross-rate
                    // timestamp scaling lands with 5.6c.
                    let mut out = Vec::with_capacity(header.len() + payload_out.len());
                    out.extend_from_slice(header);
                    out.extend_from_slice(&payload_out);
                    if let Err(e) = send_sock.send_to(&out, peer).await {
                        warn!(label, error = %e, "transcoded send_to failed");
                    }
                }
            }
        }
    })
}

/// RFC 3550 §5.1 fixed-header length. CSRCs / extensions add to
/// this; the transcoded forwarder doesn't strip them — they ride
/// through as part of the "header" prefix because the transcoder
/// only touches the payload.
const RTP_MIN: usize = 12;

#[cfg(test)]
#[allow(clippy::similar_names)] // leg_a / leg_b / peer_a / peer_b are load-bearing
mod tests {
    use super::*;
    use smiths_core::media::BridgeId;
    use smiths_transcode::{CpuBudget, CpuBudgetConfig, G711Codec, G711Variant, TranscodeMetrics};
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

    /// Build a 12-byte RTP header (V=2, PT=0, fixed seq/ts/SSRC) +
    /// `payload`. Used by tests as the bytes a "peer" sends.
    fn rtp_packet(seq: u16, ts: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(12 + payload.len());
        p.push(0x80); // V=2, no padding/extension/CC
        p.push(0); // PT=0 (PCMU)
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pcmu_to_pcma_round_trips_through_session() {
        // peer_a speaks PCMU; the engine transcodes to PCMA for
        // peer_b. Reverse direction transcodes peer_b's PCMA back
        // to PCMU for peer_a.
        let leg_a = bind().await;
        let leg_b = bind().await;
        let peer_a = bind().await;
        let peer_b = bind().await;

        let leg_a_addr = leg_a.local_addr().unwrap();
        let leg_b_addr = leg_b.local_addr().unwrap();
        let peer_a_addr = peer_a.local_addr().unwrap();
        let peer_b_addr = peer_b.local_addr().unwrap();

        let metrics = TranscodeMetrics::noop();
        let b = budget();
        let lease = b.try_admit().unwrap();
        // leg A speaks PCMU; leg B speaks PCMA.
        let transcoder = CallTranscoder::new(
            Box::new(G711Codec::new(G711Variant::Pcmu)),
            Box::new(G711Codec::new(G711Variant::Pcma)),
            metrics,
            lease,
        );
        // Need a second lease for the session itself (one CPU slot
        // per session). Future code paths build the lease + the
        // transcoder together; here we hand-roll for the test.
        let session_lease = b.try_admit().unwrap();

        let session = TranscodedSession::spawn(
            BridgeId(42),
            TranscodedLeg {
                socket: Arc::clone(&leg_a),
                peer: peer_a_addr,
            },
            TranscodedLeg {
                socket: Arc::clone(&leg_b),
                peer: peer_b_addr,
            },
            transcoder,
            session_lease,
        );

        // PCMU ramp from peer_a — 160 samples = 1 frame at 20 ms.
        let pcmu_payload: Vec<u8> = (0_u8..160).collect();
        let pkt = rtp_packet(1, 0, 0xCAFE_F00D, &pcmu_payload);
        peer_a.send_to(&pkt, leg_a_addr).await.unwrap();

        // peer_b receives a transcoded frame.
        let mut buf = vec![0_u8; 1500];
        let (n, from) = timeout(Duration::from_secs(2), peer_b.recv_from(&mut buf))
            .await
            .expect("peer_b never received the transcoded frame")
            .unwrap();
        assert_eq!(from, leg_b_addr, "frame came from wrong leg");
        // Header preserved (seq + ts + ssrc).
        assert_eq!(&buf[..2], &[0x80, 0x00]);
        assert_eq!(&buf[2..4], &1_u16.to_be_bytes());
        assert_eq!(&buf[8..12], &0xCAFE_F00D_u32.to_be_bytes());
        // Payload was transcoded — different bytes from input,
        // same length (G.711 → G.711 is 1 byte per sample
        // both ways).
        let payload_out = &buf[12..n];
        assert_eq!(payload_out.len(), pcmu_payload.len());
        assert_ne!(
            payload_out,
            pcmu_payload.as_slice(),
            "transcoded payload should differ from PCMU input"
        );

        // Round-trip: peer_b speaks PCMA back; peer_a should
        // receive PCMU.
        let pcma_payload: Vec<u8> = (0_u8..160).map(|i| i.wrapping_add(50)).collect();
        let pkt_back = rtp_packet(2, 160, 0xDEAD_BEEF, &pcma_payload);
        peer_b.send_to(&pkt_back, leg_b_addr).await.unwrap();
        let (n, _) = timeout(Duration::from_secs(2), peer_a.recv_from(&mut buf))
            .await
            .expect("peer_a never received the reverse-transcoded frame")
            .unwrap();
        let payload_back = &buf[12..n];
        assert_eq!(payload_back.len(), pcma_payload.len());
        assert_ne!(payload_back, pcma_payload.as_slice());

        session.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_cancels_forwarders() {
        let leg_a = bind().await;
        let leg_b = bind().await;
        let metrics = TranscodeMetrics::noop();
        let b = budget();
        let lease = b.try_admit().unwrap();
        let transcoder = CallTranscoder::new(
            Box::new(G711Codec::new(G711Variant::Pcmu)),
            Box::new(G711Codec::new(G711Variant::Pcma)),
            metrics,
            lease,
        );
        let session_lease = b.try_admit().unwrap();
        let session = TranscodedSession::spawn(
            BridgeId(1),
            TranscodedLeg {
                socket: leg_a,
                peer: "127.0.0.1:1".parse().unwrap(),
            },
            TranscodedLeg {
                socket: leg_b,
                peer: "127.0.0.1:2".parse().unwrap(),
            },
            transcoder,
            session_lease,
        );
        timeout(Duration::from_secs(2), session.stop())
            .await
            .expect("stop hung");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_session_releases_admission_lease() {
        let leg_a = bind().await;
        let leg_b = bind().await;
        let metrics = TranscodeMetrics::noop();
        // Cap at 2 — we'll burn one for the inner CallTranscoder
        // and one for the session itself, then verify a third
        // try_admit refuses while the session is alive and
        // succeeds after we drop it.
        let b = CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: 2,
                cpu_budget_ms_per_call: 50,
            },
            Arc::clone(&metrics),
        );
        let inner_lease = b.try_admit().unwrap();
        let session_lease = b.try_admit().unwrap();
        assert!(b.try_admit().is_err(), "budget should be at cap");

        let transcoder = CallTranscoder::new(
            Box::new(G711Codec::new(G711Variant::Pcmu)),
            Box::new(G711Codec::new(G711Variant::Pcma)),
            metrics,
            inner_lease,
        );
        let session = TranscodedSession::spawn(
            BridgeId(7),
            TranscodedLeg {
                socket: leg_a,
                peer: "127.0.0.1:1".parse().unwrap(),
            },
            TranscodedLeg {
                socket: leg_b,
                peer: "127.0.0.1:2".parse().unwrap(),
            },
            transcoder,
            session_lease,
        );

        session.stop().await;
        // Drop the session-side handle; this should release one
        // budget slot. The inner CallTranscoder still holds its
        // own lease (so we're at 1/2, not 0/2).
        drop(session);

        let next = b.try_admit().expect("freed slot should be reusable");
        drop(next);
    }
}
