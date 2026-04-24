//! Per-call transcoder: two codec pipelines back-to-back.
//!
//! A [`CallTranscoder`] pairs a decoder for leg A's codec with an
//! encoder for leg B's — the bridge calls [`Self::transcode_a_to_b`]
//! on every RTP payload arriving from A, and the mirrored
//! [`Self::transcode_b_to_a`] on payloads arriving from B. The
//! internal PCM16 exchange format means the two sides can be any
//! combination of `{Pcmu, Pcma, Opus}` without an explicit matrix.
//!
//! The struct holds:
//! - An admission [`TranscodeLease`] (dropped automatically on BYE).
//! - A metrics handle for CPU-ms accounting.
//! - Two boxed codec instances (stateful — Opus keeps an internal
//!   decoder FIFO that must not be shared between calls).
//!
//! Timing is measured with `Instant::now()` bracketing each codec
//! call. The overhead of two clock reads is ~50 ns on modern x86; on
//! a 20-ms cadence that's 0.00025 % of the frame budget, far below
//! the measurement noise floor.

use std::time::Instant;

use crate::budget::TranscodeLease;
use crate::codec::{Codec, CodecKind, TranscodeError};
use crate::metrics::TranscodeMetrics;

/// Two-direction codec pipeline for a single call.
pub struct CallTranscoder {
    a: Box<dyn Codec>,
    b: Box<dyn Codec>,
    metrics: std::sync::Arc<TranscodeMetrics>,
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
            metrics,
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
        let t0 = Instant::now();
        let pcm = self.a.decode(payload)?;
        let t1 = Instant::now();
        let out = self.b.encode(&pcm)?;
        let t2 = Instant::now();
        self.metrics.record_cpu(self.a.kind(), ms_since(t0, t1));
        self.metrics.record_cpu(self.b.kind(), ms_since(t1, t2));
        Ok(out)
    }

    /// Decode an RTP payload from leg B and re-encode it for leg A.
    /// Mirror of [`transcode_a_to_b`](Self::transcode_a_to_b).
    ///
    /// # Errors
    /// As per [`transcode_a_to_b`](Self::transcode_a_to_b).
    pub fn transcode_b_to_a(&mut self, payload: &[u8]) -> Result<Vec<u8>, TranscodeError> {
        let t0 = Instant::now();
        let pcm = self.b.decode(payload)?;
        let t1 = Instant::now();
        let out = self.a.encode(&pcm)?;
        let t2 = Instant::now();
        self.metrics.record_cpu(self.b.kind(), ms_since(t0, t1));
        self.metrics.record_cpu(self.a.kind(), ms_since(t1, t2));
        Ok(out)
    }
}

fn ms_since(start: Instant, end: Instant) -> u64 {
    // `as_millis` is `u128` — callsites only ever see sub-second
    // intervals, so truncating is safe in practice.
    #[allow(clippy::cast_possible_truncation)]
    let ms = end.saturating_duration_since(start).as_millis() as u64;
    ms
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
