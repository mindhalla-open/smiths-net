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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use smiths_core::Metrics;
use smiths_core::media::{BridgeId, MediaSession};
use smiths_core::metrics::RtpDirLabel;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::rtcp::{ReportBlock, build_sr, build_sr_with_rb, ntp_now, parse_rr};
use crate::rtp_stats::StreamStats;

/// One leg of a bridged call.
#[derive(Debug)]
pub struct Leg {
    /// Engine-owned RTP socket for this leg.
    pub socket: Arc<UdpSocket>,
    /// Where the engine should deliver RTP packets for this leg's
    /// peer. Learned from SDP `c=` / `m=` on offer/answer.
    pub peer: SocketAddr,
    /// Optional RTCP attachment. When set, the bridge spawns a
    /// periodic emitter that writes SR packets to `rtcp.peer` on
    /// `rtcp.socket`. `None` disables RTCP for this direction.
    pub rtcp: Option<RtcpLeg>,
}

/// RTCP half of one leg — a socket + the peer's RTCP address. The
/// socket is distinct from the RTP socket by RFC 3550 §11 (even/odd
/// port pair).
#[derive(Debug, Clone)]
pub struct RtcpLeg {
    /// Engine-owned RTCP socket (bound on RTP-port + 1).
    pub socket: Arc<UdpSocket>,
    /// Peer's RTCP destination. Derived by the fabric from
    /// `peer_rtp_port + 1` when the SDP doesn't explicitly carry
    /// `a=rtcp:` lines.
    pub peer: SocketAddr,
}

/// Runtime tunables for a bridge.
#[derive(Clone)]
pub struct BridgeConfig {
    /// How often to emit RTCP Sender Reports. `None` disables
    /// emission entirely even if the legs carry RTCP sockets.
    ///
    /// RFC 3550 §6.2 recommends ~5 s as the default for low-rate
    /// flows; tests override with tighter values.
    pub rtcp_interval: Option<Duration>,
    /// Optional metrics handle. When present, forwarders and the SR
    /// emitter increment `rtp_packets_forwarded` / `rtcp_sr_sent`;
    /// when absent (tests, embedded use) the bridge runs silently.
    pub metrics: Option<std::sync::Arc<Metrics>>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            rtcp_interval: Some(Duration::from_secs(5)),
            metrics: None,
        }
    }
}

impl std::fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("rtcp_interval", &self.rtcp_interval)
            .field("metrics", &self.metrics.is_some())
            .finish()
    }
}

/// Live bridge running two forwarder tasks (+ one RTCP emitter task
/// per direction if configured).
pub struct Bridge {
    id: BridgeId,
    cancel: CancellationToken,
    /// Tasks are `take`-d once when `shutdown` is first awaited, so
    /// the call is idempotent.
    tasks: Mutex<Option<Vec<JoinHandle<()>>>>,
    /// Stats for packets flowing a→b (as observed by the engine on
    /// leg `a`). Cheap to clone.
    stats_a_to_b: StreamStats,
    /// Stats for packets flowing b→a.
    stats_b_to_a: StreamStats,
}

impl Bridge {
    /// Start forwarding A ↔ B with default config (5 s RTCP interval
    /// when the legs carry RTCP sockets). Legs without an `rtcp`
    /// field fall back to RTP-only, matching prior behavior.
    #[must_use]
    pub fn spawn(id: BridgeId, a: &Leg, b: &Leg) -> Self {
        Self::spawn_with(id, a, b, &BridgeConfig::default())
    }

    /// Start forwarding A ↔ B with caller-supplied config. The engine
    /// advertises a fresh SSRC per direction so no peer's internal
    /// SSRC leaks into the other's stream. Each forwarder updates the
    /// corresponding `StreamStats` as packets pass; if both legs
    /// carry RTCP sockets and `cfg.rtcp_interval` is `Some`, a per-
    /// direction emitter task writes Sender Reports on each tick.
    #[must_use]
    pub fn spawn_with(id: BridgeId, a: &Leg, b: &Leg, cfg: &BridgeConfig) -> Self {
        let cancel = CancellationToken::new();
        let ssrc_toward_b = fresh_ssrc();
        let ssrc_toward_a = fresh_ssrc();
        let stats_a_to_b = StreamStats::new();
        let stats_b_to_a = StreamStats::new();

        let t_ab = spawn_rewriting_forward(
            Arc::clone(&a.socket),
            Arc::clone(&b.socket),
            b.peer,
            ssrc_toward_b,
            stats_a_to_b.clone(),
            cfg.metrics.clone(),
            cancel.clone(),
            "a->b",
        );
        let t_ba = spawn_rewriting_forward(
            Arc::clone(&b.socket),
            Arc::clone(&a.socket),
            a.peer,
            ssrc_toward_a,
            stats_b_to_a.clone(),
            cfg.metrics.clone(),
            cancel.clone(),
            "b->a",
        );

        let mut tasks = vec![t_ab, t_ba];
        if let Some(interval) = cfg.rtcp_interval {
            // For each direction, spawn an emitter that sends SRs on
            // the *outbound* peer's RTCP port. a->b stats describe
            // what we forward to B, so B's RTCP socket gets the SR.
            if let Some(rtcp_b) = &b.rtcp {
                tasks.push(spawn_sr_emitter(
                    rtcp_b.clone(),
                    ssrc_toward_b,
                    stats_a_to_b.clone(),
                    // RR block inside SR reports on what we've
                    // RECEIVED from peer B (the b->a stream).
                    stats_b_to_a.clone(),
                    interval,
                    cfg.metrics.clone(),
                    cancel.clone(),
                    "a->b",
                ));
                // Listen on the RTCP socket for peer B's own RRs so
                // we observe its view of our outbound.
                tasks.push(spawn_rr_listener(
                    Arc::clone(&rtcp_b.socket),
                    cancel.clone(),
                    "a->b",
                ));
            }
            if let Some(rtcp_a) = &a.rtcp {
                tasks.push(spawn_sr_emitter(
                    rtcp_a.clone(),
                    ssrc_toward_a,
                    stats_b_to_a.clone(),
                    stats_a_to_b.clone(),
                    interval,
                    cfg.metrics.clone(),
                    cancel.clone(),
                    "b->a",
                ));
                tasks.push(spawn_rr_listener(
                    Arc::clone(&rtcp_a.socket),
                    cancel.clone(),
                    "b->a",
                ));
            }
        }

        Self {
            id,
            cancel,
            tasks: Mutex::new(Some(tasks)),
            stats_a_to_b,
            stats_b_to_a,
        }
    }

    /// Bridge identifier.
    #[must_use]
    pub const fn id(&self) -> BridgeId {
        self.id
    }

    /// Snapshot the two direction-stats for observability. Cheap —
    /// atomics only, no locking.
    #[must_use]
    pub fn stats(&self) -> BridgeStats {
        BridgeStats {
            a_to_b: self.stats_a_to_b.snapshot(),
            b_to_a: self.stats_b_to_a.snapshot(),
        }
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

/// Paired stats snapshot for a bridge, one per direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStats {
    /// Packets observed going engine-leg-A → peer-B.
    pub a_to_b: crate::rtp_stats::StreamStatsSnapshot,
    /// Packets observed going engine-leg-B → peer-A.
    pub b_to_a: crate::rtp_stats::StreamStatsSnapshot,
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

#[allow(clippy::too_many_arguments)] // All args map 1:1 to forwarder state; grouping hides intent.
fn spawn_rewriting_forward(
    recv: Arc<UdpSocket>,
    send: Arc<UdpSocket>,
    dest: SocketAddr,
    ssrc_out: u32,
    stats: StreamStats,
    metrics: Option<Arc<Metrics>>,
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
                        // Observe AFTER rewrite so the recorded SSRC
                        // matches what we just emitted — downstream
                        // SR reports reference it.
                        stats.observe(&buf[..n]);
                        if let Err(e) = send.send_to(&buf[..n], dest).await {
                            warn!(dir, ?e, "bridge send failed");
                        } else if let Some(m) = &metrics {
                            m.rtp_packets_forwarded
                                .get_or_create(&RtpDirLabel {
                                    direction: dir.replace("->", "_to_"),
                                })
                                .inc();
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

/// Fire an RTCP Sender Report on each tick.
///
/// The SR body reports on our **outbound** stream (`sender_stats`);
/// the embedded RR block reports on the **inbound** stream we're
/// receiving from this peer (`receiver_stats`). If the inbound stream
/// hasn't seen any packets yet we fall back to a bare SR — RFC 3550
/// allows RC=0 when the receiver has nothing to report.
#[allow(clippy::too_many_arguments)] // All args map 1:1 to emitter state.
fn spawn_sr_emitter(
    rtcp: RtcpLeg,
    sender_ssrc: u32,
    sender_stats: StreamStats,
    receiver_stats: StreamStats,
    interval: Duration,
    metrics: Option<Arc<Metrics>>,
    cancel: CancellationToken,
    dir: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let send_snap = sender_stats.snapshot();
                    let recv_snap = receiver_stats.snapshot();
                    let pkt_bytes = if recv_snap.packets == 0 {
                        // No inbound yet — emit bare SR (RC=0).
                        build_sr(
                            sender_ssrc,
                            ntp_now(),
                            send_snap.last_rtp_ts,
                            u32::try_from(send_snap.packets).unwrap_or(u32::MAX),
                            u32::try_from(send_snap.octets).unwrap_or(u32::MAX),
                        ).to_vec()
                    } else {
                        let rb = ReportBlock {
                            ssrc: recv_snap.last_ssrc,
                            // Fraction / cumulative loss aren't tracked
                            // yet (no expected-vs-received gap
                            // detection); fill with 0 for now and
                            // tighten when the FSM slice lands.
                            fraction_lost: 0,
                            cumulative_lost: 0,
                            extended_highest_seq: recv_snap.max_seq,
                            jitter: recv_snap.jitter,
                            // last_sr / DLSR require tracking incoming
                            // SRs; RR listener (spawn_rr_listener) is
                            // where those will wire up.
                            last_sr: 0,
                            delay_since_last_sr: 0,
                        };
                        build_sr_with_rb(
                            sender_ssrc,
                            ntp_now(),
                            send_snap.last_rtp_ts,
                            u32::try_from(send_snap.packets).unwrap_or(u32::MAX),
                            u32::try_from(send_snap.octets).unwrap_or(u32::MAX),
                            &rb,
                        ).to_vec()
                    };
                    if let Err(e) = rtcp.socket.send_to(&pkt_bytes, rtcp.peer).await {
                        warn!(dir, ?e, "RTCP SR send failed");
                    } else if let Some(m) = &metrics {
                        m.rtcp_sr_sent.inc();
                    }
                }
            }
        }
        debug!(dir, "bridge RTCP emitter stopped");
    })
}

/// Listen for RTCP RR packets from the peer. Parses, logs a debug
/// line, and drops — hooking RR feedback into `StreamStats` (loss %,
/// DLSR tracking) is follow-on work that needs the cumulative-lost
/// counter wired on the emitter side first.
fn spawn_rr_listener(
    socket: Arc<tokio::net::UdpSocket>,
    cancel: CancellationToken,
    dir: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = vec![0u8; 1500];
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = socket.recv_from(&mut buf) => match res {
                    Ok((n, _src)) => {
                        if let Some((sender, blocks)) = parse_rr(&buf[..n]) {
                            for rb in &blocks {
                                debug!(
                                    dir, sender, ssrc = rb.ssrc,
                                    fraction_lost = rb.fraction_lost,
                                    cumulative_lost = rb.cumulative_lost,
                                    jitter = rb.jitter,
                                    "peer RR received"
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!(dir, ?e, "RTCP recv failed; stopping listener");
                        break;
                    }
                }
            }
        }
        debug!(dir, "bridge RTCP listener stopped");
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
                rtcp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
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
                rtcp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
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
                rtcp: None,
            },
            &Leg {
                socket: sock_b,
                peer: addr_a,
                rtcp: None,
            },
        );
        bridge.shutdown().await;
        bridge.shutdown().await; // second call must not panic or hang
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stats_update_as_rtp_packets_flow() {
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn(
            BridgeId(4),
            &Leg {
                socket: Arc::clone(&sock_engine_a),
                peer: ua_addr_a,
                rtcp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
            },
        );

        for seq in 1u16..=3 {
            ua_a.send_to(&rtp_packet(seq, 0xAAAA, b"xxx"), addr_engine_a)
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            let _ = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
        }

        let stats = bridge.stats();
        assert_eq!(stats.a_to_b.packets, 3);
        assert_eq!(stats.a_to_b.max_seq, 3);
        assert_eq!(stats.b_to_a.packets, 0);
        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rtcp_sr_is_emitted_on_interval() {
        use crate::rtcp::parse_sr;

        let (sock_engine_a, _) = bind_udp().await;
        let (sock_engine_b, addr_engine_b) = bind_udp().await;
        let (rtcp_engine_a, _) = bind_udp().await;
        let (rtcp_engine_b, _) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;
        let (ua_rtcp_a, ua_rtcp_addr_a) = bind_udp().await;
        let (ua_rtcp_b, ua_rtcp_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn_with(
            BridgeId(5),
            &Leg {
                socket: Arc::clone(&sock_engine_a),
                peer: ua_addr_a,
                rtcp: Some(RtcpLeg {
                    socket: rtcp_engine_a,
                    peer: ua_rtcp_addr_a,
                }),
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: Some(RtcpLeg {
                    socket: rtcp_engine_b,
                    peer: ua_rtcp_addr_b,
                }),
            },
            &BridgeConfig {
                rtcp_interval: Some(Duration::from_millis(100)),
                metrics: None,
            },
        );

        // Push one packet a→b so the SR for that direction has a non-
        // trivial body.
        ua_a.send_to(
            &rtp_packet(1, 0xDEAD, b"yyy"),
            sock_engine_a.local_addr().unwrap(),
        )
        .await
        .unwrap();
        let mut sink = [0u8; 64];
        let _ = timeout(Duration::from_secs(1), ua_b.recv_from(&mut sink))
            .await
            .unwrap()
            .unwrap();

        // Wait for B's RTCP peer-socket (ua_rtcp_b) to receive the
        // first SR. The emitter skips the immediate tick, so the
        // first one lands at t≈200 ms.
        let mut rtcp_buf = [0u8; 256];
        let (n, _) = timeout(Duration::from_secs(2), ua_rtcp_b.recv_from(&mut rtcp_buf))
            .await
            .expect("SR should arrive within 2 s")
            .unwrap();
        let sr = parse_sr(&rtcp_buf[..n]).expect("parse SR");
        assert_eq!(sr.packet_count, 1);

        // The other direction's RTCP also gets an SR (with zero
        // packets but syntactically valid).
        let (n2, _) = timeout(Duration::from_secs(2), ua_rtcp_a.recv_from(&mut rtcp_buf))
            .await
            .expect("b→a SR should arrive")
            .unwrap();
        assert!(parse_sr(&rtcp_buf[..n2]).is_some());

        let _ = addr_engine_b;
        bridge.shutdown().await;
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
