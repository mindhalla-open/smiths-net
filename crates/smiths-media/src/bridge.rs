//! Two-leg UDP bridge with per-leg SSRC rewrite (RFC 3550 §5.1).
//!
//! Each leg owns a UDP socket and knows the remote RTP address. The
//! bridge spawns two forwarder tasks — one per direction — that
//! `recv_from` on one leg's socket, rewrite the inbound packet's
//! synchronization source (SSRC) to the engine-chosen value for the
//! outbound leg, and `send_to` the other peer. Payload and sequence
//! numbers are preserved; only the SSRC field (bytes 8..12 of the
//! RTP header) is touched.
//!
//! Non-RTP packets (too short, wrong version) are dropped with a
//! debug log — this bridge is explicitly not a transparent byte pipe.
//!
//! Graceful shutdown is driven by a single `CancellationToken`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaSession};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// One leg of a bridged call.
#[derive(Debug)]
pub struct Leg {
    /// Engine-owned socket for this leg.
    pub socket: Arc<UdpSocket>,
    /// Where the engine should deliver packets for this leg's peer.
    /// Learned from SDP `c=` / `m=` on offer/answer.
    pub peer: SocketAddr,
}

/// Live bridge running two forwarder tasks.
pub struct Bridge {
    id: BridgeId,
    cancel: CancellationToken,
    /// Tasks are `take`-d once when `shutdown` is first awaited, so
    /// the call is idempotent.
    tasks: Mutex<Option<Vec<JoinHandle<()>>>>,
}

impl Bridge {
    /// Start forwarding A ↔ B. Both legs' sockets must already be bound.
    /// The engine advertises a fresh, stable SSRC in each direction so
    /// neither peer's internal SSRC leaks into the other's stream.
    #[must_use]
    pub fn spawn(id: BridgeId, a: &Leg, b: &Leg) -> Self {
        let cancel = CancellationToken::new();
        let ssrc_toward_b = fresh_ssrc();
        let ssrc_toward_a = fresh_ssrc();
        let t_ab = spawn_rewriting_forward(
            Arc::clone(&a.socket),
            Arc::clone(&b.socket),
            b.peer,
            ssrc_toward_b,
            cancel.clone(),
            "a->b",
        );
        let t_ba = spawn_rewriting_forward(
            Arc::clone(&b.socket),
            Arc::clone(&a.socket),
            a.peer,
            ssrc_toward_a,
            cancel.clone(),
            "b->a",
        );
        Self {
            id,
            cancel,
            tasks: Mutex::new(Some(vec![t_ab, t_ba])),
        }
    }

    /// Bridge identifier.
    #[must_use]
    pub const fn id(&self) -> BridgeId {
        self.id
    }

    /// Cancel forwarding and wait for both tasks to exit. Idempotent.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let taken = self.tasks.lock().await.take();
        if let Some(handles) = taken {
            for t in handles {
                let _ = t.await;
            }
        }
    }
}

#[async_trait]
impl MediaSession for Bridge {
    fn id(&self) -> BridgeId {
        self.id
    }
    async fn stop(&self) {
        self.shutdown().await;
    }
}

fn spawn_rewriting_forward(
    recv: Arc<UdpSocket>,
    send: Arc<UdpSocket>,
    dest: SocketAddr,
    ssrc_out: u32,
    cancel: CancellationToken,
    dir: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // RTP frames top out well under an MTU; 2 KB leaves headroom.
        let mut buf = vec![0u8; 2048];
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = recv.recv_from(&mut buf) => match res {
                    Ok((n, _src)) => {
                        if !rewrite_ssrc(&mut buf[..n], ssrc_out) {
                            debug!(dir, bytes = n, "non-RTP packet dropped");
                            continue;
                        }
                        if let Err(e) = send.send_to(&buf[..n], dest).await {
                            warn!(dir, ?e, "bridge send failed");
                        }
                    }
                    Err(e) => {
                        warn!(dir, ?e, "bridge recv failed; stopping direction");
                        break;
                    }
                },
            }
        }
        debug!(dir, "bridge forwarder stopped");
    })
}

/// Overwrite the SSRC field (bytes 8..12) with `ssrc` in-place. Returns
/// `false` if the packet doesn't look like RTP so the caller can drop
/// it. Accepts both RTP (V=2) and the practical case of a minimum 12-
/// byte header; CSRC / extension parsing isn't needed because the SSRC
/// position is fixed.
fn rewrite_ssrc(packet: &mut [u8], ssrc: u32) -> bool {
    if packet.len() < 12 {
        return false;
    }
    // Version in top two bits of byte 0 must be 2.
    if packet[0] >> 6 != 2 {
        return false;
    }
    packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
    true
}

/// Non-cryptographic source for engine-chosen SSRC values. A mix of
/// process-time and a per-process counter avoids collisions across
/// bridges without dragging in `rand`.
fn fresh_ssrc() -> u32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    // Golden-ratio-style mix; good enough for SSRC spread.
    let mixed = nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(c.wrapping_mul(0x0100_0000_01B3));
    (mixed >> 32) as u32
}

#[cfg(test)]
#[allow(clippy::similar_names)] // `ua_a` / `ua_b` etc. is the test convention.
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn bind_udp() -> (Arc<UdpSocket>, SocketAddr) {
        let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let a = s.local_addr().unwrap();
        (Arc::new(s), a)
    }

    /// Minimal RTP packet: V=2, PT=0 (PCMU), seq, ts=0, supplied SSRC,
    /// and a 4-byte payload.
    fn rtp_packet(seq: u16, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(12 + payload.len());
        v.push(0x80); // V=2 P=0 X=0 CC=0
        v.push(0x00); // M=0 PT=0
        v.extend_from_slice(&seq.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        v.extend_from_slice(&ssrc.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn ssrc_of(packet: &[u8]) -> u32 {
        u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]])
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_forwards_both_ways_and_rewrites_ssrc() {
        // Engine's two legs (receives from UA-A / UA-B).
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, addr_engine_b) = bind_udp().await;

        // UA-A and UA-B.
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn(
            BridgeId(1),
            &Leg {
                socket: Arc::clone(&sock_engine_a),
                peer: ua_addr_a,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
            },
        );

        // UA-A sends with its own SSRC; UA-B should see a DIFFERENT SSRC.
        let ua_a_ssrc: u32 = 0xDEAD_BEEF;
        let pkt = rtp_packet(1, ua_a_ssrc, b"hi-a");
        ua_a.send_to(&pkt, addr_engine_a).await.unwrap();
        let mut buf = [0u8; 256];
        let (n, from) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[12..n], b"hi-a", "payload preserved");
        assert_ne!(
            ssrc_of(&buf[..n]),
            ua_a_ssrc,
            "SSRC must be rewritten on egress"
        );
        assert_eq!(from, addr_engine_b);

        // Reverse direction.
        let ua_b_ssrc: u32 = 0xCAFE_BABE;
        let pkt_b = rtp_packet(1, ua_b_ssrc, b"hi-b");
        ua_b.send_to(&pkt_b, addr_engine_b).await.unwrap();
        let (n, _) = timeout(Duration::from_secs(1), ua_a.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[12..n], b"hi-b");
        assert_ne!(ssrc_of(&buf[..n]), ua_b_ssrc);

        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_rtp_traffic_is_dropped() {
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _addr_engine_b) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn(
            BridgeId(2),
            &Leg {
                socket: Arc::clone(&sock_engine_a),
                peer: ua_addr_a,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
            },
        );

        // Not RTP — too short, and wrong version bits.
        ua_a.send_to(b"garbage", addr_engine_a).await.unwrap();
        let mut buf = [0u8; 256];
        // UA-B should receive nothing in 200 ms.
        let r = timeout(Duration::from_millis(200), ua_b.recv_from(&mut buf)).await;
        assert!(r.is_err(), "non-RTP traffic must be dropped");

        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_is_idempotent() {
        let (sock_a, addr_a) = bind_udp().await;
        let (sock_b, addr_b) = bind_udp().await;
        let bridge = Bridge::spawn(
            BridgeId(3),
            &Leg {
                socket: sock_a,
                peer: addr_b,
            },
            &Leg {
                socket: sock_b,
                peer: addr_a,
            },
        );
        bridge.shutdown().await;
        bridge.shutdown().await; // second call must not panic or hang
    }

    #[test]
    fn fresh_ssrcs_are_distinct() {
        let a = fresh_ssrc();
        let b = fresh_ssrc();
        // With a 64-bit mixer the collision probability per pair is
        // astronomical; this guards against a degenerate all-zeros bug.
        assert_ne!(a, b);
    }

    #[test]
    fn rewrite_rejects_short_or_wrong_version() {
        let mut short = [0u8; 8];
        assert!(!rewrite_ssrc(&mut short, 1));
        let mut wrong_version = [0u8; 12];
        wrong_version[0] = 0x40; // V=1
        assert!(!rewrite_ssrc(&mut wrong_version, 1));
    }
}
