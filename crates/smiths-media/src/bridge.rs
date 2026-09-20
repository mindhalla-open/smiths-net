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
//! The bridge **terminates RTCP** rather than forwarding it: each leg
//! gets its own compound Sender Report (sender info about what the
//! engine forwards to that peer, a report block about what it
//! receives from that peer, and an SDES CNAME), and whatever the peer
//! sends back — on the RTCP socket or multiplexed onto the RTP socket
//! per RFC 5761 — is parsed and folded into the per-direction
//! [`StreamStats`] (last SR for `LSR`/`DLSR`, the peer's loss/jitter
//! view and the round-trip time derived from it). Peer RTCP is never
//! SSRC-rewritten or relayed to the other leg.
//!
//! On SRTP legs the forwarders decrypt with the ingress peer's key and
//! re-encrypt with the engine's egress key (SRTP authenticates the
//! header, so the SSRC rewrite has to happen on plaintext), and RTCP
//! is protected the same way with SRTCP. Replay protection is on for
//! both; replayed packets are dropped before they touch the stats.
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
use prometheus_client::metrics::counter::Counter;
use smiths_core::media::{BridgeId, DEFAULT_RTP_CLOCK_RATE, MediaSession};
use smiths_core::metrics::RtpDirLabel;
use smiths_core::rtp::RtpHeader;
use smiths_core::{Metrics, SrtpError, SrtpTransform};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::rtcp::{
    ReportBlock, RtcpPacket, build_compound, build_sdes_cname, build_sr, build_sr_with_rb, is_rtcp,
    ntp_middle, ntp_now, parse_compound, round_trip_time, to_dlsr,
};
use crate::rtp_stats::{IntervalLoss, StreamStats};

/// One leg of a bridged call.
#[derive(Debug)]
pub struct Leg {
    /// Engine-owned RTP socket for this leg.
    pub socket: Arc<UdpSocket>,
    /// Where the engine should deliver RTP packets for this leg's
    /// peer. Learned from SDP `c=` / `m=` on offer/answer.
    pub peer: SocketAddr,
    /// Optional RTCP attachment. When set, the bridge spawns a
    /// periodic emitter that writes compound SR packets to
    /// `rtcp.peer` on `rtcp.socket`, and — when `rtcp.socket` is a
    /// socket of its own — a listener that consumes the peer's RTCP.
    /// `None` disables RTCP emission for this leg; RTCP the peer
    /// multiplexes onto the RTP socket is still consumed.
    pub rtcp: Option<RtcpLeg>,
    /// Optional SRTP context for this leg. When set, the forwarder
    /// decrypts ingress packets with `peer_tx` and encrypts egress
    /// with `local_tx`; SSRC rewrite runs on the decrypted plaintext
    /// (SRTP authenticates the header, so we have to re-sign after
    /// the rewrite). RTCP on this leg is protected with SRTCP using
    /// the same two transforms.
    pub srtp: Option<LegSrtp>,
    /// RTP clock rate of the negotiated codec, in Hz. Drives the
    /// jitter estimator and the RFC 4733 detector's timing.
    pub clock_rate: u32,
}

impl Leg {
    /// Plain-RTP leg with no RTCP attachment at the default 8 kHz
    /// clock. Set the remaining fields after construction.
    #[must_use]
    pub fn new(socket: Arc<UdpSocket>, peer: SocketAddr) -> Self {
        Self {
            socket,
            peer,
            rtcp: None,
            srtp: None,
            clock_rate: DEFAULT_RTP_CLOCK_RATE,
        }
    }
}

/// Per-leg SRTP key context. Two transforms, one per direction, each
/// carrying its own per-SSRC rollover counters and replay windows for
/// both SRTP and SRTCP.
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

/// RTCP half of one leg — a socket + the peer's RTCP address. With
/// the RFC 3550 §11 even/odd port pair this is the leg's own RTCP
/// socket; with `a=rtcp-mux` (RFC 5761) it is the RTP socket itself
/// and `peer` equals the RTP peer address.
#[derive(Debug, Clone)]
pub struct RtcpLeg {
    /// Socket the engine sends RTCP from (and, when it isn't the RTP
    /// socket, listens for the peer's RTCP on).
    pub socket: Arc<UdpSocket>,
    /// Peer's RTCP destination.
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
    /// delivers each completed keypress to the sink.
    pub dtmf_sink: Option<Arc<dyn DtmfSink>>,
    /// `true` → also run the Goertzel inband DTMF detector on the
    /// plaintext audio stream. Default `false` because the extra
    /// FFT-like math costs ~3k FLOPs per 20 ms frame. Legs that
    /// never negotiated RFC 4733 (PSTN gateway crossings) set this
    /// to catch DTMF the RFC 4733 detector would miss.
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

/// Live bridge running two forwarder tasks (+ RTCP emitter and
/// listener tasks per leg if configured).
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
    /// SSRC the engine uses on packets it sends to peer A.
    ssrc_toward_a: u32,
    /// SSRC the engine uses on packets it sends to peer B.
    ssrc_toward_b: u32,
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
    /// corresponding `StreamStats` as packets pass; legs with RTCP
    /// sockets get an emitter task (when `cfg.rtcp_interval` is
    /// `Some`) and a listener task.
    #[must_use]
    pub fn spawn_with(id: BridgeId, a: &Leg, b: &Leg, cfg: &BridgeConfig) -> Self {
        let cancel = CancellationToken::new();
        let ssrc_toward_b = fresh_ssrc();
        let ssrc_toward_a = fresh_ssrc();
        // Each direction's stats describe the stream received on its
        // ingress leg, so the jitter clock is that leg's codec clock.
        let stats_a_to_b = StreamStats::with_clock_rate(a.clock_rate);
        let stats_b_to_a = StreamStats::with_clock_rate(b.clock_rate);

        // RTCP arriving from each peer: SRs describe the stream we
        // receive from it, report blocks describe the stream we send
        // to it.
        let rtcp_from_a = RtcpIngress {
            unprotect: a.srtp.as_ref().map(|s| Arc::clone(&s.peer_tx)),
            recv_stats: stats_a_to_b.clone(),
            send_stats: stats_b_to_a.clone(),
            our_ssrc: ssrc_toward_a,
            dir: "a->b",
        };
        let rtcp_from_b = RtcpIngress {
            unprotect: b.srtp.as_ref().map(|s| Arc::clone(&s.peer_tx)),
            recv_stats: stats_b_to_a.clone(),
            send_stats: stats_a_to_b.clone(),
            our_ssrc: ssrc_toward_b,
            dir: "b->a",
        };

        let forwarded_counter = |dir: &str| {
            cfg.metrics.as_ref().map(|m| {
                m.rtp_packets_forwarded
                    .get_or_create(&RtpDirLabel {
                        direction: dir.replace("->", "_to_"),
                    })
                    .clone()
            })
        };

        let t_ab = spawn_forwarder(Forwarder {
            recv: Arc::clone(&a.socket),
            send: Arc::clone(&b.socket),
            dest: b.peer,
            ssrc_out: ssrc_toward_b,
            stats: stats_a_to_b.clone(),
            srtp: srtp_pair(a.srtp.as_ref(), b.srtp.as_ref()),
            forwarded: forwarded_counter("a->b"),
            dtmf_sink: cfg.dtmf_sink.clone(),
            inband_dtmf: cfg.inband_dtmf,
            clock_rate: a.clock_rate,
            rtcp: rtcp_from_a.clone(),
            cancel: cancel.clone(),
            dir: "a->b",
        });
        let t_ba = spawn_forwarder(Forwarder {
            recv: Arc::clone(&b.socket),
            send: Arc::clone(&a.socket),
            dest: a.peer,
            ssrc_out: ssrc_toward_a,
            stats: stats_b_to_a.clone(),
            srtp: srtp_pair(b.srtp.as_ref(), a.srtp.as_ref()),
            forwarded: forwarded_counter("b->a"),
            dtmf_sink: cfg.dtmf_sink.clone(),
            inband_dtmf: cfg.inband_dtmf,
            clock_rate: b.clock_rate,
            rtcp: rtcp_from_b.clone(),
            cancel: cancel.clone(),
            dir: "b->a",
        });

        let mut tasks = vec![t_ab, t_ba];
        let sr_counter = cfg.metrics.as_ref().map(|m| m.rtcp_sr_sent.clone());
        let mut attach_rtcp = |leg: &Leg, ingress: RtcpIngress, sender_ssrc: u32| {
            let Some(rtcp) = &leg.rtcp else {
                return;
            };
            if let Some(interval) = cfg.rtcp_interval {
                tasks.push(spawn_sr_emitter(RtcpEmitter {
                    rtcp: rtcp.clone(),
                    sender_ssrc,
                    sender_stats: ingress.send_stats.clone(),
                    receiver_stats: ingress.recv_stats.clone(),
                    protect: leg.srtp.as_ref().map(|s| Arc::clone(&s.local_tx)),
                    interval,
                    sr_sent: sr_counter.clone(),
                    cancel: cancel.clone(),
                    dir: ingress.dir,
                }));
            }
            // With rtcp-mux the RTCP socket *is* the RTP socket and the
            // forwarder already demuxes RTCP off it; a second reader
            // on the same socket would steal RTP datagrams.
            if !Arc::ptr_eq(&rtcp.socket, &leg.socket) {
                tasks.push(spawn_rtcp_listener(
                    Arc::clone(&rtcp.socket),
                    ingress,
                    cancel.clone(),
                ));
            }
        };
        attach_rtcp(b, rtcp_from_b, ssrc_toward_b);
        attach_rtcp(a, rtcp_from_a, ssrc_toward_a);

        Self {
            id,
            cancel,
            tasks: Mutex::new(Some(tasks)),
            stats_a_to_b,
            stats_b_to_a,
            ssrc_toward_a,
            ssrc_toward_b,
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
            ssrc_toward_a: self.ssrc_toward_a,
            ssrc_toward_b: self.ssrc_toward_b,
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
    /// SSRC the engine stamps on packets sent to peer A.
    pub ssrc_toward_a: u32,
    /// SSRC the engine stamps on packets sent to peer B.
    pub ssrc_toward_b: u32,
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

/// Everything one forwarder task needs.
struct Forwarder {
    recv: Arc<UdpSocket>,
    send: Arc<UdpSocket>,
    dest: SocketAddr,
    ssrc_out: u32,
    stats: StreamStats,
    srtp: Option<(Arc<dyn SrtpTransform>, Arc<dyn SrtpTransform>)>,
    /// Pre-resolved `rtp_packets_forwarded{direction}` counter so the
    /// hot path does one atomic increment, not a label lookup.
    forwarded: Option<Counter>,
    dtmf_sink: Option<Arc<dyn DtmfSink>>,
    inband_dtmf: bool,
    clock_rate: u32,
    /// Handler for RTCP the peer multiplexes onto the RTP socket.
    rtcp: RtcpIngress,
    cancel: CancellationToken,
    dir: &'static str,
}

fn spawn_forwarder(f: Forwarder) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Forwarder {
            recv,
            send,
            dest,
            ssrc_out,
            stats,
            srtp,
            forwarded,
            dtmf_sink,
            inband_dtmf,
            clock_rate,
            rtcp,
            cancel,
            dir,
        } = f;
        // RTP frames top out well under an MTU; 2 KB leaves headroom
        // (SRTP ciphertext adds only the auth tag suffix).
        let mut buf = vec![0u8; 2048];
        // DTMF detectors: one per direction, built only when a sink is
        // wired so non-DTMF deployments pay nothing. The inband
        // Goertzel detector additionally needs the opt-in flag.
        let mut dtmf_detector = dtmf_sink
            .as_ref()
            .map(|_| smiths_core::DtmfDetector::new(dir, clock_rate));
        let mut inband_detector =
            (inband_dtmf && dtmf_sink.is_some()).then(|| smiths_core::InbandDtmfDetector::new(dir));
        loop {
            tokio::select! {
                           biased;
                           () = cancel.cancelled() => break,
                           res = recv.recv_from(&mut buf) => match res {
                               Ok((n, _src)) => {
            // RFC 5761: RTCP multiplexed onto the RTP port is
            // terminated here, never SSRC-rewritten or relayed.
                                   if is_rtcp(&buf[..n]) {
                                       rtcp.handle(&buf[..n]);
                                       continue;
                                   }
            // Plain RTP is rewritten in place; SRTP is
            // decrypted, rewritten, re-encrypted, because
            // the auth tag covers the header.
                                   let mut plain_owned: Option<Vec<u8>> = None;
                                   let packet: &mut [u8] = match &srtp {
                                       None => &mut buf[..n],
                                       Some((decrypt, _)) => match decrypt.unprotect_rtp(&buf[..n]) {
                                           Ok(p) => plain_owned.insert(p),
                                           Err(SrtpError::Replayed) => {
                                               debug!(dir, "SRTP replay rejected; dropped");
                                               continue;
                                           }
                                           Err(e) => {
                                               debug!(dir, ?e, "SRTP auth/decrypt failed; dropped");
                                               continue;
                                           }
                                       },
                                   };
                                   let Some(hdr) = RtpHeader::parse(packet) else {
                                       debug!(dir, bytes = packet.len(), "non-RTP packet dropped");
                                       continue;
                                   };
            // Stats see the peer's own SSRC — the report block
            // we send back must name the stream as received.
                                   stats.observe_header(&hdr);
                                   if let Some(sink) = dtmf_sink.as_ref() {
                                       sniff_dtmf(
                                           packet,
                                           &hdr,
                                           sink.as_ref(),
                                           dtmf_detector.as_mut(),
                                           inband_detector.as_mut(),
                                           dir,
                                       );
                                   }
                                   set_ssrc(packet, ssrc_out);
                                   let egress: &[u8] = match &srtp {
                                       None => packet,
                                       Some((_, encrypt)) => match encrypt.protect_rtp(packet) {
                                           Ok(ct) => {
                                               plain_owned = Some(ct);
                                               plain_owned.as_deref().unwrap_or(&[])
                                           }
                                           Err(e) => {
                                               warn!(dir, ?e, "SRTP re-encrypt failed; dropped");
                                               continue;
                                           }
                                       },
                                   };
                                   if let Err(e) = send.send_to(egress, dest).await {
                                       warn!(dir, ?e, "bridge send failed");
                                   } else if let Some(c) = &forwarded {
                                       c.inc();
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

/// PCMU payload type per RFC 3551 §4.5 Table 4 — the only codec the
/// inband detector decodes.
const PT_PCMU_FOR_DTMF: u8 = 0;

/// Feed one plaintext RTP packet to the DTMF detectors. The payload
/// type gate runs before any payload parsing so audio packets cost
/// one comparison; keypresses go to `sink` as they complete.
fn sniff_dtmf(
    packet: &[u8],
    hdr: &RtpHeader,
    sink: &dyn DtmfSink,
    rfc4733: Option<&mut smiths_core::DtmfDetector>,
    inband: Option<&mut smiths_core::InbandDtmfDetector>,
    dir: &'static str,
) {
    match hdr.payload_type {
        smiths_core::RFC4733_PAYLOAD_TYPE => {
            if let Some(detector) = rfc4733
                && let Some(ev) = smiths_core::TelephoneEvent::parse(hdr.payload(packet))
 // The detector treats "same (event, start ts)" as one
 // press, so the packet timestamp is the press key.
                && let Some(press) = detector.feed(&ev, hdr.timestamp)
            {
                sink.deliver(dir, press);
            }
        }
        PT_PCMU_FOR_DTMF => {
            if let Some(detector) = inband {
                for press in detector.feed_pcmu(hdr.payload(packet)) {
                    sink.deliver(dir, press);
                }
            }
        }
        _ => {}
    }
}

/// RTCP received from one peer, however it arrived (dedicated RTCP
/// socket or multiplexed onto the RTP socket).
#[derive(Clone)]
struct RtcpIngress {
    /// SRTCP transform for this leg (the peer's transmit key), or
    /// `None` on plain legs.
    unprotect: Option<Arc<dyn SrtpTransform>>,
    /// Stats for the stream we receive from this peer — its SRs land
    /// here (for `LSR` / `DLSR`).
    recv_stats: StreamStats,
    /// Stats for the stream we send to this peer — its report blocks
    /// about our SSRC land here.
    send_stats: StreamStats,
    /// SSRC we stamp on packets to this peer; report blocks naming it
    /// are about our stream.
    our_ssrc: u32,
    dir: &'static str,
}

impl RtcpIngress {
    fn handle(&self, bytes: &[u8]) {
        let plain_owned;
        let plain: &[u8] = match &self.unprotect {
            None => bytes,
            Some(t) => match t.unprotect_rtcp(bytes) {
                Ok(p) => {
                    plain_owned = p;
                    &plain_owned
                }
                Err(e) => {
                    debug!(dir = self.dir, ?e, "SRTCP decrypt failed; dropped");
                    return;
                }
            },
        };
        let arrival = ntp_middle(ntp_now());
        for pkt in parse_compound(plain) {
            match pkt {
                RtcpPacket::SenderReport { sr, blocks } => {
                    self.recv_stats.record_sender_report(sr.ntp_ts);
                    debug!(
                        dir = self.dir,
                        sender = sr.sender_ssrc,
                        packets = sr.packet_count,
                        octets = sr.octet_count,
                        "peer SR received"
                    );
                    self.absorb_blocks(&blocks, arrival);
                }
                RtcpPacket::ReceiverReport {
                    sender_ssrc,
                    blocks,
                } => {
                    debug!(dir = self.dir, sender = sender_ssrc, "peer RR received");
                    self.absorb_blocks(&blocks, arrival);
                }
                RtcpPacket::SourceDescription(chunks) => {
                    for c in chunks {
                        debug!(dir = self.dir, ssrc = c.ssrc, cname = ?c.cname, "peer SDES");
                    }
                }
                RtcpPacket::Bye { ssrcs, reason } => {
                    debug!(dir = self.dir, ?ssrcs, ?reason, "peer RTCP BYE");
                }
                RtcpPacket::Other { payload_type } => {
                    debug!(dir = self.dir, payload_type, "peer RTCP packet ignored");
                }
            }
        }
    }

    /// Fold the peer's report blocks about our stream into the
    /// send-side stats. Blocks about other SSRCs (stale streams, the
    /// peer's own mixer sources) are ignored.
    fn absorb_blocks(&self, blocks: &[ReportBlock], arrival: u32) {
        for rb in blocks.iter().filter(|rb| rb.ssrc == self.our_ssrc) {
            let rtt = round_trip_time(arrival, rb);
            self.send_stats.record_peer_report(rb, rtt);
            debug!(
                dir = self.dir,
                ssrc = rb.ssrc,
                fraction_lost = rb.fraction_lost,
                cumulative_lost = rb.cumulative_lost,
                jitter = rb.jitter,
                ?rtt,
                "peer report block absorbed"
            );
        }
    }
}

/// Everything the per-leg RTCP emitter needs.
struct RtcpEmitter {
    rtcp: RtcpLeg,
    /// SSRC we send to this peer with.
    sender_ssrc: u32,
    /// Stream we forward to this peer (sender info).
    sender_stats: StreamStats,
    /// Stream we receive from this peer (report block).
    receiver_stats: StreamStats,
    /// SRTCP transform (our transmit key), or `None` on plain legs.
    protect: Option<Arc<dyn SrtpTransform>>,
    interval: Duration,
    sr_sent: Option<Counter>,
    cancel: CancellationToken,
    dir: &'static str,
}

/// Fire a compound RTCP packet (SR + SDES CNAME) on each tick.
///
/// The SR body reports on our **outbound** stream (`sender_stats`);
/// the embedded report block reports on the **inbound** stream we're
/// receiving from this peer (`receiver_stats`), with the fraction lost
/// computed over the interval since the previous report and `LSR` /
/// `DLSR` echoing the peer's last SR. If the inbound stream hasn't
/// seen any packets yet we fall back to a bare SR — RFC 3550 allows
/// RC=0 when the receiver has nothing to report.
fn spawn_sr_emitter(e: RtcpEmitter) -> JoinHandle<()> {
    tokio::spawn(async move {
        let RtcpEmitter {
            rtcp,
            sender_ssrc,
            sender_stats,
            receiver_stats,
            protect,
            interval,
            sr_sent,
            cancel,
            dir,
        } = e;
        let sdes = build_sdes_cname(sender_ssrc, &format!("{sender_ssrc:08x}@smiths-net"));
        let mut interval_loss = IntervalLoss::default();
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
                    let ntp = ntp_now();
                    let packets = u32::try_from(send_snap.packets).unwrap_or(u32::MAX);
                    let octets = u32::try_from(send_snap.octets).unwrap_or(u32::MAX);
                    let sr: Vec<u8> = if recv_snap.packets == 0 {
                        build_sr(sender_ssrc, ntp, send_snap.last_rtp_ts, packets, octets).to_vec()
                    } else {
                        let rb = ReportBlock {
                            ssrc: recv_snap.last_ssrc,
                            fraction_lost: interval_loss.fraction_lost(&recv_snap),
                            cumulative_lost: recv_snap.cumulative_lost,
                            extended_highest_seq: recv_snap.max_seq,
                            jitter: recv_snap.jitter,
                            last_sr: recv_snap.last_sr,
                            delay_since_last_sr: recv_snap.last_sr_age.map_or(0, to_dlsr),
                        };
                        build_sr_with_rb(sender_ssrc, ntp, send_snap.last_rtp_ts, packets, octets, &rb)
                            .to_vec()
                    };
                    let compound = build_compound(&[&sr, &sdes]);
                    let wire = match &protect {
                        None => compound,
                        Some(t) => match t.protect_rtcp(&compound) {
                            Ok(ct) => ct,
                            Err(err) => {
                                warn!(dir, ?err, "SRTCP protect failed; SR skipped");
                                continue;
                            }
                        },
                    };
                    if let Err(err) = rtcp.socket.send_to(&wire, rtcp.peer).await {
                        warn!(dir, ?err, "RTCP SR send failed");
                    } else if let Some(c) = &sr_sent {
                        c.inc();
                    }
                }
            }
        }
        debug!(dir, "bridge RTCP emitter stopped");
    })
}

/// Consume the peer's RTCP on a dedicated RTCP socket.
fn spawn_rtcp_listener(
    socket: Arc<UdpSocket>,
    ingress: RtcpIngress,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dir = ingress.dir;
        let mut buf = vec![0u8; 1500];
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                res = socket.recv_from(&mut buf) => match res {
                    Ok((n, _src)) => ingress.handle(&buf[..n]),
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

/// Overwrite the SSRC field (bytes 8..12) with `ssrc` in place. The
/// caller has already validated the header; the SSRC position is
/// fixed regardless of CSRCs or extensions.
fn set_ssrc(packet: &mut [u8], ssrc: u32) {
    packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
}

/// Non-cryptographic source for engine-chosen SSRC values. A mix of
/// process-time and a per-process counter avoids collisions across
/// bridges without dragging in `rand`.
pub(crate) fn fresh_ssrc() -> u32 {
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
    use crate::rtcp::{build_rr, build_sr as build_peer_sr, parse_sr, parse_sr_with_blocks};
    use crate::srtp::AesCmHmacSha1_80Transform;
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

    fn report_block(ssrc: u32, fraction_lost: u8) -> ReportBlock {
        ReportBlock {
            ssrc,
            fraction_lost,
            cumulative_lost: 3,
            extended_highest_seq: 10,
            jitter: 5,
            last_sr: 0,
            delay_since_last_sr: 0,
        }
    }

    fn fast_rtcp() -> BridgeConfig {
        BridgeConfig {
            rtcp_interval: Some(Duration::from_millis(100)),
            ..BridgeConfig::default()
        }
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
            &Leg::new(Arc::clone(&sock_engine_a), ua_addr_a),
            &Leg::new(Arc::clone(&sock_engine_b), ua_addr_b),
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

        // Stats name the peers' SSRCs as received, not the rewritten ones.
        let stats = bridge.stats();
        assert_eq!(stats.a_to_b.last_ssrc, ua_a_ssrc);
        assert_eq!(stats.b_to_a.last_ssrc, ua_b_ssrc);
        assert_eq!(ssrc_of(&buf[..n]), stats.ssrc_toward_a);

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
            &Leg::new(Arc::clone(&sock_engine_a), ua_addr_a),
            &Leg::new(Arc::clone(&sock_engine_b), ua_addr_b),
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
            &Leg::new(sock_a, addr_b),
            &Leg::new(sock_b, addr_a),
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
            &Leg::new(Arc::clone(&sock_engine_a), ua_addr_a),
            &Leg::new(Arc::clone(&sock_engine_b), ua_addr_b),
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
        assert_eq!(stats.a_to_b.octets, 9, "payload octets only");
        assert_eq!(stats.b_to_a.packets, 0);
        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rtcp_sr_is_emitted_on_interval_with_peer_ssrc_in_report_block() {
        let (sock_engine_a, _) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (rtcp_engine_a, _) = bind_udp().await;
        let (rtcp_engine_b, _) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;
        let (ua_rtcp_a, ua_rtcp_addr_a) = bind_udp().await;
        let (ua_rtcp_b, ua_rtcp_addr_b) = bind_udp().await;

        let mut leg_a = Leg::new(Arc::clone(&sock_engine_a), ua_addr_a);
        leg_a.rtcp = Some(RtcpLeg {
            socket: rtcp_engine_a,
            peer: ua_rtcp_addr_a,
        });
        let mut leg_b = Leg::new(Arc::clone(&sock_engine_b), ua_addr_b);
        leg_b.rtcp = Some(RtcpLeg {
            socket: rtcp_engine_b,
            peer: ua_rtcp_addr_b,
        });
        let bridge = Bridge::spawn_with(BridgeId(5), &leg_a, &leg_b, &fast_rtcp());

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

        // B's RTCP socket gets the SR describing what we forward to B.
        let mut rtcp_buf = [0u8; 256];
        let (n, _) = timeout(Duration::from_secs(2), ua_rtcp_b.recv_from(&mut rtcp_buf))
            .await
            .expect("SR should arrive within 2 s")
            .unwrap();
        let sr = parse_sr(&rtcp_buf[..n]).expect("parse SR");
        assert_eq!(sr.packet_count, 1);
        assert_eq!(sr.octet_count, 3);
        assert_eq!(sr.sender_ssrc, bridge.stats().ssrc_toward_b);
        // Compound packet: SDES CNAME follows the SR.
        let parsed = parse_compound(&rtcp_buf[..n]);
        assert!(
            matches!(&parsed[1], RtcpPacket::SourceDescription(c) if c[0].cname.is_some()),
            "SR must be followed by an SDES CNAME, got {parsed:?}"
        );

        // A's RTCP socket gets the SR whose report block describes the
        // stream received from A — named by A's own SSRC, not ours.
        let (n2, _) = timeout(Duration::from_secs(2), ua_rtcp_a.recv_from(&mut rtcp_buf))
            .await
            .expect("b→a SR should arrive")
            .unwrap();
        let (sr_a, blocks) = parse_sr_with_blocks(&rtcp_buf[..n2]).expect("parse SR");
        assert_eq!(sr_a.packet_count, 0, "nothing forwarded toward A yet");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].ssrc, 0xDEAD, "report block names peer A's SSRC");
        assert_eq!(blocks[0].extended_highest_seq, 1);
        assert_ne!(blocks[0].ssrc, bridge.stats().ssrc_toward_b);

        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn report_block_carries_interval_loss_and_echoes_peer_sr() {
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (rtcp_engine_a, rtcp_engine_addr_a) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;
        let (ua_rtcp_a, ua_rtcp_addr_a) = bind_udp().await;

        let mut leg_a = Leg::new(Arc::clone(&sock_engine_a), ua_addr_a);
        leg_a.rtcp = Some(RtcpLeg {
            socket: rtcp_engine_a,
            peer: ua_rtcp_addr_a,
        });
        let leg_b = Leg::new(Arc::clone(&sock_engine_b), ua_addr_b);
        let bridge = Bridge::spawn_with(BridgeId(6), &leg_a, &leg_b, &fast_rtcp());

        // Peer A announces itself with an SR before any media.
        let peer_ntp = 0x0102_0304_0506_0708u64;
        ua_rtcp_a
            .send_to(
                &build_peer_sr(0xDEAD, peer_ntp, 0, 0, 0),
                rtcp_engine_addr_a,
            )
            .await
            .unwrap();

        // Seq 1..=3 then 8..=10: 4 of 10 expected packets missing.
        let mut buf = [0u8; 64];
        for seq in [1u16, 2, 3, 8, 9, 10] {
            ua_a.send_to(&rtp_packet(seq, 0xDEAD, b"x"), addr_engine_a)
                .await
                .unwrap();
            timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
        }

        // Wait for the first SR toward A whose block has seen seq 10.
        let mut rtcp_buf = [0u8; 256];
        let rb = loop {
            let (n, _) = timeout(Duration::from_secs(2), ua_rtcp_a.recv_from(&mut rtcp_buf))
                .await
                .expect("SR toward A")
                .unwrap();
            let (_, blocks) = parse_sr_with_blocks(&rtcp_buf[..n]).expect("parse");
            if blocks.first().is_some_and(|b| b.extended_highest_seq == 10) {
                break blocks[0];
            }
        };
        assert_eq!(rb.cumulative_lost, 4);
        assert_eq!(
            rb.fraction_lost, 102,
            "4 of 10 lost this interval = 4*256/10"
        );
        assert_eq!(rb.last_sr, ntp_middle(peer_ntp), "LSR echoes the peer's SR");
        assert!(
            rb.delay_since_last_sr > 0,
            "DLSR measures time since that SR"
        );

        // The next interval had no loss at all: fraction resets while
        // the cumulative count stays.
        let rb2 = {
            let (n, _) = timeout(Duration::from_secs(2), ua_rtcp_a.recv_from(&mut rtcp_buf))
                .await
                .expect("second SR toward A")
                .unwrap();
            let (_, blocks) = parse_sr_with_blocks(&rtcp_buf[..n]).expect("parse");
            blocks[0]
        };
        assert_eq!(rb2.fraction_lost, 0);
        assert_eq!(rb2.cumulative_lost, 4);

        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn muxed_rtcp_on_rtp_socket_is_processed_not_forwarded() {
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let bridge = Bridge::spawn(
            BridgeId(7),
            &Leg::new(Arc::clone(&sock_engine_a), ua_addr_a),
            &Leg::new(Arc::clone(&sock_engine_b), ua_addr_b),
        );
        let our_ssrc_to_a = bridge.stats().ssrc_toward_a;

        // Peer A multiplexes an RR about our stream onto the RTP port,
        // sandwiched between two RTP packets.
        let mut buf = [0u8; 256];
        ua_a.send_to(&rtp_packet(1, 0xDEAD, b"one"), addr_engine_a)
            .await
            .unwrap();
        ua_a.send_to(
            &build_rr(0xDEAD, &report_block(our_ssrc_to_a, 64)),
            addr_engine_a,
        )
        .await
        .unwrap();
        ua_a.send_to(&rtp_packet(2, 0xDEAD, b"two"), addr_engine_a)
            .await
            .unwrap();

        let (n1, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[12..n1], b"one");
        let (n2, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[12..n2], b"two", "RTCP must not be relayed to B");
        assert!(
            timeout(Duration::from_millis(200), ua_b.recv_from(&mut buf))
                .await
                .is_err(),
            "nothing else reaches B"
        );

        let stats = bridge.stats();
        assert_eq!(stats.a_to_b.packets, 2, "RTCP is not counted as RTP");
        assert_eq!(
            stats.b_to_a.peer_reports, 1,
            "RR absorbed into send-side stats"
        );
        assert_eq!(stats.b_to_a.peer_fraction_lost, 64);
        assert_eq!(stats.b_to_a.peer_cumulative_lost, 3);
        assert_eq!(stats.b_to_a.peer_jitter, 5);

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
    fn set_ssrc_touches_only_bytes_8_to_12() {
        let mut pkt = rtp_packet(9, 0x1111_1111, b"payload");
        let before = pkt.clone();
        set_ssrc(&mut pkt, 0x2222_2222);
        assert_eq!(&pkt[..8], &before[..8]);
        assert_eq!(ssrc_of(&pkt), 0x2222_2222);
        assert_eq!(&pkt[12..], &before[12..]);
    }

    /// Four independent 30-byte key materials (one per transform) and
    /// the UA-side counterparts.
    struct SrtpFixture {
        leg_a: LegSrtp,
        leg_b: LegSrtp,
        ua_a_tx: AesCmHmacSha1_80Transform,
        ua_b_tx: AesCmHmacSha1_80Transform,
        ua_b_rx: AesCmHmacSha1_80Transform,
    }

    fn srtp_fixture() -> SrtpFixture {
        let km_a_tx: Vec<u8> = (0..30u8).collect();
        let km_e_to_a: Vec<u8> = (30..60u8).collect();
        let km_b_tx: Vec<u8> = (60..90u8).collect();
        let km_e_to_b: Vec<u8> = (90..120u8).collect();
        let t = |km: &[u8]| -> Arc<dyn SrtpTransform> {
            Arc::new(AesCmHmacSha1_80Transform::from_sdes(km).unwrap())
        };
        SrtpFixture {
            leg_a: LegSrtp {
                peer_tx: t(&km_a_tx),
                local_tx: t(&km_e_to_a),
            },
            leg_b: LegSrtp {
                peer_tx: t(&km_b_tx),
                local_tx: t(&km_e_to_b),
            },
            ua_a_tx: AesCmHmacSha1_80Transform::from_sdes(&km_a_tx).unwrap(),
            ua_b_tx: AesCmHmacSha1_80Transform::from_sdes(&km_b_tx).unwrap(),
            ua_b_rx: AesCmHmacSha1_80Transform::from_sdes(&km_e_to_b).unwrap(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn srtp_bridge_decrypts_rewrites_reencrypts_and_rejects_replay() {
        // UA-A encrypts with its `peer_tx_a` key, engine decrypts with
        // the same key on ingress, rewrites SSRC, re-encrypts with
        // engine's `local_tx_b` key toward UA-B, UA-B decrypts.
        let fx = srtp_fixture();
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;

        let mut leg_a = Leg::new(Arc::clone(&sock_engine_a), ua_addr_a);
        leg_a.srtp = Some(fx.leg_a.clone());
        let mut leg_b = Leg::new(Arc::clone(&sock_engine_b), ua_addr_b);
        leg_b.srtp = Some(fx.leg_b.clone());
        let bridge = Bridge::spawn(BridgeId(100), &leg_a, &leg_b);

        let plain = rtp_packet(1, 0xDEAD_BEEF, b"srtp-hi");
        let ciphertext = fx.ua_a_tx.protect_rtp(&plain).unwrap();
        ua_a.send_to(&ciphertext, addr_engine_a).await.unwrap();

        let mut buf = [0u8; 1024];
        let (n, _) = timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .expect("UA-B should receive within 1s")
            .unwrap();
        let recovered = fx
            .ua_b_rx
            .unprotect_rtp(&buf[..n])
            .expect("UA-B must successfully decrypt engine's re-encrypted packet");
        assert_eq!(&recovered[12..], b"srtp-hi", "payload preserved end-to-end");
        assert_ne!(
            ssrc_of(&recovered),
            0xDEAD_BEEF,
            "engine must rewrite SSRC even on the SRTP path"
        );
        assert_eq!(bridge.stats().a_to_b.last_ssrc, 0xDEAD_BEEF);

        // Replaying the captured ciphertext must be dropped by the
        // ingress replay window: B sees nothing and stats don't move.
        ua_a.send_to(&ciphertext, addr_engine_a).await.unwrap();
        assert!(
            timeout(Duration::from_millis(200), ua_b.recv_from(&mut buf))
                .await
                .is_err(),
            "replayed SRTP packet must not be forwarded"
        );
        assert_eq!(bridge.stats().a_to_b.packets, 1);

        bridge.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn srtp_legs_send_and_accept_srtcp() {
        let fx = srtp_fixture();
        let (sock_engine_a, addr_engine_a) = bind_udp().await;
        let (sock_engine_b, _) = bind_udp().await;
        let (rtcp_engine_b, rtcp_engine_addr_b) = bind_udp().await;
        let (ua_a, ua_addr_a) = bind_udp().await;
        let (ua_b, ua_addr_b) = bind_udp().await;
        let (ua_rtcp_b, ua_rtcp_addr_b) = bind_udp().await;

        let mut leg_a = Leg::new(Arc::clone(&sock_engine_a), ua_addr_a);
        leg_a.srtp = Some(fx.leg_a.clone());
        let mut leg_b = Leg::new(Arc::clone(&sock_engine_b), ua_addr_b);
        leg_b.srtp = Some(fx.leg_b.clone());
        leg_b.rtcp = Some(RtcpLeg {
            socket: rtcp_engine_b,
            peer: ua_rtcp_addr_b,
        });
        let bridge = Bridge::spawn_with(BridgeId(101), &leg_a, &leg_b, &fast_rtcp());

        // One media packet so the SR toward B has packet_count 1.
        let ct = fx
            .ua_a_tx
            .protect_rtp(&rtp_packet(1, 0xDEAD_BEEF, b"media"))
            .unwrap();
        ua_a.send_to(&ct, addr_engine_a).await.unwrap();
        let mut buf = [0u8; 1024];
        timeout(Duration::from_secs(1), ua_b.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();

        // The SR reaching B is SRTCP: plaintext parse sees garbage
        // sender info, decrypting with the engine→B key restores it.
        let (n, _) = timeout(Duration::from_secs(2), ua_rtcp_b.recv_from(&mut buf))
            .await
            .expect("SRTCP SR toward B")
            .unwrap();
        let wire = &buf[..n];
        assert_ne!(
            parse_sr(wire).map(|sr| sr.packet_count),
            Some(1),
            "sender info must not be readable without the key"
        );
        let plain = fx
            .ua_b_rx
            .unprotect_rtcp(wire)
            .expect("UA-B decrypts SRTCP");
        let sr = parse_sr(&plain).expect("decrypted SR parses");
        assert_eq!(sr.packet_count, 1);
        assert_eq!(sr.sender_ssrc, bridge.stats().ssrc_toward_b);

        // B answers with an SRTCP-protected RR about our stream.
        let rr = build_rr(0xB0B0, &report_block(bridge.stats().ssrc_toward_b, 13));
        let rr_ct = fx.ua_b_tx.protect_rtcp(&rr).unwrap();
        ua_rtcp_b.send_to(&rr_ct, rtcp_engine_addr_b).await.unwrap();
        let absorbed = timeout(Duration::from_secs(2), async {
            loop {
                if bridge.stats().a_to_b.peer_reports == 1 {
                    break bridge.stats().a_to_b;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("engine must absorb the SRTCP RR");
        assert_eq!(absorbed.peer_fraction_lost, 13);

        // A plaintext RR on an SRTP leg fails authentication and is ignored.
        ua_rtcp_b.send_to(&rr, rtcp_engine_addr_b).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(bridge.stats().a_to_b.peer_reports, 1);

        bridge.shutdown().await;
    }
}
