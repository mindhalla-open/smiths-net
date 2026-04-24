//! Leave-one-out N:N sum + clip.
//!
//! Given `N` input frames of identical length, the mixer produces
//! `N` output frames where output `i` = sum(input `j` for `j ≠ i`)
//! with i32 accumulation clipped back to i16. This is the classic
//! "everyone hears everyone else, but not themselves" layout — feed
//! a participant their own voice back and they'll hear the local
//! echo of their own microphone, which is what the room-level echo
//! canceller on their terminal has to suppress against. Leaving it
//! out is cheap and right.
//!
//! ## Scale
//!
//! At 8 kHz / 20 ms per tick, one frame is 160 samples. Summing N
//! streams into N outputs is O(N²) samples per tick — 25.6 k adds
//! for N=4, 102.4 k for N=8. On a modern x86 that's a few µs of
//! CPU. The mixer is tuned for small-to-mid conferences (N ≤ 16);
//! a large-conference path (hundreds of participants with SFU-style
//! selective forwarding) is a different product.

use crate::agc::Agc;

/// Tunables for the mixer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerConfig {
    /// Number of samples per frame (per participant per tick). The
    /// mixer doesn't care what the sample rate is; it just wants
    /// every participant's frame to be exactly this long. 8 kHz /
    /// 20 ms → 160; 48 kHz / 20 ms → 960.
    pub samples_per_frame: usize,
}

impl Default for MixerConfig {
    fn default() -> Self {
        Self {
            samples_per_frame: 160,
        }
    }
}

/// Leave-one-out N:N sum.
///
/// The mixer is stateless **per tick**: it takes N input slices and
/// writes N output slices. Per-participant AGC state (running-RMS +
/// gain) lives on the [`Agc`] struct the caller maintains.
#[derive(Debug, Default)]
pub struct Mixer {
    cfg: MixerConfig,
}

impl Mixer {
    /// Build a mixer with the given config.
    #[must_use]
    pub fn new(cfg: MixerConfig) -> Self {
        Self { cfg }
    }

    /// The configured frame size.
    #[must_use]
    pub fn samples_per_frame(&self) -> usize {
        self.cfg.samples_per_frame
    }

    /// Mix N inputs into N outputs.
    ///
    /// `outputs[i]` receives the sum of `inputs[j]` for `j ≠ i`,
    /// clipped to `i16`. `agcs` is a per-participant AGC state
    /// (`agcs.len() == inputs.len()`); each output is run through
    /// `agcs[i].apply_inplace(outputs[i])` before the function
    /// returns.
    ///
    /// # Panics
    /// Only in test-like contexts via the length assertions; all
    /// slices must be equal to the configured frame size and the
    /// outer slice lengths must match.
    pub fn mix(&self, inputs: &[&[i16]], agcs: &mut [Agc], outputs: &mut [&mut [i16]]) {
        let n = inputs.len();
        debug_assert_eq!(outputs.len(), n, "N outputs must match N inputs");
        debug_assert_eq!(agcs.len(), n, "one AGC per participant");
        let frame_len = self.cfg.samples_per_frame;
        for slice in inputs {
            debug_assert_eq!(slice.len(), frame_len, "input frame must match config");
        }
        for slice in outputs.iter() {
            debug_assert_eq!(slice.len(), frame_len, "output frame must match config");
        }

        // Strategy: compute the full-N sum once into a scratch i32
        // buffer, then per-output subtract participant `i`'s input
        // and clip. O(N · frame_len) for the full sum + O(N ·
        // frame_len) for the subtractions — O(N · frame_len) total,
        // not O(N² · frame_len).
        let mut scratch = vec![0_i32; frame_len];
        for input in inputs {
            for (acc, &s) in scratch.iter_mut().zip(input.iter()) {
                *acc += i32::from(s);
            }
        }

        for i in 0..n {
            for j in 0..frame_len {
                let sum = scratch[j] - i32::from(inputs[i][j]);
                outputs[i][j] = clip_i32_to_i16(sum);
            }
            agcs[i].apply_inplace(outputs[i]);
        }
    }
}

fn clip_i32_to_i16(x: i32) -> i16 {
    #[allow(clippy::cast_possible_truncation)] // clamped to i16 range below
    if x > i32::from(i16::MAX) {
        i16::MAX
    } else if x < i32::from(i16::MIN) {
        i16::MIN
    } else {
        x as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agc::{Agc, AgcConfig};

    fn no_agc() -> Agc {
        // AGC configured so the attenuator never kicks in — target
        // above any possible i16 RMS, unit gain cap. Lets the mixer
        // math be tested without AGC noise on the floor bits.
        Agc::new(AgcConfig {
            target_rms: u32::MAX,
            attack: 1.0,
            release: 1.0,
            max_gain: 1.0,
        })
    }

    #[test]
    fn mix_of_two_produces_one_minus_two_and_vice_versa() {
        let cfg = MixerConfig {
            samples_per_frame: 4,
        };
        let mx = Mixer::new(cfg);
        let a = [100, 200, 300, 400];
        let b = [1, 2, 3, 4];
        let mut out_a = [0_i16; 4];
        let mut out_b = [0_i16; 4];
        let mut agcs = vec![no_agc(), no_agc()];
        let inputs: Vec<&[i16]> = vec![&a, &b];
        let mut out_slices: [&mut [i16]; 2] = [&mut out_a, &mut out_b];
        mx.mix(&inputs, &mut agcs, &mut out_slices);
        // A hears only B; B hears only A.
        assert_eq!(out_a, b);
        assert_eq!(out_b, a);
    }

    #[test]
    fn mix_of_three_is_leave_one_out_sum() {
        let cfg = MixerConfig {
            samples_per_frame: 2,
        };
        let mx = Mixer::new(cfg);
        let a = [10, 20];
        let b = [100, 200];
        let c = [1000, 2000];
        let mut out_a = [0; 2];
        let mut out_b = [0; 2];
        let mut out_c = [0; 2];
        let mut agcs = vec![no_agc(), no_agc(), no_agc()];
        let inputs: Vec<&[i16]> = vec![&a, &b, &c];
        let mut out_slices: [&mut [i16]; 3] = [&mut out_a, &mut out_b, &mut out_c];
        mx.mix(&inputs, &mut agcs, &mut out_slices);
        assert_eq!(out_a, [b[0] + c[0], b[1] + c[1]]);
        assert_eq!(out_b, [a[0] + c[0], a[1] + c[1]]);
        assert_eq!(out_c, [a[0] + b[0], a[1] + b[1]]);
    }

    #[test]
    fn clipping_saturates_to_i16_range() {
        let cfg = MixerConfig {
            samples_per_frame: 1,
        };
        let mx = Mixer::new(cfg);
        // Two maxima sum to 0x0000_FFFE, which exceeds i16::MAX and
        // must saturate to i16::MAX on the third participant's ear.
        let a = [i16::MAX];
        let b = [i16::MAX];
        let c = [0];
        let mut out_a = [0; 1];
        let mut out_b = [0; 1];
        let mut out_c = [0; 1];
        let mut agcs = vec![no_agc(), no_agc(), no_agc()];
        let inputs: Vec<&[i16]> = vec![&a, &b, &c];
        let mut out_slices: [&mut [i16]; 3] = [&mut out_a, &mut out_b, &mut out_c];
        mx.mix(&inputs, &mut agcs, &mut out_slices);
        assert_eq!(out_c[0], i16::MAX, "positive sum must clip, not wrap");

        // And the minimum path:
        let a = [i16::MIN];
        let b = [i16::MIN];
        let c = [0];
        let mut out_a = [0; 1];
        let mut out_b = [0; 1];
        let mut out_c = [0; 1];
        let inputs: Vec<&[i16]> = vec![&a, &b, &c];
        let mut agcs = vec![no_agc(), no_agc(), no_agc()];
        let mut out_slices: [&mut [i16]; 3] = [&mut out_a, &mut out_b, &mut out_c];
        mx.mix(&inputs, &mut agcs, &mut out_slices);
        assert_eq!(out_c[0], i16::MIN, "negative sum must clip, not wrap");
    }

    #[test]
    fn silent_room_mixes_to_silence() {
        let cfg = MixerConfig {
            samples_per_frame: 4,
        };
        let mx = Mixer::new(cfg);
        let a = [0_i16; 4];
        let b = [0_i16; 4];
        let mut out_a = [0_i16; 4];
        let mut out_b = [0_i16; 4];
        let mut agcs = vec![no_agc(), no_agc()];
        let inputs: Vec<&[i16]> = vec![&a, &b];
        let mut out_slices: [&mut [i16]; 2] = [&mut out_a, &mut out_b];
        mx.mix(&inputs, &mut agcs, &mut out_slices);
        assert_eq!(out_a, [0; 4]);
        assert_eq!(out_b, [0; 4]);
    }
}
