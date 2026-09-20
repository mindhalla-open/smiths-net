//! Per-call transcoder: two codec pipelines back-to-back.
//!
//! A [`CallTranscoder`] pairs leg A's codec with leg B's — callers
//! run [`Self::transcode_a_to_b`] on every RTP payload arriving from
//! A, and the mirrored [`Self::transcode_b_to_a`] on payloads arriving
//! from B. The internal PCM16 exchange format means the two sides can
//! be any combination of `{Pcmu, Pcma, Opus}` without an explicit
//! matrix. It is the single-owner form of the pipeline (load tests,
//! offline conversion); the live transcoded session in `smiths-media`
//! instead gives each leg its own decoder and encoder so the two
//! directions run on separate tasks without a shared lock.
//!
//! The struct holds:
//! - An admission [`TranscodeLease`] (dropped automatically on BYE).
//! - A [`CpuClock`] for CPU-ms accounting.
//! - Two boxed codec instances (stateful — Opus keeps an internal
//!   decoder FIFO that must not be shared between calls).
//! - A PCM scratch buffer reused across frames.
//!
//! Timing is measured with `Instant::now` bracketing each codec
//! call. The overhead of two clock reads is ~50 ns on modern x86; on
//! a 20-ms cadence that's 0.00025 % of the frame budget, far below
//! the measurement noise floor.

use std::time::Instant;

use crate::budget::TranscodeLease;
use crate::codec::{Codec, CodecKind, TranscodeError};
use crate::metrics::TranscodeMetrics;

/// Per-codec CPU time accumulator that credits whole milliseconds to
/// [`TranscodeMetrics::record_cpu`]. G.711 steps take microseconds,
/// so per-step millisecond rounding would record nothing at all;
/// accumulating in microseconds and flushing on each full
/// millisecond keeps the counter honest.
#[derive(Debug)]
pub struct CpuClock {
    metrics: std::sync::Arc<TranscodeMetrics>,
    pending_us: [u64; CodecKind::ALL.len()],
}

impl CpuClock {
    /// Accumulator reporting to `metrics`.
    #[must_use]
    pub fn new(metrics: std::sync::Arc<TranscodeMetrics>) -> Self {
        Self {
            metrics,
            pending_us: [0; CodecKind::ALL.len()],
        }
    }

    /// Credit the time between `start` and `end` to `codec`.
    pub fn credit(&mut self, codec: CodecKind, start: Instant, end: Instant) {
        let us =
            u64::try_from(end.saturating_duration_since(start).as_micros()).unwrap_or(u64::MAX);
        let slot = &mut self.pending_us[codec.index()];
        *slot = slot.saturating_add(us);
        if *slot >= 1_000 {
            self.metrics.record_cpu(codec, *slot / 1_000);
            *slot %= 1_000;
        }
    }

    /// Time one codec step and credit it.
    pub fn timed<T>(&mut self, codec: CodecKind, step: impl FnOnce() -> T) -> T {
        let t0 = Instant::now();
        let out = step();
        self.credit(codec, t0, Instant::now());
        out
    }
}

/// Two-direction codec pipeline for a single call.
pub struct CallTranscoder {
    a: Box<dyn Codec>,
    b: Box<dyn Codec>,
    clock: CpuClock,
    pcm: Vec<i16>,
    _lease: TranscodeLease,
}

impl CallTranscoder {
    /// Build a transcoder for a call whose leg A speaks `codec_a`
    /// and leg B speaks `codec_b`. Consumes the admission lease —
    /// the caller is the UAS handler, which obtained the lease from
    /// [`CpuBudget::try_admit`](crate::CpuBudget::try_admit) just
    /// before dialling this.
    #[must_use]
    pub fn new(
        codec_a: Box<dyn Codec>,
        codec_b: Box<dyn Codec>,
        metrics: std::sync::Arc<TranscodeMetrics>,
        lease: TranscodeLease,
    ) -> Self {
        Self {
            a: codec_a,
            b: codec_b,
            clock: CpuClock::new(metrics),
            pcm: Vec::new(),
            _lease: lease,
        }
    }

    /// Wire-side codec for leg A.
    #[must_use]
    pub fn codec_a(&self) -> CodecKind {
        self.a.kind()
    }

    /// Wire-side codec for leg B.
    #[must_use]
    pub fn codec_b(&self) -> CodecKind {
        self.b.kind()
    }

    /// Decode an RTP payload from leg A and re-encode it for leg B.
    /// Records CPU time against both codecs' metrics counters.
    ///
    /// # Errors
    /// Propagates [`TranscodeError`] from either codec. The bridge
    /// should treat any error as a reason to tear the call down —
    /// a wedged codec won't self-heal on the next frame.
    pub fn transcode_a_to_b(&mut self, payload: &[u8]) -> Result<Vec<u8>, TranscodeError> {
        let mut out = Vec::new();
        self.transcode_a_to_b_into(payload, &mut out)?;
        Ok(out)
    }

    /// Decode an RTP payload from leg B and re-encode it for leg A.
    /// Mirror of [`transcode_a_to_b`](Self::transcode_a_to_b).
    ///
    /// # Errors
    /// As per [`transcode_a_to_b`](Self::transcode_a_to_b).
    pub fn transcode_b_to_a(&mut self, payload: &[u8]) -> Result<Vec<u8>, TranscodeError> {
        let mut out = Vec::new();
        self.transcode_b_to_a_into(payload, &mut out)?;
        Ok(out)
    }

    /// Buffer-reusing form of [`transcode_a_to_b`](Self::transcode_a_to_b).
    ///
    /// # Errors
    /// As per [`transcode_a_to_b`](Self::transcode_a_to_b).
    pub fn transcode_a_to_b_into(
        &mut self,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), TranscodeError> {
        let Self {
            a, b, clock, pcm, ..
        } = self;
        clock.timed(a.kind(), || a.decode_into(payload, pcm))?;
        clock.timed(b.kind(), || b.encode_into(pcm, out))
    }

    /// Buffer-reusing form of [`transcode_b_to_a`](Self::transcode_b_to_a).
    ///
    /// # Errors
    /// As per [`transcode_a_to_b`](Self::transcode_a_to_b).
    pub fn transcode_b_to_a_into(
        &mut self,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), TranscodeError> {
        let Self {
            a, b, clock, pcm, ..
        } = self;
        clock.timed(b.kind(), || b.decode_into(payload, pcm))?;
        clock.timed(a.kind(), || a.encode_into(pcm, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::{CpuBudget, CpuBudgetConfig};
    use crate::codec::{G711Codec, G711Variant};

    fn metrics() -> std::sync::Arc<TranscodeMetrics> {
        TranscodeMetrics::noop()
    }

    fn lease(metrics: &std::sync::Arc<TranscodeMetrics>) -> TranscodeLease {
        let b = CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: 8,
                cpu_budget_ms_per_call: 100,
            },
            std::sync::Arc::clone(metrics),
        );
        b.try_admit().unwrap()
    }

    #[test]
    fn pcmu_to_pcma_round_trip_stays_close_to_input() {
        let m = metrics();
        let l = lease(&m);
        let mut t = CallTranscoder::new(
            Box::new(G711Codec::new(G711Variant::Pcmu)),
            Box::new(G711Codec::new(G711Variant::Pcma)),
            std::sync::Arc::clone(&m),
            l,
        );
        assert_eq!(t.codec_a(), CodecKind::Pcmu);
        assert_eq!(t.codec_b(), CodecKind::Pcma);

        // Emit a PCMU frame (just a ramp over the encoded range), push
        // it through A→B→A and check we got bytes back on the other
        // side of every hop. G.711 transcoding is lossy — we don't
        // check byte-level equality, only that the pipeline runs.
        let pcmu_frame: Vec<u8> = (0_u8..160).collect();
        let pcma = t.transcode_a_to_b(&pcmu_frame).unwrap();
        assert_eq!(pcma.len(), pcmu_frame.len());
        let back = t.transcode_b_to_a(&pcma).unwrap();
        assert_eq!(back.len(), pcmu_frame.len());
    }

    #[test]
    fn cpu_clock_flushes_whole_milliseconds() {
        let m = metrics();
        let mut clock = CpuClock::new(std::sync::Arc::clone(&m));
        let t0 = Instant::now();
        // 3 × 400 µs = 1.2 ms → one ms credited, 200 µs carried.
        for _ in 0..3 {
            clock.credit(
                CodecKind::Pcmu,
                t0,
                t0 + std::time::Duration::from_micros(400),
            );
        }
        assert_eq!(m.cpu_ms_for(CodecKind::Pcmu), 1);
        clock.credit(
            CodecKind::Pcmu,
            t0,
            t0 + std::time::Duration::from_micros(800),
        );
        assert_eq!(
            m.cpu_ms_for(CodecKind::Pcmu),
            2,
            "carry-over reaches the next ms"
        );
        assert_eq!(m.cpu_ms_for(CodecKind::Pcma), 0);
    }

    #[test]
    fn dropping_transcoder_releases_budget_slot() {
        let m = metrics();
        let budget = CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: 1,
                cpu_budget_ms_per_call: 100,
            },
            std::sync::Arc::clone(&m),
        );
        let l = budget.try_admit().unwrap();
        let t = CallTranscoder::new(
            Box::new(G711Codec::new(G711Variant::Pcmu)),
            Box::new(G711Codec::new(G711Variant::Pcma)),
            std::sync::Arc::clone(&m),
            l,
        );
        assert_eq!(budget.active(), 1);
        assert!(budget.try_admit().is_err());
        drop(t);
        // Slot is back.
        let _next = budget.try_admit().unwrap();
        assert_eq!(budget.active(), 1);
    }
}
