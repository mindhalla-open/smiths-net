//! Per-direction RTP stream statistics.
//!
//! A `StreamStats` snapshot records everything RFC 3550 §A.8 needs to
//! derive jitter + loss across one RTP stream: packets seen, bytes
//! seen, last observed sequence / timestamp, and a running interarrival
//! jitter estimate. The bridge attaches one instance per direction
//! (a→b and b→a) and updates it as packets pass through the forwarder.
//!
//! Stats are kept in atomics so the emission task can sample them
//! concurrently with the forwarder task without synchronization.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// Assumed RTP clock rate (Hz) for PCMU / PCMA — the only codecs the
/// current offer/answer negotiator supports. Used to convert
/// wall-clock arrival deltas into RTP-ticks for the jitter estimator.
/// If the media layer gains dynamic codec support, this needs to flow
/// from the capability descriptor.
pub const DEFAULT_CLOCK_RATE_HZ: u32 = 8_000;

/// Shared handle to the stats for one stream direction. Cheap to clone
/// (internally an `Arc` around atomics).
#[derive(Clone, Default)]
pub struct StreamStats {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Count of packets observed (valid RTP only).
    packets: AtomicU64,
    /// Count of RTP payload + header bytes observed.
    octets: AtomicU64,
    /// Last RTP sequence number seen. Stored as `u32` so `u16::MAX + 1`
    /// = 65536 is distinguishable from 0 on wrap detection.
    last_seq: AtomicU32,
    /// Highest seq ever seen (for loss %).
    max_seq: AtomicU32,
    /// Last observed RTP timestamp — used as the SR's `rtp_ts` field.
    last_rtp_ts: AtomicU32,
    /// SSRC of the last observed packet.
    last_ssrc: AtomicU32,
    /// Running interarrival jitter, scaled by 1 clock tick. Stored as
    /// u32 fixed-point with a 4-bit fractional part for RFC 3550's
    /// 1/16-tick smoothing; on read we shift back down.
    jitter_x16: AtomicU32,
    /// Arrival instant of the previous packet, as nanos-since-start.
    /// `0` means "no prior packet".
    prev_arrival_ns: AtomicU64,
    /// Previous packet's RTP timestamp (plain, not fixed-point).
    prev_rtp_ts: AtomicU32,
    /// Bridge start time for deriving `prev_arrival_ns` deltas.
    epoch: std::sync::OnceLock<Instant>,
}

/// A point-in-time snapshot of a [`StreamStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStatsSnapshot {
    /// Total packets observed.
    pub packets: u64,
    /// Total RTP bytes observed (header + payload).
    pub octets: u64,
    /// Highest sequence number seen.
    pub max_seq: u32,
    /// Most recent RTP timestamp.
    pub last_rtp_ts: u32,
    /// Most recent SSRC.
    pub last_ssrc: u32,
    /// Jitter in RTP timestamp units (RFC 3550 §A.8 smoothing).
    pub jitter: u32,
}

impl StreamStats {
    /// Build a fresh stats handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one RTP packet. Parses header fields (seq, ts, ssrc) and
    /// updates every counter atomically. Non-RTP packets are ignored —
    /// the caller has already dropped them in the forwarder path, but
    /// defensive re-validation here keeps the stats clean if the
    /// upstream gets looser.
    pub fn observe(&self, packet: &[u8]) {
        let Some(hdr) = parse_minimal_rtp(packet) else {
            return;
        };
        self.inner.packets.fetch_add(1, Ordering::Relaxed);
        self.inner
            .octets
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
        self.inner.last_seq.store(hdr.seq.into(), Ordering::Relaxed);
        self.inner
            .max_seq
            .fetch_max(hdr.seq.into(), Ordering::Relaxed);
        self.inner.last_rtp_ts.store(hdr.ts, Ordering::Relaxed);
        self.inner.last_ssrc.store(hdr.ssrc, Ordering::Relaxed);
        self.update_jitter(hdr.ts);
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
        let epoch = self.inner.epoch.get_or_init(Instant::now);
        let now_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);

        let prev_arrival = self.inner.prev_arrival_ns.swap(now_ns, Ordering::Relaxed);
        let prev_ts = self.inner.prev_rtp_ts.swap(rtp_ts, Ordering::Relaxed);
        if prev_arrival == 0 {
            // First packet — nothing to compare against yet.
            return;
        }
        let arrival_delta_ns = now_ns.saturating_sub(prev_arrival);
        // Convert ns → RTP ticks at the codec clock rate (PCMU = 8 kHz).
        // The multiplication can exceed u64 only for multi-hour gaps,
        // which don't exist within a live call; cap at u32 on cast.
        let arrival_ticks =
            (arrival_delta_ns * u64::from(DEFAULT_CLOCK_RATE_HZ) / 1_000_000_000) as u32;
        let ts_delta = rtp_ts.wrapping_sub(prev_ts);
        let d = arrival_ticks.abs_diff(ts_delta);

        // Do the smoothing in the ×16 fixed-point field so we keep
        // sub-tick precision across updates. Jitter never needs more
        // than u32 headroom — at 8 kHz that's 149 hours of ticks.
        let prev_j16 = self.inner.jitter_x16.load(Ordering::Relaxed);
        let d_x16 = d.saturating_mul(16);
        // new_j16 = prev_j16 + (d_x16 - prev_j16) / 16
        let delta = (i64::from(d_x16) - i64::from(prev_j16)) / 16;
        let new_j16 = (i64::from(prev_j16) + delta).clamp(0, i64::from(u32::MAX)) as u32;
        self.inner.jitter_x16.store(new_j16, Ordering::Relaxed);
    }

    /// Snapshot every counter.
    #[must_use]
    pub fn snapshot(&self) -> StreamStatsSnapshot {
        StreamStatsSnapshot {
            packets: self.inner.packets.load(Ordering::Relaxed),
            octets: self.inner.octets.load(Ordering::Relaxed),
            max_seq: self.inner.max_seq.load(Ordering::Relaxed),
            last_rtp_ts: self.inner.last_rtp_ts.load(Ordering::Relaxed),
            last_ssrc: self.inner.last_ssrc.load(Ordering::Relaxed),
            // Shift the fixed-point jitter back to whole RTP ticks.
            jitter: self.inner.jitter_x16.load(Ordering::Relaxed) / 16,
        }
    }
}

struct RtpHeader {
    seq: u16,
    ts: u32,
    ssrc: u32,
}

fn parse_minimal_rtp(packet: &[u8]) -> Option<RtpHeader> {
    if packet.len() < 12 {
        return None;
    }
    if packet[0] >> 6 != 2 {
        return None;
    }
    Some(RtpHeader {
        seq: u16::from_be_bytes([packet[2], packet[3]]),
        ts: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
    })
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
        assert_eq!(snap.octets, 32); // 16 bytes each
        assert_eq!(snap.max_seq, 11);
        assert_eq!(snap.last_rtp_ts, 1160);
        assert_eq!(snap.last_ssrc, 0xAAAA);
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
}
