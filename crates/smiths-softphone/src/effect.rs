//! Real-time voice changer applied to the outgoing 8 kHz mono frames
//! (so the peer hears the modified voice). Each effect processes one
//! 20 ms / `FRAME_SAMPLES` frame in place and is cheap enough to run on
//! every packet.
//!
//! - **pitch** (`deep` / `high` / `chipmunk`) — a compact two-tap
//!   variable-delay pitch shifter that preserves frame length (and
//!   therefore RTP timing) while shifting pitch.
//! - **robot** — ring modulation against a low-frequency carrier for a
//!   metallic, robotic timbre.

// DSP is full of intentional, bounded sample-format casts (f32↔i16,
// ring-buffer index arithmetic). The pedantic cast lints are noise here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::f32::consts::TAU;

use clap::ValueEnum;

use crate::codec::RTP_SAMPLE_RATE;

/// Selectable voice effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Voice {
    /// Pass-through — your normal voice.
    None,
    /// Lower pitch (−5 semitones).
    Deep,
    /// Higher pitch (+5 semitones).
    High,
    /// Much higher pitch (+10 semitones).
    Chipmunk,
    /// Ring-modulated metallic robot.
    Robot,
}

impl Voice {
    /// Parse a voice name typed at runtime (case-insensitive). Returns
    /// `None` for an unknown name so the caller can warn and ignore.
    pub(crate) fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "normal" => Some(Self::None),
            "deep" | "low" => Some(Self::Deep),
            "high" => Some(Self::High),
            "chipmunk" | "chip" => Some(Self::Chipmunk),
            "robot" => Some(Self::Robot),
            _ => None,
        }
    }

    /// Pitch ratio (output / input frequency) for the pitch effects.
    fn pitch_ratio(self) -> f32 {
        let semitones = match self {
            Self::Deep => -5.0,
            Self::High => 5.0,
            Self::Chipmunk => 10.0,
            _ => 0.0,
        };
        2.0_f32.powf(semitones / 12.0)
    }
}

/// Applies the currently-selected effect to outgoing frames. Holds the
/// per-effect state (pitch-shifter buffer, robot carrier phase) so it
/// stays continuous across frames; swapping voices keeps the buffers
/// and just retunes.
pub(crate) struct VoiceChanger {
    voice: Voice,
    shifter: PitchShifter,
    robot_phase: f32,
}

impl VoiceChanger {
    pub(crate) fn new(voice: Voice) -> Self {
        let mut shifter = PitchShifter::new();
        shifter.set_ratio(voice.pitch_ratio());
        Self {
            voice,
            shifter,
            robot_phase: 0.0,
        }
    }

    /// Switch effect live. Retunes the shifter; leaves its buffer intact
    /// so there's no click on the boundary.
    pub(crate) fn set(&mut self, voice: Voice) {
        self.voice = voice;
        self.shifter.set_ratio(voice.pitch_ratio());
    }

    /// Process one frame in place.
    pub(crate) fn process(&mut self, frame: &mut [i16]) {
        match self.voice {
            Voice::None => {}
            Voice::Deep | Voice::High | Voice::Chipmunk => {
                for s in frame.iter_mut() {
                    let y = self.shifter.process_sample(f32::from(*s) / 32768.0);
                    *s = to_i16(y);
                }
            }
            Voice::Robot => {
                // Ring modulation: multiply by a 75 Hz carrier.
                let carrier_hz = 75.0;
                let step = TAU * carrier_hz / RTP_SAMPLE_RATE as f32;
                for s in frame.iter_mut() {
                    let x = f32::from(*s) / 32768.0;
                    let y = x * self.robot_phase.sin();
                    self.robot_phase += step;
                    if self.robot_phase >= TAU {
                        self.robot_phase -= TAU;
                    }
                    *s = to_i16(y);
                }
            }
        }
    }
}

/// Two-tap variable-delay pitch shifter (a.k.a. the classic crossfading
/// delay-line shifter). Output sample count equals input, so RTP timing
/// is untouched. Some warble is inherent to the method — fine for a fun
/// real-time changer.
struct PitchShifter {
    buf: Vec<f32>,
    write: usize,
    /// Current read delay in samples, in `[0, WIN)`.
    phase: f32,
    ratio: f32,
}

/// Crossfade window length (samples). ~64 ms at 8 kHz.
const WIN: f32 = 512.0;

impl PitchShifter {
    fn new() -> Self {
        Self {
            buf: vec![0.0; 2048],
            write: 0,
            phase: 0.0,
            ratio: 1.0,
        }
    }

    fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio;
    }

    fn process_sample(&mut self, x: f32) -> f32 {
        let n = self.buf.len();
        self.buf[self.write] = x;

        if (self.ratio - 1.0).abs() < f32::EPSILON {
            // No shift — return the freshest sample to avoid latency.
            self.write = (self.write + 1) % n;
            return x;
        }

        // Output time advances at `ratio`; the read delay therefore
        // changes by (1 - ratio) per sample, wrapped into [0, WIN).
        self.phase += 1.0 - self.ratio;
        while self.phase < 0.0 {
            self.phase += WIN;
        }
        while self.phase >= WIN {
            self.phase -= WIN;
        }

        let d1 = self.phase;
        let d2 = if self.phase + WIN * 0.5 >= WIN {
            self.phase - WIN * 0.5
        } else {
            self.phase + WIN * 0.5
        };
        let s1 = self.read_delayed(d1);
        let s2 = self.read_delayed(d2);
        // Triangular crossfade: each tap fades to 0 at its wrap point.
        let y = s1 * tri(d1 / WIN) + s2 * tri(d2 / WIN);

        self.write = (self.write + 1) % n;
        y
    }

    /// Read the buffer `d` samples behind the write head, linearly
    /// interpolated.
    fn read_delayed(&self, d: f32) -> f32 {
        let n = self.buf.len();
        let nf = n as f32;
        let mut pos = self.write as f32 - d;
        if pos < 0.0 {
            pos += nf;
        }
        let i0 = pos.floor() as usize % n;
        let i1 = (i0 + 1) % n;
        let frac = pos - pos.floor();
        self.buf[i0] * (1.0 - frac) + self.buf[i1] * frac
    }
}

/// Triangular window on `[0, 1]`: 0 at the ends, 1 at the centre.
fn tri(x: f32) -> f32 {
    1.0 - (2.0 * x - 1.0).abs()
}

/// Clamp an f32 in roughly `[-1, 1]` to `i16` PCM.
fn to_i16(s: f32) -> i16 {
    (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_name_parses_aliases() {
        assert_eq!(Voice::from_name("DEEP"), Some(Voice::Deep));
        assert_eq!(Voice::from_name("off"), Some(Voice::None));
        assert_eq!(Voice::from_name("chip"), Some(Voice::Chipmunk));
        assert_eq!(Voice::from_name("bogus"), None);
    }

    #[test]
    fn none_is_identity_and_preserves_length() {
        let mut vc = VoiceChanger::new(Voice::None);
        let mut frame = [100i16, -200, 300, -400, 500];
        let before = frame;
        vc.process(&mut frame);
        assert_eq!(frame, before);
    }

    #[test]
    fn pitch_preserves_frame_length_and_produces_signal() {
        let mut vc = VoiceChanger::new(Voice::High);
        // A steady tone fed frame-by-frame. The shifter has ~WIN/2
        // samples of latency, so prime it before checking for output.
        let tone: Vec<i16> = (0..160)
            .map(|i| ((i as f32 * 0.3).sin() * 8000.0) as i16)
            .collect();
        let mut last = tone.clone();
        for _ in 0..20 {
            let mut frame = tone.clone();
            vc.process(&mut frame);
            assert_eq!(frame.len(), 160); // RTP timing preserved every frame
            last = frame;
        }
        // Once primed, the shifted output carries energy (not silence).
        assert!(
            last.iter().any(|&s| s != 0),
            "primed shifter produced silence"
        );
    }
}
