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
//! The mixer owns one `i32` accumulator sized to the frame and reuses
//! it every tick, so a frame costs no heap traffic: callers either
//! run [`Mixer::mix`] over slices they already hold, or drive the
//! two phases directly ([`Mixer::begin_frame`] / [`Mixer::add_input`]
//! then [`Mixer::leave_one_out`] per participant) when their inputs
//! live in a structure that can't hand out a slice-of-slices.
//!
//! ## Scale
//!
//! At 8 kHz / 20 ms per tick, one frame is 160 samples. The full sum
//! is computed once, so mixing is `O(N · frame_len)` per tick, not
//! `O(N²)` — 640 adds for N=4, 2 560 for N=16. The mixer is tuned for
//! small-to-mid conferences (N ≤ 16); a large-conference path
//! (hundreds of participants with SFU-style selective forwarding) is
//! a different product.

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

/// Leave-one-out N:N sum with a reusable accumulator.
#[derive(Debug, Default)]
pub struct Mixer {
    cfg: MixerConfig,
    /// Full-N sum for the current frame; sized to `samples_per_frame`
    /// on first use and kept.
    sum: Vec<i32>,
}

impl Mixer {
    /// Build a mixer with the given config.
    #[must_use]
    pub fn new(cfg: MixerConfig) -> Self {
        Self {
            cfg,
            sum: vec![0; cfg.samples_per_frame],
        }
    }

    /// The configured frame size.
    #[must_use]
    pub fn samples_per_frame(&self) -> usize {
        self.cfg.samples_per_frame
    }

    /// Start a new frame: zero the accumulator.
    pub fn begin_frame(&mut self) {
        self.sum.clear();
        self.sum.resize(self.cfg.samples_per_frame, 0);
    }

    /// Add one participant's input to the accumulator. Inputs shorter
    /// than the frame contribute what they have; longer ones are cut.
    pub fn add_input(&mut self, input: &[i16]) {
        for (acc, &s) in self.sum.iter_mut().zip(input) {
            *acc += i32::from(s);
        }
    }

    /// Write the accumulated sum minus `own` into `out`, clipped to
    /// `i16`. Call once per participant after every input was added.
    pub fn leave_one_out(&self, own: &[i16], out: &mut [i16]) {
        for (i, o) in out.iter_mut().enumerate() {
            let total = self.sum.get(i).copied().unwrap_or(0);
            let mine = own.get(i).copied().map_or(0, i32::from);
            *o = clip_i32_to_i16(total - mine);
        }
    }

    /// Mix N inputs into N outputs.
    ///
    /// `outputs[i]` receives the sum of `inputs[j]` for `j ≠ i`,
    /// clipped to `i16`. `agcs` is a per-participant AGC state
    /// (`agcs.len == inputs.len`); each output is run through
    /// `agcs[i].apply_inplace(outputs[i])` before the function
    /// returns.
    ///
    /// # Panics
    /// Only in test-like contexts via the length assertions; all
    /// slices must be equal to the configured frame size and the
    /// outer slice lengths must match.
    pub fn mix(&mut self, inputs: &[&[i16]], agcs: &mut [Agc], outputs: &mut [&mut [i16]]) {
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

        self.begin_frame();
        for input in inputs {
            self.add_input(input);
        }
        for ((input, output), agc) in inputs.iter().zip(outputs.iter_mut()).zip(agcs.iter_mut()) {
            self.leave_one_out(input, output);
            agc.apply_inplace(output);
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
        let mut mx = Mixer::new(cfg);
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
        let mut mx = Mixer::new(cfg);
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
        let mut mx = Mixer::new(cfg);
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
        let mut mx = Mixer::new(cfg);
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

    #[test]
    fn accumulator_is_reused_across_frames() {
        let mut mx = Mixer::new(MixerConfig {
            samples_per_frame: 4,
        });
        let ptr_before = mx.sum.as_ptr();
        for round in 0..3_i16 {
            mx.begin_frame();
            mx.add_input(&[round; 4]);
            mx.add_input(&[1; 4]);
            let mut out = [0_i16; 4];
            mx.leave_one_out(&[1; 4], &mut out);
            assert_eq!(
                out, [round; 4],
                "previous frames must not leak into the sum"
            );
        }
        assert_eq!(
            mx.sum.as_ptr(),
            ptr_before,
            "no reallocation between frames"
        );
    }
}
