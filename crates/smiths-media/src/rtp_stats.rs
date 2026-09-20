//! Per-direction RTP stream statistics.
//!
//! A `StreamStats` handle tracks one stream as the bridge observes it
//! on ingress: packets and payload octets seen, extended highest
//! sequence number, base sequence, cumulative loss and the RFC 3550
//! §A.8 interarrival jitter estimate. Because the bridge forwards
//! every valid packet 1:1, the same counters also describe what the
//! engine *sends* on the paired egress leg, which is what the RTCP
//! Sender Report's sender-info block needs.
//!
//! Two more pieces of RTCP state live here because they are keyed by
//! the same stream:
//!
//! - the most recent Sender Report received from the stream's
//!   originator (feeds the `LSR` / `DLSR` fields of the report block
//!   we send back), and
//! - the most recent report block the far peer sent about the stream
//!   we forward to it (fraction lost / jitter as the peer sees them,
//!   plus the round-trip time derived from its `LSR` / `DLSR`).
//!
//! Stats are kept in atomics so the emission task can sample them
//! concurrently with the forwarder task without synchronization.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use smiths_core::rtp::RtpHeader;

use crate::rtcp::ReportBlock;

/// RTP clock rate assumed when a caller doesn't specify one: 8 kHz,
/// the rate of G.711 and every other static audio payload type.
pub const DEFAULT_CLOCK_RATE_HZ: u32 = 8_000;

/// Sentinel for "never set" in nanosecond-since-epoch atomics. `0` is
/// not usable because `epoch.elapsed` legitimately returns 0 ns for
/// the first observation on a fast path.
const NEVER: u64 = u64::MAX;

/// Shared handle to the stats for one stream direction. Cheap to clone
/// (internally an `Arc` around atomics).
#[derive(Clone)]
pub struct StreamStats {
    inner: Arc<Inner>,
}

impl Default for StreamStats {
    fn default() -> Self {
        Self::with_clock_rate(DEFAULT_CLOCK_RATE_HZ)
    }
}

struct Inner {
    /// RTP clock rate in Hz; converts arrival deltas into RTP ticks for
    /// the jitter estimator.
    clock_rate: u32,
    /// Count of packets observed (valid RTP only).
    packets: AtomicU64,
    /// Count of RTP payload octets observed (RFC 3550 §6.4.1: header
    /// and padding excluded).
    octets: AtomicU64,
    /// Last RTP sequence number seen.
    last_seq: AtomicU32,
    /// Extended highest sequence number seen (top 16 bits = cycles).
    max_seq: AtomicU32,
    /// First sequence number we saw on this stream. Captured on the
    /// first observed packet; used to compute `expected = max_seq -
    /// base_seq + 1` for cumulative-lost (RFC 3550 §A.3).
    base_seq: AtomicU32,
    /// Number of sequence-number wraps observed.
    cycles: AtomicU32,
    /// Set to 1 after the first observation so `base_seq` is only
    /// written once.
    seen_first: AtomicU32,
    /// Last observed RTP timestamp — used as the SR's `rtp_ts` field.
    last_rtp_ts: AtomicU32,
    /// SSRC of the last observed packet (the originator's, as
    /// received — never the engine's rewritten value).
    last_ssrc: AtomicU32,
    /// Running interarrival jitter in ×16 fixed point (RFC 3550 §A.8
    /// smoothing keeps 1/16-tick precision).
    jitter_x16: AtomicU32,
    /// Arrival instant of the previous packet, as nanos since `epoch`.
    prev_arrival_ns: AtomicU64,
    /// Previous packet's RTP timestamp.
    prev_rtp_ts: AtomicU32,
    /// Middle 32 bits of the NTP timestamp of the last SR received
    /// from this stream's originator; 0 = none yet.
    last_sr_ntp_mid: AtomicU32,
    /// Arrival of that SR as nanos since `epoch`; `NEVER` = none yet.
    last_sr_arrival_ns: AtomicU64,
    /// Number of report blocks the far peer has sent about the stream
    /// we forward to it.
    peer_reports: AtomicU64,
    /// Peer-reported fraction lost (0–255) from its latest block.
    peer_fraction_lost: AtomicU32,
    /// Peer-reported cumulative loss (i32 bit pattern).
    peer_cumulative_lost: AtomicU32,
    /// Peer-reported interarrival jitter, in RTP ticks.
    peer_jitter: AtomicU32,
    /// Round-trip time derived from the peer's LSR/DLSR, in
    /// microseconds; `NEVER` = not measurable yet.
    peer_rtt_us: AtomicU64,
    /// Bridge start time for deriving nanosecond deltas.
    epoch: std::sync::OnceLock<Instant>,
}

impl Inner {
    fn new(clock_rate: u32) -> Self {
        Self {
            clock_rate: clock_rate.max(1),
            packets: AtomicU64::new(0),
            octets: AtomicU64::new(0),
            last_seq: AtomicU32::new(0),
            max_seq: AtomicU32::new(0),
            base_seq: AtomicU32::new(0),
            cycles: AtomicU32::new(0),
            seen_first: AtomicU32::new(0),
            last_rtp_ts: AtomicU32::new(0),
            last_ssrc: AtomicU32::new(0),
            jitter_x16: AtomicU32::new(0),
            prev_arrival_ns: AtomicU64::new(NEVER),
            prev_rtp_ts: AtomicU32::new(0),
            last_sr_ntp_mid: AtomicU32::new(0),
            last_sr_arrival_ns: AtomicU64::new(NEVER),
            peer_reports: AtomicU64::new(0),
            peer_fraction_lost: AtomicU32::new(0),
            peer_cumulative_lost: AtomicU32::new(0),
            peer_jitter: AtomicU32::new(0),
            peer_rtt_us: AtomicU64::new(NEVER),
            epoch: std::sync::OnceLock::new(),
        }
    }

    fn now_ns(&self) -> u64 {
        let epoch = self.epoch.get_or_init(Instant::now);
        u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX - 1)
    }
}

/// A point-in-time snapshot of a [`StreamStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStatsSnapshot {
    /// Total packets observed.
    pub packets: u64,
    /// Total RTP payload octets observed.
    pub octets: u64,
    /// Extended highest sequence number seen (top 16 bits are the
    /// cycle count, low 16 the RTP seq).
    pub max_seq: u32,
    /// Packets expected so far per RFC 3550 §A.3
    /// (`max_seq − base_seq + 1`); 0 before the first packet.
    pub expected: u64,
    /// Most recent RTP timestamp.
    pub last_rtp_ts: u32,
    /// Most recent SSRC, as received from the originator.
    pub last_ssrc: u32,
    /// Jitter in RTP timestamp units (RFC 3550 §A.8 smoothing).
    pub jitter: u32,
    /// Cumulative packets lost so far (`expected − received`), clamped
    /// at 0 when reordering pushes received past expected.
    pub cumulative_lost: i32,
    /// Middle 32 bits of the last SR received from the originator, or
    /// 0 when none has arrived (what the report block's `LSR` carries).
    pub last_sr: u32,
    /// Time since that SR arrived; `None` when none has arrived.
    pub last_sr_age: Option<Duration>,
    /// Report blocks the far peer has sent about the forwarded stream.
    pub peer_reports: u64,
    /// Peer-reported fraction lost (0–255) in its latest block.
    pub peer_fraction_lost: u8,
    /// Peer-reported cumulative loss in its latest block.
    pub peer_cumulative_lost: i32,
    /// Peer-reported jitter in its latest block, RTP ticks.
    pub peer_jitter: u32,
    /// Round-trip time to the peer derived from its latest block's
    /// `LSR` / `DLSR`; `None` until the peer has echoed one of our SRs.
    pub peer_rtt: Option<Duration>,
}

impl StreamStats {
    /// Build a fresh stats handle at the default 8 kHz clock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a fresh stats handle for a stream whose RTP clock ticks
    /// at `clock_rate` Hz.
    #[must_use]
    pub fn with_clock_rate(clock_rate: u32) -> Self {
        Self {
            inner: Arc::new(Inner::new(clock_rate)),
        }
    }

    /// RTP clock rate this stream's jitter is measured against.
    #[must_use]
    pub fn clock_rate(&self) -> u32 {
        self.inner.clock_rate
    }

    /// Observe one RTP packet. Parses the header (CSRC / extension /
    /// padding aware) and updates every counter. Non-RTP packets are
    /// ignored — the caller has already dropped them in the forwarder
    /// path, but defensive re-validation here keeps the stats clean if
    /// the upstream gets looser.
    pub fn observe(&self, packet: &[u8]) {
        if let Some(hdr) = RtpHeader::parse(packet) {
            self.observe_header(&hdr);
        }
    }

    /// Observe an already-parsed header. `hdr.payload_start..end`
    /// gives the payload octet count.
    pub fn observe_header(&self, hdr: &RtpHeader) {
        let inner = &self.inner;
        inner.packets.fetch_add(1, Ordering::Relaxed);
        inner.octets.fetch_add(
            (hdr.payload_end.saturating_sub(hdr.payload_start)) as u64,
            Ordering::Relaxed,
        );

        // Seq-wrap detection (RFC 3550 §A.3). Compare against the low
        // 16 bits of the previous extended max; a large negative delta
        // means the counter wrapped.
        let seq_u32 = u32::from(hdr.sequence);
        if inner.seen_first.swap(1, Ordering::Relaxed) == 0 {
            inner.base_seq.store(seq_u32, Ordering::Relaxed);
            inner.max_seq.store(seq_u32, Ordering::Relaxed);
        } else {
            let prev_max = inner.max_seq.load(Ordering::Relaxed);
            #[allow(clippy::cast_possible_truncation)] // low half by construction
            let prev_low_u16 = (prev_max & 0xFFFF) as u16;
            let diff = i32::from(hdr.sequence) - i32::from(prev_low_u16);
            if diff > 0 {
                let cycles = inner.cycles.load(Ordering::Relaxed);
                inner
                    .max_seq
                    .store((cycles << 16) | seq_u32, Ordering::Relaxed);
            } else if diff < -0x8000 {
                let cycles = inner.cycles.fetch_add(1, Ordering::Relaxed) + 1;
                inner
                    .max_seq
                    .store((cycles << 16) | seq_u32, Ordering::Relaxed);
            }
            // Small negative deltas = reorder within the same cycle;
            // leave max_seq alone.
        }
        inner.last_seq.store(seq_u32, Ordering::Relaxed);
        inner.last_rtp_ts.store(hdr.timestamp, Ordering::Relaxed);
        inner.last_ssrc.store(hdr.ssrc, Ordering::Relaxed);
        self.update_jitter(hdr.timestamp);
    }

    /// Record a Sender Report received from this stream's originator.
    /// `ntp_ts` is the SR's full 64-bit NTP timestamp; the middle 32
    /// bits become the `LSR` we echo back, and the arrival instant
    /// drives `DLSR`.
    pub fn record_sender_report(&self, ntp_ts: u64) {
        #[allow(clippy::cast_possible_truncation)] // middle 32 bits by construction
        let mid = (ntp_ts >> 16) as u32;
        self.inner.last_sr_ntp_mid.store(mid, Ordering::Relaxed);
        let now = self.inner.now_ns();
        self.inner.last_sr_arrival_ns.store(now, Ordering::Relaxed);
    }

    /// Record a report block the far peer sent about the stream we
    /// forward to it. `rtt` is the round-trip time derived from the
    /// block's `LSR` / `DLSR` (see [`crate::rtcp::round_trip_time`]);
    /// `None` when the block didn't echo one of our SRs.
    pub fn record_peer_report(&self, rb: &ReportBlock, rtt: Option<Duration>) {
        let inner = &self.inner;
        inner.peer_reports.fetch_add(1, Ordering::Relaxed);
        inner
            .peer_fraction_lost
            .store(u32::from(rb.fraction_lost), Ordering::Relaxed);
        #[allow(clippy::cast_sign_loss)] // bit pattern round-trips through the atomic
        inner
            .peer_cumulative_lost
            .store(rb.cumulative_lost as u32, Ordering::Relaxed);
        inner.peer_jitter.store(rb.jitter, Ordering::Relaxed);
        if let Some(rtt) = rtt {
            let us = u64::try_from(rtt.as_micros()).unwrap_or(NEVER - 1);
            inner.peer_rtt_us.store(us, Ordering::Relaxed);
        }
    }

    /// RFC 3550 §A.8: `J = J + (|D(i-1, i)| - J) / 16`, where
    /// `D = (arrival_i - arrival_{i-1}) - (ts_i - ts_{i-1})` — both
    /// expressed in RTP ticks.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::similar_names
    )]
    fn update_jitter(&self, rtp_ts: u32) {
        let inner = &self.inner;
        let now_ns = inner.now_ns();
        let prev_arrival = inner.prev_arrival_ns.swap(now_ns, Ordering::Relaxed);
        let prev_ts = inner.prev_rtp_ts.swap(rtp_ts, Ordering::Relaxed);
        if prev_arrival == NEVER {
            // First packet — nothing to compare against yet.
            return;
        }
        let arrival_delta_ns = now_ns.saturating_sub(prev_arrival);
        // ns → RTP ticks at the stream's clock rate. The 128-bit
        // product can't overflow; the tick count is capped at u32.
        let arrival_ticks = u32::try_from(
            u128::from(arrival_delta_ns) * u128::from(inner.clock_rate) / 1_000_000_000,
        )
        .unwrap_or(u32::MAX);
        let ts_delta = rtp_ts.wrapping_sub(prev_ts);
        let d = arrival_ticks.abs_diff(ts_delta);

        // Smooth in the ×16 fixed-point field to keep sub-tick
        // precision across updates.
        let prev_j16 = inner.jitter_x16.load(Ordering::Relaxed);
        let d_x16 = d.saturating_mul(16);
        let delta = (i64::from(d_x16) - i64::from(prev_j16)) / 16;
        let new_j16 = (i64::from(prev_j16) + delta).clamp(0, i64::from(u32::MAX)) as u32;
        inner.jitter_x16.store(new_j16, Ordering::Relaxed);
    }

    /// Snapshot every counter.
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    #[must_use]
    pub fn snapshot(&self) -> StreamStatsSnapshot {
        let inner = &self.inner;
        let packets = inner.packets.load(Ordering::Relaxed);
        let max_seq = inner.max_seq.load(Ordering::Relaxed);
        let base_seq = inner.base_seq.load(Ordering::Relaxed);
        let seen_first = inner.seen_first.load(Ordering::Relaxed) == 1;
        // RFC 3550 §A.3: expected = extended_max - base_seq + 1.
        // Expected and lost both clamp to non-negative; reordering
        // can push received past expected without a real loss.
        let expected: i64 = if seen_first {
            i64::from(max_seq)
                .saturating_sub(i64::from(base_seq))
                .saturating_add(1)
                .max(0)
        } else {
            0
        };
        let cumulative_lost = expected.saturating_sub(packets as i64).max(0) as i32;

        let last_sr_arrival = inner.last_sr_arrival_ns.load(Ordering::Relaxed);
        let last_sr_age = (last_sr_arrival != NEVER)
            .then(|| Duration::from_nanos(inner.now_ns().saturating_sub(last_sr_arrival)));
        let rtt_us = inner.peer_rtt_us.load(Ordering::Relaxed);
        let peer_rtt = (rtt_us != NEVER).then(|| Duration::from_micros(rtt_us));
        StreamStatsSnapshot {
            packets,
            octets: inner.octets.load(Ordering::Relaxed),
            max_seq,
            expected: expected.cast_unsigned(),
            last_rtp_ts: inner.last_rtp_ts.load(Ordering::Relaxed),
            last_ssrc: inner.last_ssrc.load(Ordering::Relaxed),
            // Shift the fixed-point jitter back to whole RTP ticks.
            jitter: inner.jitter_x16.load(Ordering::Relaxed) / 16,
            cumulative_lost,
            last_sr: inner.last_sr_ntp_mid.load(Ordering::Relaxed),
            last_sr_age,
            peer_reports: inner.peer_reports.load(Ordering::Relaxed),
            peer_fraction_lost: (inner.peer_fraction_lost.load(Ordering::Relaxed) & 0xFF) as u8,
            peer_cumulative_lost: inner.peer_cumulative_lost.load(Ordering::Relaxed) as i32,
            peer_jitter: inner.peer_jitter.load(Ordering::Relaxed),
            peer_rtt,
        }
    }
}

/// Fraction-lost tracker for the RTCP report block (RFC 3550 §A.3):
/// compares the stream's expected/received counts against the values
/// at the previous report so each block describes only the interval
/// since the last one.
#[derive(Debug, Default, Clone, Copy)]
pub struct IntervalLoss {
    expected_prior: u64,
    received_prior: u64,
}

impl IntervalLoss {
    /// Compute the 8-bit fraction lost since the previous call and
    /// roll the interval forward.
    #[must_use]
    pub fn fraction_lost(&mut self, snap: &StreamStatsSnapshot) -> u8 {
        let expected_interval = snap.expected.saturating_sub(self.expected_prior);
        let received_interval = snap.packets.saturating_sub(self.received_prior);
        self.expected_prior = snap.expected;
        self.received_prior = snap.packets;
        if expected_interval == 0 || received_interval >= expected_interval {
            return 0;
        }
        let lost = expected_interval - received_interval;
        u8::try_from(lost * 256 / expected_interval).unwrap_or(u8::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp(seq: u16, ts: u32, ssrc: u32) -> Vec<u8> {
        let mut v = vec![0u8; 12];
        v[0] = 0x80;
        v[1] = 0x00;
        v[2..4].copy_from_slice(&seq.to_be_bytes());
        v[4..8].copy_from_slice(&ts.to_be_bytes());
        v[8..12].copy_from_slice(&ssrc.to_be_bytes());
        v.extend_from_slice(&[1, 2, 3, 4]); // 4-byte payload
        v
    }

    #[test]
    fn observe_tracks_counts_and_last_fields() {
        let s = StreamStats::new();
        s.observe(&rtp(10, 1000, 0xAAAA));
        s.observe(&rtp(11, 1160, 0xAAAA));
        let snap = s.snapshot();
        assert_eq!(snap.packets, 2);
        assert_eq!(snap.octets, 8, "payload octets only: 4 bytes each");
        assert_eq!(snap.max_seq, 11);
        assert_eq!(snap.expected, 2);
        assert_eq!(snap.last_rtp_ts, 1160);
        assert_eq!(snap.last_ssrc, 0xAAAA);
    }

    #[test]
    fn octets_exclude_csrcs_extension_and_padding() {
        // CC=1, P=1: one CSRC and 2 bytes of padding around a 4-byte
        // payload — only the payload counts.
        let mut v = vec![0b1010_0001, 0x00];
        v.extend_from_slice(&5u16.to_be_bytes());
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(&0x1234_5678u32.to_be_bytes()); // CSRC
        v.extend_from_slice(&[1, 2, 3, 4, 0, 2]);
        let s = StreamStats::new();
        s.observe(&v);
        assert_eq!(s.snapshot().octets, 4);
    }

    #[test]
    fn non_rtp_is_ignored() {
        let s = StreamStats::new();
        s.observe(&[0x40; 16]); // V=1, wrong
        s.observe(&[0x00; 8]); // too short
        assert_eq!(s.snapshot().packets, 0);
    }

    #[test]
    fn first_packet_does_not_move_jitter() {
        let s = StreamStats::new();
        s.observe(&rtp(1, 100, 1));
        assert_eq!(s.snapshot().jitter, 0);
    }

    #[test]
    fn cumulative_lost_counts_gap_in_sequence() {
        // Receive seq 10, 11, 14, 15 — two packets (12, 13) were lost.
        // expected = 15 - 10 + 1 = 6; received = 4; lost = 2.
        let s = StreamStats::new();
        s.observe(&rtp(10, 0, 1));
        s.observe(&rtp(11, 160, 1));
        s.observe(&rtp(14, 640, 1));
        s.observe(&rtp(15, 800, 1));
        let snap = s.snapshot();
        assert_eq!(snap.packets, 4);
        assert_eq!(snap.expected, 6);
        assert_eq!(
            snap.cumulative_lost, 2,
            "two missing packets between 11 and 14 should count as lost"
        );
    }

    #[test]
    fn cumulative_lost_stays_zero_with_no_gap() {
        let s = StreamStats::new();
        for seq in 100..110 {
            s.observe(&rtp(seq, u32::from(seq) * 160, 1));
        }
        let snap = s.snapshot();
        assert_eq!(snap.packets, 10);
        assert_eq!(snap.cumulative_lost, 0);
    }

    #[test]
    fn cumulative_lost_clamps_at_zero_on_reorder() {
        // Out-of-order packet shouldn't push cumulative_lost negative.
        let s = StreamStats::new();
        s.observe(&rtp(5, 0, 1));
        s.observe(&rtp(6, 160, 1));
        s.observe(&rtp(4, 80, 1)); // reordered arrival
        let snap = s.snapshot();
        assert_eq!(snap.packets, 3);
        assert_eq!(
            snap.cumulative_lost, 0,
            "reordering must not yield negative loss"
        );
    }

    #[test]
    fn sequence_wrap_extends_max_seq() {
        let s = StreamStats::new();
        s.observe(&rtp(65_534, 0, 1));
        s.observe(&rtp(65_535, 160, 1));
        s.observe(&rtp(0, 320, 1));
        s.observe(&rtp(1, 480, 1));
        let snap = s.snapshot();
        assert_eq!(snap.max_seq, (1 << 16) | 1);
        assert_eq!(snap.expected, 4);
        assert_eq!(snap.cumulative_lost, 0);
    }

    #[test]
    fn jitter_increases_when_interarrival_deviates_from_ts_delta() {
        // Observe packets with a large TS gap but near-zero real-world
        // delay — the jitter estimator should pick up the gap.
        let s = StreamStats::new();
        s.observe(&rtp(1, 0, 1));
        // Next packet claims 8000 ticks (1 s) later but arrived right
        // away — the jitter should bump.
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.observe(&rtp(2, 8000, 1));
        assert!(
            s.snapshot().jitter > 0,
            "jitter should rise on ts/arrival mismatch"
        );
    }

    #[test]
    fn jitter_scales_with_clock_rate() {
        // Same wall-clock gap, same "instant" arrival: a 48 kHz stream
        // whose timestamps claim 1 s (48 000 ticks) must report ~6×
        // the jitter of an 8 kHz stream claiming 1 s (8 000 ticks).
        let slow = StreamStats::with_clock_rate(8_000);
        slow.observe(&rtp(1, 0, 1));
        slow.observe(&rtp(2, 8_000, 1));
        let fast = StreamStats::with_clock_rate(48_000);
        fast.observe(&rtp(1, 0, 1));
        fast.observe(&rtp(2, 48_000, 1));
        let (js, jf) = (slow.snapshot().jitter, fast.snapshot().jitter);
        assert!(
            jf > js * 5,
            "48 kHz jitter {jf} should dwarf 8 kHz jitter {js}"
        );
    }

    #[test]
    fn interval_loss_reports_only_the_last_interval() {
        let s = StreamStats::new();
        let mut il = IntervalLoss::default();
        for seq in 0..10u16 {
            s.observe(&rtp(seq, 0, 1));
        }
        assert_eq!(il.fraction_lost(&s.snapshot()), 0, "clean first interval");
        // Second interval: 10 expected (10..20), only 5 arrive.
        for seq in [10u16, 12, 14, 16, 19] {
            s.observe(&rtp(seq, 0, 1));
        }
        let fl = il.fraction_lost(&s.snapshot());
        assert_eq!(fl, 128, "5 of 10 lost = 128/256, got {fl}");
        // Third interval with no loss again reports 0 even though the
        // cumulative count is still 5.
        for seq in 20..30u16 {
            s.observe(&rtp(seq, 0, 1));
        }
        assert_eq!(il.fraction_lost(&s.snapshot()), 0);
        assert_eq!(s.snapshot().cumulative_lost, 5);
    }

    #[test]
    fn sender_report_is_remembered_for_lsr_dlsr() {
        let s = StreamStats::new();
        assert_eq!(s.snapshot().last_sr, 0);
        assert!(s.snapshot().last_sr_age.is_none());
        s.record_sender_report(0x1234_5678_9ABC_DEF0);
        let snap = s.snapshot();
        assert_eq!(snap.last_sr, 0x5678_9ABC, "middle 32 bits of the NTP stamp");
        assert!(snap.last_sr_age.is_some());
    }

    #[test]
    fn peer_report_is_recorded_with_rtt() {
        let s = StreamStats::new();
        let rb = ReportBlock {
            ssrc: 1,
            fraction_lost: 25,
            cumulative_lost: -3,
            extended_highest_seq: 100,
            jitter: 7,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        s.record_peer_report(&rb, Some(Duration::from_millis(42)));
        let snap = s.snapshot();
        assert_eq!(snap.peer_reports, 1);
        assert_eq!(snap.peer_fraction_lost, 25);
        assert_eq!(snap.peer_cumulative_lost, -3);
        assert_eq!(snap.peer_jitter, 7);
        assert_eq!(snap.peer_rtt, Some(Duration::from_millis(42)));
    }
}
