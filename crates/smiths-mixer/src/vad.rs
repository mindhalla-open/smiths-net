//! Voice-activity detection hook for dominant-speaker selection.
//!
//! Conferences care about "who's currently speaking" for two
//! reasons: UI (highlight the active speaker's tile) and
//! operational (selective forwarding, speaker-switched layouts,
//! targeted transcription). The mixer exposes a [`Vad`] trait so a
//! deployment can plug in WebRTC-grade VAD, a neural VAD, or the
//! built-in energy-based [`EnergyVad`].
//!
//! **Why "hook" and not "always-on"**: per-participant VAD on
//! every 20 ms frame for all N participants is cheap at small N,
//! but at N=16 with a sophisticated detector it's non-trivial. The
//! trait lets deployments pay only for what they need — an
//! operator that doesn't care about dominant-speaker events can
//! configure a `NullVad` and skip the work.

/// Voice-activity score for one participant's frame. `0.0` =
/// silence, `1.0` = confident speech. Implementations MAY return
/// fractional values; the [`dominant_speaker`] helper treats
/// anything ≥ `speech_threshold` as "speaking".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VadScore(pub f32);

impl VadScore {
    /// Convenience: sort-of-speech threshold used by
    /// [`dominant_speaker`]. Callers can pass their own threshold
    /// to that helper if they want to retune.
    pub const DEFAULT_SPEECH: f32 = 0.5;
}

/// VAD trait. Implementations maintain per-stream state; the
/// mixer's tick task calls [`Self::observe`] once per participant
/// per tick.
pub trait Vad: Send {
    /// Observe one PCM16 frame; return the speech score. Called in
    /// frame-cadence order; implementations MAY carry state between
    /// calls (smoothing, hangover).
    fn observe(&mut self, samples: &[i16]) -> VadScore;
}

/// Simple energy-based VAD. No speech/noise model; just "is the
/// RMS above threshold". Good enough to pick out "someone is
/// talking" vs "silent room" — which is what dominant-speaker
/// selection needs.
///
/// Hangover logic: once the stream crosses `rms_threshold`, the
/// score stays at 1.0 for `hangover_frames` before it can fall
/// back to 0.0. This matches how humans perceive speech: we don't
/// flicker "speaking / not speaking" on every short pause within a
/// phrase.
#[derive(Clone, Debug)]
pub struct EnergyVad {
    rms_threshold: f32,
    hangover_frames: u32,
    remaining_hangover: u32,
    smoothed_rms: f32,
    smoothing: f32,
}

impl EnergyVad {
    /// Build a detector with the given energy threshold and
    /// hangover duration. Reasonable defaults: `rms_threshold =
    /// 500` (very quiet speech clears this, room tone doesn't),
    /// `hangover_frames = 10` (~200 ms at 20 ms frames).
    #[must_use]
    pub fn new(rms_threshold: f32, hangover_frames: u32) -> Self {
        Self {
            rms_threshold,
            hangover_frames,
            remaining_hangover: 0,
            smoothed_rms: 0.0,
            smoothing: 0.3,
        }
    }
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self::new(500.0, 10)
    }
}

impl Vad for EnergyVad {
    fn observe(&mut self, samples: &[i16]) -> VadScore {
        let mut sumsq: u64 = 0;
        for &s in samples {
            let v = i32::from(s);
            // v*v is always ≥ 0; the as-u64 is sign-safe by construction.
            #[allow(clippy::cast_sign_loss)]
            let sq = (v * v) as u64;
            sumsq += sq;
        }
        #[allow(clippy::cast_precision_loss)]
        let mean = sumsq as f32 / samples.len().max(1) as f32;
        let frame_rms = mean.sqrt();
        self.smoothed_rms = self.smoothing * frame_rms + (1.0 - self.smoothing) * self.smoothed_rms;

        if self.smoothed_rms >= self.rms_threshold {
            self.remaining_hangover = self.hangover_frames;
            VadScore(1.0)
        } else if self.remaining_hangover > 0 {
            self.remaining_hangover -= 1;
            VadScore(1.0)
        } else {
            VadScore(0.0)
        }
    }
}

/// Null VAD — always reports silence. Useful when the operator
/// doesn't care about dominant-speaker events; picks zero CPU cost
/// at the cost of no speaker highlighting.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullVad;

impl Vad for NullVad {
    fn observe(&mut self, _samples: &[i16]) -> VadScore {
        VadScore(0.0)
    }
}

/// Pick the "dominant speaker" from a slice of per-participant
/// scores. Returns the index with the highest score, or `None` if
/// every score is below `threshold`.
#[must_use]
pub fn dominant_speaker(scores: &[VadScore], threshold: f32) -> Option<usize> {
    let mut best: Option<(usize, f32)> = None;
    for (i, s) in scores.iter().enumerate() {
        if s.0 >= threshold {
            match best {
                None => best = Some((i, s.0)),
                Some((_, bv)) if s.0 > bv => best = Some((i, s.0)),
                _ => {}
            }
        }
    }
    best.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_vad_silent_is_silent() {
        let mut v = EnergyVad::default();
        let frame = [0_i16; 160];
        assert_eq!(v.observe(&frame), VadScore(0.0));
    }

    #[test]
    fn energy_vad_loud_is_speech() {
        let mut v = EnergyVad::default();
        let frame = [5_000_i16; 160];
        let score = v.observe(&frame);
        assert_eq!(score, VadScore(1.0));
    }

    #[test]
    fn energy_vad_holds_through_hangover_then_drops() {
        // Hangover semantics: after loud speech, subsequent silent
        // frames keep reporting speech until the smoothed RMS
        // drops below threshold *and* the hangover counter
        // exhausts. Exact crossover frame depends on the smoothing
        // constant; the invariants we care about are
        // "loud → speech" and "eventually → silence given enough
        // silent frames".
        let mut v = EnergyVad::new(500.0, 3);
        let loud = [5_000_i16; 160];
        let quiet = [0_i16; 160];
        assert_eq!(v.observe(&loud), VadScore(1.0));
        // Pump silent frames until the detector releases.
        let mut released_at: Option<usize> = None;
        for i in 1..200 {
            let score = v.observe(&quiet);
            if score == VadScore(0.0) {
                released_at = Some(i);
                break;
            }
        }
        assert!(released_at.is_some(), "detector never released");
        let released_at = released_at.unwrap();
        // Hangover must extend past the first silent frame and
        // eventually release; these are the load-bearing
        // guarantees.
        assert!(released_at > 1, "released too eagerly ({released_at})");
        assert!(released_at < 200, "released too slowly");
    }

    #[test]
    fn dominant_speaker_picks_highest_above_threshold() {
        let scores = [VadScore(0.1), VadScore(0.9), VadScore(0.6), VadScore(0.0)];
        assert_eq!(dominant_speaker(&scores, 0.5), Some(1));
    }

    #[test]
    fn dominant_speaker_returns_none_when_all_silent() {
        let scores = [VadScore(0.0), VadScore(0.1), VadScore(0.2)];
        assert_eq!(dominant_speaker(&scores, 0.5), None);
    }

    #[test]
    fn null_vad_never_reports_speech() {
        let mut v = NullVad;
        assert_eq!(v.observe(&[5_000_i16; 160]), VadScore(0.0));
    }
}
