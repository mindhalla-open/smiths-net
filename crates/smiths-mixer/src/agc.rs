//! Per-stream automatic gain control.
//!
//! Lightweight AGC appropriate for a conference mixer. Tracks a
//! running RMS estimate of the output frame; when that RMS climbs
//! past `target_rms` the `Agc` attenuates to bring it back down.
//! The attack/release smoothing prevents the gain from slamming up
//! and down within a single frame (audible as a "pumping" effect).
//!
//! This is *not* a full AGC like WebRTC's — no two-band filter, no
//! adaptive digital limiter, no neural residual echo. It's a
//! single-tap smoothed attenuator, which is the right floor for a
//! mixer whose job is "don't let three loud talkers saturate the
//! fourth's ear". Richer processing belongs in a plugin (`audio.agc`
//! capability, future slice).

/// Tunables for [`Agc`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgcConfig {
    /// RMS target (0..=`i16::MAX` as `u32`). When the smoothed RMS
    /// exceeds this, the AGC attenuates. A typical speech RMS on
    /// PCM16 sits near 3000–5000; 6000 is a reasonable ceiling that
    /// leaves ~14 dB of headroom before clipping at ±32 767.
    pub target_rms: u32,
    /// Attack coefficient for the RMS low-pass filter. `1.0`
    /// disables smoothing (gain reacts instantly); `0.1` is typical
    /// for speech (~10-frame time constant ≈ 200 ms at 20 ms frames).
    pub attack: f32,
    /// Release coefficient — same shape, applied when the RMS is
    /// falling. Typically slower than attack (`0.05`) so the gain
    /// doesn't snap back on a short silence and then have to
    /// re-attenuate on the next loud frame.
    pub release: f32,
    /// Ceiling on the gain the AGC is willing to hand out when the
    /// input is quiet. `1.0` disables boost (pure attenuator);
    /// `2.0` gives up to +6 dB of boost. The mixer typically sets
    /// this to `1.0` — we never *boost* a participant, only hold
    /// back a loud one.
    pub max_gain: f32,
}

impl Default for AgcConfig {
    fn default() -> Self {
        Self {
            target_rms: 6_000,
            attack: 0.2,
            release: 0.05,
            max_gain: 1.0,
        }
    }
}

/// Per-stream AGC state.
#[derive(Clone, Debug)]
pub struct Agc {
    cfg: AgcConfig,
    /// Smoothed RMS estimate (f32 to avoid repeated sqrt on u32).
    rms: f32,
    /// Current gain in 0.0..=1.0 (times `max_gain`).
    gain: f32,
}

impl Agc {
    /// Build a fresh AGC at unity gain.
    #[must_use]
    pub fn new(cfg: AgcConfig) -> Self {
        Self {
            cfg,
            rms: 0.0,
            gain: cfg.max_gain,
        }
    }

    /// Current smoothed RMS. Readable by the VAD so the two share
    /// the same signal-energy estimate instead of computing it
    /// twice.
    #[must_use]
    pub fn rms(&self) -> f32 {
        self.rms
    }

    /// Current gain. Useful for debug + metrics.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Apply gain to the frame in place. Updates the RMS estimate
    /// and the gain towards the target.
    pub fn apply_inplace(&mut self, samples: &mut [i16]) {
        // Measure frame RMS on the *input* — this lets the gain
        // react on the first frame above threshold rather than
        // waiting for the output to climb.
        let mut sumsq: u64 = 0;
        for s in samples.iter() {
            let v = i32::from(*s);
            // v*v is always ≥ 0; the as-u64 is sign-safe by construction.
            #[allow(clippy::cast_sign_loss)]
            let sq = (v * v) as u64;
            sumsq += sq;
        }
        #[allow(clippy::cast_precision_loss)] // sample-count is small
        let mean = sumsq as f32 / samples.len().max(1) as f32;
        let frame_rms = mean.sqrt();

        // Smooth towards the frame RMS.
        let coef = if frame_rms > self.rms {
            self.cfg.attack
        } else {
            self.cfg.release
        };
        self.rms = coef * frame_rms + (1.0 - coef) * self.rms;

        // Compute desired gain.
        #[allow(clippy::cast_precision_loss)] // target_rms is bounded
        let target = self.cfg.target_rms as f32;
        let desired = if self.rms > target {
            target / self.rms
        } else {
            self.cfg.max_gain
        };
        let desired = desired.clamp(0.0, self.cfg.max_gain);
        // Smooth gain changes per-frame to avoid pumping.
        self.gain = 0.3 * desired + 0.7 * self.gain;

        // Apply.
        for s in samples.iter_mut() {
            #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
            let scaled =
                (f32::from(*s) * self.gain).clamp(f32::from(i16::MIN), f32::from(i16::MAX));
            #[allow(clippy::cast_possible_truncation)]
            {
                *s = scaled as i16;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_gain_on_quiet_signal() {
        let mut agc = Agc::new(AgcConfig::default());
        let mut frame = [1_000_i16; 160];
        agc.apply_inplace(&mut frame);
        // Input RMS = 1000, target = 6000 → desired gain = 1.0,
        // actual gain ≈ 1.0 (smoothed from initial 1.0).
        assert!(frame.iter().all(|&s| (s - 1000).abs() <= 1));
    }

    #[test]
    fn loud_signal_is_attenuated_over_multiple_frames() {
        // Feed a 20 000-RMS frame repeatedly; by the end, the AGC
        // should have brought gain below 1.0.
        let mut agc = Agc::new(AgcConfig::default());
        for _ in 0..20 {
            let mut frame = [20_000_i16; 160];
            agc.apply_inplace(&mut frame);
        }
        assert!(
            agc.gain() < 0.5,
            "loud steady signal should have pulled gain below 0.5, got {}",
            agc.gain()
        );
    }

    #[test]
    fn silence_leaves_gain_at_max() {
        let mut agc = Agc::new(AgcConfig::default());
        for _ in 0..10 {
            let mut frame = [0_i16; 160];
            agc.apply_inplace(&mut frame);
        }
        assert!((agc.gain() - 1.0).abs() < 1e-3);
    }
}
