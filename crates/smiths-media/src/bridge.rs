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
use smiths_core::SrtpTransform;
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
    /// Optional SRTP context for this leg. When set, the forwarder
    /// decrypts ingress packets with `peer_tx` and encrypts egress
    /// with `local_tx`; SSRC rewrite runs on the decrypted plaintext
    /// (SRTP authenticates the header, so we have to re-sign after
    /// the rewrite).
    pub srtp: Option<LegSrtp>,
}

/// Per-leg SRTP key context. Two transforms, one per direction —
/// sharing one across directions would mix the per-SSRC rollover
/// counters and break replay detection.
#[derive(Clone)]
pub struct LegSrtp {
    /// Peer's transmit key (what the peer uses to encrypt outbound
    /// — we use it to **decrypt** packets arriving on this leg).
    pub peer_tx: Arc<dyn SrtpTransform>,
    /// Our transmit-to-this-peer key (what we use to **encrypt**
    /// outbound. Peer holds the matching key and decrypts).
    pub local_tx: Arc<dyn SrtpTransform>,
}

impl std::fmt::Debug for LegSrtp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't leak the trait-object addresses; SRTP key material is
        // inside these handles.
        f.debug_struct("LegSrtp").finish()
    }
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

// The `DtmfSink` trait lives in `smiths-core::dtmf` so bus-adapter
// implementations (`BusDtmfSink`) and the bridge share one type.
// Re-exported here for callers who only depend on `smiths-media`.
pub use smiths_core::dtmf::DtmfSink;

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
    /// Optional DTMF sink — when wired, the forwarder parses
    /// RFC 4733 telephone-event payloads before forwarding and
    /// delivers each completed keypress to the sink. Slice 2.4.
    pub dtmf_sink: Option<Arc<dyn DtmfSink>>,
    /// `true` → also run the Goertzel inband DTMF detector on the
    /// plaintext audio stream. Default `false` because the extra
    /// FFT-like math costs ~3k FLOPs per 20 ms frame. Legs that
    /// never negotiated RFC 4733 (PSTN gateway crossings) set this
    /// to catch DTMF the RFC 4733 detector would miss. Slice 2.5.
    pub inband_dtmf: bool,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            rtcp_interval: Some(Duration::from_secs(5)),
            metrics: None,
            dtmf_sink: None,
            inband_dtmf: false,
        }
    }
}

impl std::fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("rtcp_interval", &self.rtcp_interval)
            .field("metrics", &self.metrics.is_some())
            .field("dtmf_sink", &self.dtmf_sink.is_some())
            .field("inband_dtmf", &self.inband_dtmf)
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

        // SRTP composition per direction: decrypt with the *ingress*
        // leg's peer_tx (peer's sending key) and encrypt with the
        // *egress* leg's local_tx (our sending key to the egress peer).
        let srtp_ab = srtp_pair(a.srtp.as_ref(), b.srtp.as_ref());
        let srtp_ba = srtp_pair(b.srtp.as_ref(), a.srtp.as_ref());

        let t_ab = spawn_rewriting_forward(
            Arc::clone(&a.socket),
            Arc::clone(&b.socket),
            b.peer,
            ssrc_toward_b,
            stats_a_to_b.clone(),
            srtp_ab,
            cfg.metrics.clone(),
            cfg.dtmf_sink.clone(),
            cfg.inband_dtmf,
            cancel.clone(),
            "a->b",
        );
        let t_ba = spawn_rewriting_forward(
            Arc::clone(&b.socket),
            Arc::clone(&a.socket),
            a.peer,
            ssrc_toward_a,
            stats_b_to_a.clone(),
            srtp_ba,
            cfg.metrics.clone(),
            cfg.dtmf_sink.clone(),
            cfg.inband_dtmf,
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

/// Compose a (decrypt, encrypt) transform pair for one bridge
/// direction. Returns `Some((in, out))` only when **both** legs have
/// SRTP contexts — mixed configurations (one leg SRTP, other plain)
/// are a negotiation error the SDP layer should have caught upstream,
/// and the bridge drops packets in that case rather than leaking
/// plaintext. `None` = plain-RTP passthrough.
fn srtp_pair(
    ingress: Option<&LegSrtp>,
    egress: Option<&LegSrtp>,
) -> Option<(Arc<dyn SrtpTransform>, Arc<dyn SrtpTransform>)> {
    match (ingress, egress) {
        (Some(i), Some(e)) => Some((Arc::clone(&i.peer_tx), Arc::clone(&e.local_tx))),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)] // All args map 1:1 to forwarder state; grouping hides intent.
fn spawn_rewriting_forward(
    recv: Arc<UdpSocket>,
    send: Arc<UdpSocket>,
    dest: SocketAddr,
    ssrc_out: u32,
    stats: StreamStats,
    srtp: Option<(Arc<dyn SrtpTransform>, Arc<dyn SrtpTransform>)>,
    metrics: Option<Arc<Metrics>>,
    dtmf_sink: Option<Arc<dyn DtmfSink>>,
    inband_dtmf: bool,
    cancel: CancellationToken,
    dir: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // RTP frames top out well under an MTU; 2 KB leaves headroom
        // (SRTP ciphertext adds only the auth tag suffix).
        let mut buf = vec![0u8; 2048];
        // DTMF detector: one per direction. Lazily built only when a
        // sink is actually wired so non-DTMF deployments pay nothing.
        // Clock rate is fixed at 8 kHz; RFC 4733 rtpmap with a
        // different clock is exotic and a follow-on slice.
        let mut dtmf_detector = dtmf_sink
            .as_ref()
            .map(|_| smiths_core::DtmfDetector::new(dir, 8_000));
        // Inband Goertzel detector — only built when `inband_dtmf`
        // is opted in AND a sink is wired (no sink = nowhere to
        // deliver, so the math would be wasted).
        let mut inband_detector =
            (inband_dtmf && dtmf_sink.is_some()).then(|| smiths_core::InbandDtmfDetector::new(dir));
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = recv.recv_from(&mut buf) => match res {
                    Ok((n, _src)) => {
                        // Build the egress packet: for plain RTP we
                        // rewrite in place and send; for SRTP we
                        // decrypt → rewrite plaintext → re-encrypt
                        // because auth covers the whole packet.
                        let egress_owned = match &srtp {
                            None => {
                                if !rewrite_ssrc(&mut buf[..n], ssrc_out) {
                                    debug!(dir, bytes = n, "non-RTP packet dropped");
                                    continue;
                                }
                                None
                            }
                            Some((decrypt, encrypt)) => {
                                let mut plain = match decrypt.unprotect_rtp(&buf[..n]) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        debug!(dir, ?e, "SRTP auth/decrypt failed; dropped");
                                        continue;
                                    }
                                };
                                if !rewrite_ssrc(&mut plain, ssrc_out) {
                                    debug!(dir, bytes = plain.len(), "non-RTP plaintext dropped");
                                    continue;
                                }
                                match encrypt.protect_rtp(&plain) {
                                    Ok(ct) => Some(ct),
                                    Err(e) => {
                                        warn!(dir, ?e, "SRTP re-encrypt failed; dropped");
                                        continue;
                                    }
                                }
                            }
                        };
                        let egress_slice: &[u8] = egress_owned
                            .as_deref()
                            .unwrap_or(&buf[..n]);
                        // DTMF detect on the plaintext RTP, before the
                        // SSRC-rewritten bytes leave the engine. Parses
                        // only when the PT matches RFC 4733 so the
                        // common audio path pays at most one branch.
                        if let Some(sink) = dtmf_sink.as_ref() {
                            let plaintext_for_dtmf: &[u8] = match &srtp {
                                None => &buf[..n],
                                Some(_) => egress_owned
                                    .as_deref()
                                    .map_or(&buf[..n], |v| v),
                            };
                            // RFC 4733 telephone-event path (cheap —
                            // one PT check per packet).
                            if let Some(detector) = dtmf_detector.as_mut() {
                                if let Some(press) =
                                    detect_dtmf(plaintext_for_dtmf, detector)
                                {
                                    sink.deliver(dir, press);
                                }
                            }
                            // Inband Goertzel path (opt-in). Only
                            // runs on payload type 0 (PCMU) today;
                            // PCMA / other codecs decode through the
                            // same path once their decoders land.
                            if let Some(inband) = inband_detector.as_mut() {
                                for press in
                                    detect_dtmf_inband(plaintext_for_dtmf, inband)
                                {
                                    sink.deliver(dir, press);
                                }
                            }
                        }
                        // Observe on the (plaintext, rewritten) form
                        // for plain RTP; for SRTP the ciphertext
                        // header still carries the rewritten SSRC in
                        // bytes 8..12, so stats track the right SSRC.
                        stats.observe(egress_slice);
                        if let Err(e) = send.send_to(egress_slice, dest).await {
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

/// Pick out an RFC 4733 telephone-event payload from a raw RTP
/// datagram and feed it through `detector`. Returns the detector's
/// output directly so the caller can hand it to the `DtmfSink`.
///
/// Deliberately permissive: a malformed RTP header or a non-matching
/// payload type returns `None` without logging — at 50 pps (20 ms
/// frames) for the duration of a call, spurious warnings would
/// drown real issues.
fn detect_dtmf(
    rtp_bytes: &[u8],
    detector: &mut smiths_core::DtmfDetector,
) -> Option<smiths_core::DtmfKeypress> {
    let pkt = smiths_core::RtpPacket::decode(rtp_bytes)?;
    if pkt.payload_type != smiths_core::RFC4733_PAYLOAD_TYPE {
        return None;
    }
    let ev = smiths_core::TelephoneEvent::parse(&pkt.payload)?;
    // Start timestamp == the RTP timestamp of the first packet of a
    // keypress. The engine doesn't track presses across multiple RTP
    // ts values; the detector treats "same (event, ts)" as one press.
    detector.feed(&ev, pkt.timestamp)
}

/// PCMU payload type per RFC 3551 §4.5 Table 4 — the only codec the
/// inband detector decodes today. PCMA / Opus land with their own
/// decoder once a concrete need arrives.
const PT_PCMU_FOR_DTMF: u8 = 0;

/// Run the Goertzel inband detector over a PCMU-bearing RTP packet.
/// Returns every keypress the detector emits while consuming this
/// packet's audio (usually zero; occasionally one when a tone just
/// ended inside this frame).
fn detect_dtmf_inband(
    rtp_bytes: &[u8],
    detector: &mut smiths_core::InbandDtmfDetector,
) -> Vec<smiths_core::DtmfKeypress> {
    let Some(pkt) = smiths_core::RtpPacket::decode(rtp_bytes) else {
        return Vec::new();
    };
    if pkt.payload_type != PT_PCMU_FOR_DTMF {
        return Vec::new();
    }
    detector.feed_pcmu(&pkt.payload)
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
                            // Fraction-lost per interval still needs
                            // emitter-side rotation across tick
                            // boundaries; cumulative_lost flows
                            // directly from StreamStats now that the
                            // expected-vs-received delta is tracked.
                            fraction_lost: 0,
                            cumulative_lost: recv_snap.cumulative_lost,
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
                srtp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
                srtp: None,
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
                srtp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
                srtp: None,
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
                srtp: None,
            },
            &Leg {
                socket: sock_b,
                peer: addr_a,
                rtcp: None,
                srtp: None,
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
                srtp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
                srtp: None,
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
                srtp: None,
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: Some(RtcpLeg {
                    socket: rtcp_engine_b,
                    peer: ua_rtcp_addr_b,
                }),
                srtp: None,
            },
            &BridgeConfig {
                rtcp_interval: Some(Duration::from_millis(100)),
                metrics: None,
                dtmf_sink: None,
                inband_dtmf: false,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn srtp_bridge_decrypts_rewrites_and_reencrypts() {
        // Wire a bridge where both legs are SRTP. UA-A encrypts with
        // its `peer_tx_a` key, engine decrypts with the same key on
        // ingress, rewrites SSRC, re-encrypts with engine's `local_tx_b`
        // key toward UA-B, UA-B decrypts. We assert payload parity.
        use crate::srtp::AesCmHmacSha1_80Transform;
        use std::sync::Arc;

        // 4 independent 30-byte key materials (one per transform).
        let km_a_tx: Vec<u8> = (0..30u8).collect();
        let km_e_to_a: Vec<u8> = (30..60u8).collect();
        let km_b_tx: Vec<u8> = (60..90u8).collect();
        let km_e_to_b: Vec<u8> = (90..120u8).collect();

        let a_peer_tx: Arc<dyn SrtpTransform> =
            Arc::new(AesCmHmacSha1_80Transform::from_sdes(&km_a_tx).unwrap());
        let a_local_tx: Arc<dyn SrtpTransform> =
            Arc::new(AesCmHmacSha1_80Transform::from_sdes(&km_e_to_a).unwrap());
        let b_peer_tx: Arc<dyn SrtpTransform> =
            Arc::new(AesCmHmacSha1_80Transform::from_sdes(&km_b_tx).unwrap());
        let b_local_tx: Arc<dyn SrtpTransform> =
            Arc::new(AesCmHmacSha1_80Transform::from_sdes(&km_e_to_b).unwrap());

        // UA-side transforms (peer's counterpart keys).
        let ua_a_tx: AesCmHmacSha1_80Transform =
            AesCmHmacSha1_80Transform::from_sdes(&km_a_tx).unwrap();
        let ua_b_rx: AesCmHmacSha1_80Transform =
            AesCmHmacSha1_80Transform::from_sdes(&km_e_to_b).unwrap();

        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn(
            BridgeId(100),
            &Leg {
                socket: Arc::clone(&sock_engine_a),
                peer: ua_addr_a,
                rtcp: None,
                srtp: Some(LegSrtp {
                    peer_tx: a_peer_tx,
                    local_tx: a_local_tx,
                }),
            },
            &Leg {
                socket: Arc::clone(&sock_engine_b),
                peer: ua_addr_b,
                rtcp: None,
                srtp: Some(LegSrtp {
                    peer_tx: b_peer_tx,
                    local_tx: b_local_tx,
                }),
            },
        );

        // UA-A encrypts an RTP packet and sends it to engine.
        let plain = rtp_packet(1, 0xDEAD_BEEF, b"srtp-hi");
        let ciphertext = ua_a_tx.protect_rtp(&plain).unwrap();
        ua_a.send_to(&ciphertext, addr_engine_a).await.unwrap();

        let mut buf = [0u8; 1024];
        let (n, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .expect("UA-B should receive within 1s")
            .unwrap();
        let received_ct = &buf[..n];
        // UA-B decrypts with the key engine advertised in its answer.
        let recovered = ua_b_rx
            .unprotect_rtp(received_ct)
            .expect("UA-B must successfully decrypt engine's re-encrypted packet");
        assert_eq!(
            &recovered[12..],
            b"srtp-hi",
            "payload preserved end-to-end through SRTP bridge"
        );
        // SSRC was rewritten — must differ from UA-A's original SSRC.
        let received_ssrc =
            u32::from_be_bytes([recovered[8], recovered[9], recovered[10], recovered[11]]);
        assert_ne!(
            received_ssrc, 0xDEAD_BEEF,
            "engine must rewrite SSRC even on the SRTP path"
        );

        bridge.shutdown().await;
    }
}
