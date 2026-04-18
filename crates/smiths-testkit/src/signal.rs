//! Tiny signal generators for audio tests.

use std::f32::consts::TAU;

/// Generate a mono sine wave of `freq_hz` for `duration_secs` seconds at
/// `sample_rate` samples per second, as 16-bit PCM.
#[must_use]
pub fn sine_wave(freq_hz: f32, duration_secs: f32, sample_rate: u32, amplitude: i16) -> Vec<i16> {
    // Round; f32→f64 conversion of e.g. 0.02 drifts below the integer.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let total = (f64::from(sample_rate) * f64::from(duration_secs)).round() as usize;
    let mut out = Vec::with_capacity(total);
    let amp = f32::from(amplitude);
    #[allow(clippy::cast_precision_loss)] // n/sr has plenty of fraction room
    for n in 0..total {
        let t = n as f32 / sample_rate as f32;
        let v = (TAU * freq_hz * t).sin() * amp;
        // Round to nearest, clamp to i16.
        let clamped = v.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX));
        #[allow(clippy::cast_possible_truncation)] // clamped above
        out.push(clamped as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_has_expected_length() {
        let s = sine_wave(1_000.0, 0.02, 8_000, 1_000);
        assert_eq!(s.len(), 160); // 20 ms at 8 kHz
    }

    #[test]
    fn sine_peaks_near_amplitude() {
        let s = sine_wave(1_000.0, 1.0, 8_000, 10_000);
        let max = s.iter().copied().max().unwrap();
        assert!(max > 9_500 && max <= 10_000, "peak = {max}");
    }
}
