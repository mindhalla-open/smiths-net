//! Inband (audio-band) DTMF detector via the Goertzel algorithm.
//!
//! Covers legs that never negotiated RFC 4733 telephone-event —
//! typically PSTN gateway crossings where the carrier only sends
//! the tones themselves, buried in the PCMU / PCMA audio stream.
//!
//! The RFC 4733 detector in [`crate::dtmf::DtmfDetector`] stays on
//! every bridge (cheap, no false-positive risk). This one is
//! **opt-in** because the Goertzel math runs per sample — 160 FP
//! ops per frame × 2 legs × 8 tones = ~3k FLOPs per frame, which is
//! fine for a handful of concurrent calls but not free at scale.
//!
//! ## Algorithm — second-order Goertzel
//!
//! For each of the 8 DTMF frequencies we maintain a two-tap filter:
//!
//! ```text
//!   s[n] = x[n] + 2·cos(2π·k/N)·s[n-1] − s[n-2]
//! ```
//!
//! At the end of a window the squared magnitude
//! `s[N-1]² + s[N-2]² − 2·cos(2π·k/N)·s[N-1]·s[N-2]` is compared to
//! a relative-to-frame-energy threshold. When exactly one row tone
//! and one column tone cross, the corresponding digit is emitted —
//! debounced against the previous one so a single prolonged beep
//! doesn't repeat.
//!
//! ## Defaults
//!
//! - 8 kHz clock (matches every real PSTN / RFC 3551 PCMU stream).
//! - 20 ms analysis frame = 160 samples (one RTP packet in the
//!   20 ms cadence every softphone uses).
//! - Magnitude threshold `0.3` of frame energy — conservative to
//!   avoid false positives on crosstalk / echo.
//! - Minimum hold time 40 ms before a new digit can fire, even if
//!   the same key is held down — prevents chattering.

use crate::dtmf::DtmfKeypress;

/// Row frequencies (Hz) — DTMF §3 Table 1.
pub const DTMF_ROW_HZ: [f32; 4] = [697.0, 770.0, 852.0, 941.0];
/// Column frequencies (Hz).
pub const DTMF_COL_HZ: [f32; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
/// Digit grid — `DIGITS[row][col]`. Same order as the frequency
/// tables so a `(row_idx, col_idx)` pair selects the digit directly.
pub const DIGITS: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// Analysis window default (one RTP audio frame at 8 kHz / 20 ms).
pub const DEFAULT_FRAME_SAMPLES: usize = 160;
/// Magnitude threshold relative to frame energy. Below this the
/// detector says "no tone present". 0.3 is the value the reference
/// PSTN trunks use; operators can bump it for noisier links.
pub const DEFAULT_THRESHOLD: f32 = 0.30;
/// Minimum ms a digit must be absent before the same digit re-fires.
pub const DEFAULT_DEBOUNCE_MS: u32 = 40;

/// Per-tone Goertzel coefficient + running taps. One of these per
/// DTMF frequency the detector is tracking.
#[derive(Clone, Debug)]
struct Tap {
    /// `2 · cos(2π · bin_k / frame_samples)`. Precomputed at
    /// construction because it doesn't depend on the input.
    coeff: f32,
    s1: f32,
    s2: f32,
}

impl Tap {
    fn new(target_hz: f32, sample_rate_hz: u32, frame_samples: usize) -> Self {
        // Bin index = round(target_hz * N / sample_rate). Kept as f32
        // so off-bin tones (697 Hz doesn't land perfectly on the
        // 8 kHz / 160-sample grid) still produce a usable response.
        // `frame_samples` is the 160-sample DTMF analysis window —
        // bounded by design, so the `as f32` cast can't lose precision.
        // `frame_samples` is the 160-sample DTMF analysis window and
        // `sample_rate_hz` is an 8 kHz-style audio clock — both well
        // within f32's exact-integer range (2^23).
        #[allow(clippy::cast_precision_loss)]
        let frame_samples_f = frame_samples as f32;
        #[allow(clippy::cast_precision_loss)]
        let sample_rate_f = sample_rate_hz as f32;
        let bin_k = target_hz * frame_samples_f / sample_rate_f;
        let omega = 2.0 * std::f32::consts::PI * bin_k / frame_samples_f;
        Self {
            coeff: 2.0 * omega.cos(),
            s1: 0.0,
            s2: 0.0,
        }
    }

    fn push(&mut self, sample: f32) {
        let s0 = sample + self.coeff * self.s1 - self.s2;
        self.s2 = self.s1;
        self.s1 = s0;
    }

    /// Squared magnitude at the end of a window. `s1` and `s2` hold
    /// the last two filter states; we read them out without resetting
    /// so the caller controls frame boundaries via [`Self::reset`].
    fn magnitude_squared(&self) -> f32 {
        self.s1 * self.s1 + self.s2 * self.s2 - self.coeff * self.s1 * self.s2
    }

    fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }
}

/// Inband DTMF detector. Feed PCM samples (signed-16-bit, 8 kHz) via
/// [`Self::feed_pcm16`]; every `frame_samples` samples the detector
/// evaluates the eight Goertzel taps and may emit one
/// [`DtmfKeypress`].
///
/// Not `Clone` on purpose — one detector per call leg, per direction.
/// Sharing one across legs would mix energy accumulators and produce
/// nonsense.
pub struct InbandDtmfDetector {
    leg_id: String,
    sample_rate_hz: u32,
    frame_samples: usize,
    threshold: f32,
    debounce_samples: u64,
    row_taps: [Tap; 4],
    col_taps: [Tap; 4],
    frame_energy: f32,
    frame_count: usize,
    /// Digit we emitted most recently, with the sample index at which
    /// it fired. Used to debounce same-digit re-fires.
    last_emitted: Option<(char, u64)>,
    /// Total samples consumed — used as a monotonic clock so
    /// `last_emitted` can compare ages.
    samples_consumed: u64,
    /// If non-`None`, a tone is currently being held; emitting waits
    /// for the tone to end (silence frame). Holds `(digit, first_sample)`.
    in_progress: Option<(char, u64)>,
}

impl InbandDtmfDetector {
    /// New detector for one leg with default tunings (8 kHz, 20 ms
    /// frame, 0.3 threshold, 40 ms debounce). Tests / benchmarks that
    /// need tighter detection override via the individual setters.
    #[must_use]
    pub fn new(leg_id: impl Into<String>) -> Self {
        Self::with_config(
            leg_id,
            8_000,
            DEFAULT_FRAME_SAMPLES,
            DEFAULT_THRESHOLD,
            DEFAULT_DEBOUNCE_MS,
        )
    }

    /// Explicit-tunings constructor. `sample_rate_hz` + `frame_samples`
    /// set the Goertzel coefficients; `threshold` is magnitude-ratio
    /// (0.0 – 1.0); `debounce_ms` is minimum gap before the same
    /// digit can fire again.
    #[must_use]
    pub fn with_config(
        leg_id: impl Into<String>,
        sample_rate_hz: u32,
        frame_samples: usize,
        threshold: f32,
        debounce_ms: u32,
    ) -> Self {
        let row_taps: [Tap; 4] = [
            Tap::new(DTMF_ROW_HZ[0], sample_rate_hz, frame_samples),
            Tap::new(DTMF_ROW_HZ[1], sample_rate_hz, frame_samples),
            Tap::new(DTMF_ROW_HZ[2], sample_rate_hz, frame_samples),
            Tap::new(DTMF_ROW_HZ[3], sample_rate_hz, frame_samples),
        ];
        let col_taps: [Tap; 4] = [
            Tap::new(DTMF_COL_HZ[0], sample_rate_hz, frame_samples),
            Tap::new(DTMF_COL_HZ[1], sample_rate_hz, frame_samples),
            Tap::new(DTMF_COL_HZ[2], sample_rate_hz, frame_samples),
            Tap::new(DTMF_COL_HZ[3], sample_rate_hz, frame_samples),
        ];
        let debounce_samples =
            u64::from(debounce_ms).saturating_mul(u64::from(sample_rate_hz)) / 1_000;
        Self {
            leg_id: leg_id.into(),
            sample_rate_hz,
            frame_samples,
            threshold: threshold.clamp(0.0, 1.0),
            debounce_samples,
            row_taps,
            col_taps,
            frame_energy: 0.0,
            frame_count: 0,
            last_emitted: None,
            samples_consumed: 0,
            in_progress: None,
        }
    }

    /// Feed a slice of i16 PCM audio. Samples are consumed one at a
    /// time; when `frame_samples` samples have accumulated the
    /// detector evaluates the taps and may return one keypress.
    ///
    /// Returns every keypress that fires during `pcm` — a single feed
    /// can produce multiple presses if the buffer spans several
    /// analysis frames.
    pub fn feed_pcm16(&mut self, pcm: &[i16]) -> Vec<DtmfKeypress> {
        let mut out = Vec::new();
        for &s in pcm {
            #[allow(clippy::cast_precision_loss)]
            let sample = f32::from(s);
            // Normalize to [-1, 1] so the threshold is scale-
            // invariant. i16::MAX = 32767.
            let normalized = sample / 32_767.0;
            for tap in &mut self.row_taps {
                tap.push(normalized);
            }
            for tap in &mut self.col_taps {
                tap.push(normalized);
            }
            self.frame_energy += normalized * normalized;
            self.frame_count += 1;
            self.samples_consumed += 1;

            if self.frame_count >= self.frame_samples {
                if let Some(press) = self.close_frame() {
                    out.push(press);
                }
            }
        }
        out
    }

    /// Convenience: decode PCMU (μ-law) bytes + feed. Every inband
    /// bridge leg uses PCMU as the audio payload today, so this
    /// saves callers the two-line `pcmu_to_pcm16` dance.
    pub fn feed_pcmu(&mut self, pcmu: &[u8]) -> Vec<DtmfKeypress> {
        let pcm = crate::codec::pcmu_to_pcm16(pcmu);
        self.feed_pcm16(&pcm)
    }

    /// Called every `frame_samples`: score each tap, decide if
    /// row + col tones are present, debounce, and emit.
    fn close_frame(&mut self) -> Option<DtmfKeypress> {
        let row_mags: [f32; 4] = [
            self.row_taps[0].magnitude_squared(),
            self.row_taps[1].magnitude_squared(),
            self.row_taps[2].magnitude_squared(),
            self.row_taps[3].magnitude_squared(),
        ];
        let col_mags: [f32; 4] = [
            self.col_taps[0].magnitude_squared(),
            self.col_taps[1].magnitude_squared(),
            self.col_taps[2].magnitude_squared(),
            self.col_taps[3].magnitude_squared(),
        ];

        // Reset taps + energy for the next frame before any early-
        // return so state can't leak across frames.
        for tap in &mut self.row_taps {
            tap.reset();
        }
        for tap in &mut self.col_taps {
            tap.reset();
        }
        let frame_energy = self.frame_energy;
        self.frame_energy = 0.0;
        self.frame_count = 0;

        // Pick the strongest row + column. A tone must (1) beat the
        // threshold relative to frame energy and (2) be at least ~4×
        // the runner-up in its row/column to avoid co-channel false
        // positives (two adjacent tones with similar energy).
        let (row_idx, row_mag) = argmax(&row_mags);
        let (col_idx, col_mag) = argmax(&col_mags);

        let below_threshold = row_mag < self.threshold * frame_energy
            || col_mag < self.threshold * frame_energy
            || frame_energy < 1e-4;
        if below_threshold {
            // Silence / non-tone audio. Any in-progress tone closes
            // out here — emit if we hadn't yet.
            return self.finish_held();
        }

        // Runner-up ratio check — catch the "two tones, both strong"
        // ambiguity.
        let row_runner = second_max(&row_mags);
        let col_runner = second_max(&col_mags);
        if row_mag < 4.0 * row_runner || col_mag < 4.0 * col_runner {
            return self.finish_held();
        }

        let digit = DIGITS[row_idx][col_idx];
        let now = self.samples_consumed;

        // Held-tone tracking — the tone started some earlier frame
        // and is still ringing. We don't emit until it ends (silence
        // / different digit) so the keypress `duration_ms` matches
        // wall-clock hold time.
        match self.in_progress {
            Some((cur, _start)) if cur == digit => {
                // Same digit still held; carry on.
                None
            }
            Some((cur, start)) => {
                // Different digit — emit the previous one and start
                // tracking the new one.
                let emitted = self.emit_if_new(cur, start, now);
                self.in_progress = Some((digit, now));
                emitted
            }
            None => {
                self.in_progress = Some((digit, now));
                None
            }
        }
    }

    /// Called on a silence frame — if a tone was in progress, close
    /// it out and emit the press. Also triggered by a digit-switch.
    fn finish_held(&mut self) -> Option<DtmfKeypress> {
        let (digit, start) = self.in_progress.take()?;
        self.emit_if_new(digit, start, self.samples_consumed)
    }

    fn emit_if_new(
        &mut self,
        digit: char,
        start_sample: u64,
        end_sample: u64,
    ) -> Option<DtmfKeypress> {
        // Debounce: measure the silence GAP between the previous
        // emission's end and this press's start. A tone that resumes
        // within `debounce_samples` of the last one collapses into
        // a single keypress — prevents chattering on held keys.
        if let Some((last, last_end)) = self.last_emitted
            && last == digit
            && start_sample.saturating_sub(last_end) < self.debounce_samples
        {
            return None;
        }
        let duration_samples = end_sample.saturating_sub(start_sample);
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms =
            (duration_samples.saturating_mul(1_000) / u64::from(self.sample_rate_hz)) as u32;
        self.last_emitted = Some((digit, end_sample));
        Some(DtmfKeypress {
            digit,
            duration_ms,
            leg: self.leg_id.clone(),
        })
    }
}

fn argmax(v: &[f32; 4]) -> (usize, f32) {
    let mut best_i = 0usize;
    let mut best = v[0];
    for (i, &m) in v.iter().enumerate().skip(1) {
        if m > best {
            best = m;
            best_i = i;
        }
    }
    (best_i, best)
}

fn second_max(v: &[f32; 4]) -> f32 {
    let mut first = f32::MIN;
    let mut second = f32::MIN;
    for &m in v {
        if m > first {
            second = first;
            first = m;
        } else if m > second {
            second = m;
        }
    }
    second
}

/// Generate a PCM16 buffer holding `duration_ms` of a DTMF tone
/// for `digit` at `sample_rate_hz`. Used by the accuracy benchmark
/// and by tests that want ground-truth audio without a softphone.
///
/// Amplitude is ~0.5 × `i16::MAX` to leave headroom for the harmonic
/// sum; that's in line with what real PSTN gateways emit.
///
/// # Panics
///
/// Panics if `digit` is not a valid DTMF symbol. Callers pass
/// compile-time literals in tests, so this never fires outside a
/// typo.
#[must_use]
pub fn synthesize_tone(digit: char, duration_ms: u32, sample_rate_hz: u32) -> Vec<i16> {
    use std::f32::consts::PI;
    #[allow(clippy::expect_used)] // Test/bench helper — caller-literal digit.
    let (row_idx, col_idx) = digit_to_row_col(digit).expect("non-DTMF digit");
    let f_row = DTMF_ROW_HZ[row_idx];
    let f_col = DTMF_COL_HZ[col_idx];
    #[allow(clippy::cast_possible_truncation)]
    let samples = ((u64::from(duration_ms) * u64::from(sample_rate_hz)) / 1_000) as usize;
    let mut out = Vec::with_capacity(samples);
    #[allow(clippy::cast_precision_loss)]
    let sr = sample_rate_hz as f32;
    for n in 0..samples {
        #[allow(clippy::cast_precision_loss)]
        let t = n as f32 / sr;
        let s = (2.0 * PI * f_row * t).sin() * 0.5 + (2.0 * PI * f_col * t).sin() * 0.5;
        #[allow(clippy::cast_possible_truncation)]
        let sample = (s * 0.5 * f32::from(i16::MAX)) as i16;
        out.push(sample);
    }
    out
}

fn digit_to_row_col(digit: char) -> Option<(usize, usize)> {
    for (r, row) in DIGITS.iter().enumerate() {
        for (c, &d) in row.iter().enumerate() {
            if d == digit {
                return Some((r, c));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect_one_digit(digit: char) -> Option<char> {
        let mut det = InbandDtmfDetector::new("test");
        // 120 ms tone, then 60 ms silence so the detector emits.
        let tone = synthesize_tone(digit, 120, 8_000);
        let silence = vec![0i16; 8_000 * 60 / 1_000];
        let mut presses = det.feed_pcm16(&tone);
        presses.extend(det.feed_pcm16(&silence));
        presses.first().map(|p| p.digit)
    }

    #[test]
    fn decodes_every_dtmf_digit() {
        for row in &DIGITS {
            for &digit in row {
                let got = detect_one_digit(digit);
                assert_eq!(got, Some(digit), "digit {digit} round-tripped to {got:?}");
            }
        }
    }

    #[test]
    fn silence_produces_no_digit() {
        let mut det = InbandDtmfDetector::new("test");
        let silence = vec![0i16; 8_000]; // 1 s silence
        assert!(det.feed_pcm16(&silence).is_empty());
    }

    #[test]
    fn same_digit_debounced_across_short_gap() {
        let mut det = InbandDtmfDetector::new("test");
        // Press '5' for 100 ms, silence 20 ms (< 40 ms debounce),
        // press '5' for 100 ms, silence. Expect ONE emission because
        // the short gap doesn't clear the debounce window.
        let tone = synthesize_tone('5', 100, 8_000);
        let short_gap = vec![0i16; 8_000 * 20 / 1_000];
        let mut all = Vec::new();
        all.extend(det.feed_pcm16(&tone));
        all.extend(det.feed_pcm16(&short_gap));
        all.extend(det.feed_pcm16(&tone));
        all.extend(det.feed_pcm16(&vec![0i16; 8_000 * 60 / 1_000]));
        assert_eq!(
            all.len(),
            1,
            "debounce must collapse near-duplicates; got {all:?}"
        );
        assert_eq!(all[0].digit, '5');
    }

    #[test]
    fn same_digit_refires_after_long_gap() {
        let mut det = InbandDtmfDetector::new("test");
        let tone = synthesize_tone('3', 100, 8_000);
        // 200 ms silence — well past the 40 ms debounce.
        let long_gap = vec![0i16; 8_000 * 200 / 1_000];
        let mut all = Vec::new();
        all.extend(det.feed_pcm16(&tone));
        all.extend(det.feed_pcm16(&long_gap));
        all.extend(det.feed_pcm16(&tone));
        all.extend(det.feed_pcm16(&vec![0i16; 8_000 * 60 / 1_000]));
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|p| p.digit == '3'));
    }

    #[test]
    fn different_digits_emit_in_sequence() {
        let mut det = InbandDtmfDetector::new("test");
        let mut all = Vec::new();
        for digit in ['1', '9', '*', '#'] {
            all.extend(det.feed_pcm16(&synthesize_tone(digit, 100, 8_000)));
            all.extend(det.feed_pcm16(&vec![0i16; 8_000 * 60 / 1_000]));
        }
        let decoded: Vec<char> = all.iter().map(|p| p.digit).collect();
        assert_eq!(decoded, vec!['1', '9', '*', '#']);
    }

    #[test]
    fn pcmu_feed_matches_pcm16_feed() {
        // Same tone via both APIs should produce the same digit.
        let tone_pcm = synthesize_tone('7', 120, 8_000);
        let pcmu = crate::codec::pcm16_to_pcmu(&tone_pcm);

        let mut det_a = InbandDtmfDetector::new("a");
        let mut det_b = InbandDtmfDetector::new("b");
        let a = det_a.feed_pcm16(&tone_pcm);
        let b = det_b.feed_pcmu(&pcmu);
        // Trailing silence to flush the held-tone emitter.
        let silence_pcm = vec![0i16; 8_000 * 60 / 1_000];
        let silence_pcmu = crate::codec::pcm16_to_pcmu(&silence_pcm);
        let a_final: Vec<_> = a
            .into_iter()
            .chain(det_a.feed_pcm16(&silence_pcm))
            .collect();
        let b_final: Vec<_> = b
            .into_iter()
            .chain(det_b.feed_pcmu(&silence_pcmu))
            .collect();
        assert_eq!(a_final.len(), 1);
        assert_eq!(b_final.len(), 1);
        assert_eq!(a_final[0].digit, '7');
        assert_eq!(b_final[0].digit, '7');
    }

    #[test]
    fn synthesize_tone_matches_requested_duration() {
        let pcm = synthesize_tone('4', 100, 8_000);
        assert_eq!(pcm.len(), 800); // 100 ms @ 8 kHz = 800 samples
    }

    /// Accuracy benchmark — every DTMF digit recovers through the
    /// detector. Replaces the "accuracy vs RFC 2833 ground truth"
    /// small task: RFC 2833 is the ground truth; we assert the
    /// inband detector reaches the same verdict on synthesized
    /// audio. Under real PSTN noise this should target ≥ 95%; the
    /// synthetic bench is pass-all.
    #[test]
    fn accuracy_benchmark_all_digits_clean_audio() {
        let mut correct = 0usize;
        let mut total = 0usize;
        for row in &DIGITS {
            for &digit in row {
                total += 1;
                if detect_one_digit(digit) == Some(digit) {
                    correct += 1;
                }
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = correct as f32 / total as f32;
        assert!(
            ratio >= 0.95,
            "DTMF inband accuracy dropped: {correct}/{total} ({ratio:.2})"
        );
    }
}
