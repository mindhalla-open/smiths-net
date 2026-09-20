//! Live mic capture and speaker playback via `cpal`, bridged to the
//! 8 kHz mono PCM the SIP/RTP side speaks.
//!
//! Two lock-free single-producer / single-consumer rings carry 8 kHz
//! mono `i16` between the audio thread and the async RTP loops:
//! - `capture` — mic → (mono-ize + resample to 8 kHz) → ring → RTP send loop
//! - `playback` — RTP recv loop → ring → (resample to device rate) → speaker
//!
//! The cpal callbacks run on a real-time audio thread, so they never
//! take a lock and never allocate in steady state: every scratch
//! buffer they touch is captured by the closure and reused, and the
//! rings are arrays of atomics indexed by monotonically increasing
//! read/write counters.
//!
//! Devices typically run at 44.1/48 kHz with ≥1 channel, so each
//! direction carries a stateful linear resampler. Linear
//! interpolation alone aliases badly when decimating 48 kHz → 8 kHz
//! (everything above 4 kHz folds back into the voice band), so the
//! resampler runs a two-stage low-pass ahead of decimation (and after
//! interpolation on the way back up) tuned just under the lower
//! rate's Nyquist frequency.

// Audio DSP is full of intentional, bounded sample-format casts:
// f32↔i16 (clamped in `to_i16`), and f64 fractional indices for the
// resampler that are bounded by buffer lengths. The pedantic cast
// lints are noise in this module.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI16, AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use tracing::{info, warn};

use crate::codec::{FRAME_SAMPLES, RTP_SAMPLE_RATE};

/// Each ring holds ~1 s of audio. The rings are a jitter cushion, not
/// a recording buffer, so bounding them keeps latency and memory in
/// check.
const RING_CAPACITY: usize = RTP_SAMPLE_RATE as usize;

/// Lock-free single-producer / single-consumer ring of `i16` samples.
///
/// `head` counts samples consumed, `tail` samples produced; both grow
/// monotonically (wrapping) and index the storage modulo its length.
/// The producer publishes with a `Release` store of `tail` after
/// writing samples; the consumer acquires `tail` before reading them.
/// The mirror pair on `head` lets the producer reuse slots the
/// consumer has finished with. Exactly one thread may push and one
/// may pop.
pub(crate) struct SpscRing {
    buf: Box<[AtomicI16]>,
    head: AtomicUsize,
    tail: AtomicUsize,
}

impl SpscRing {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        let buf: Vec<AtomicI16> = (0..capacity.max(1)).map(|_| AtomicI16::new(0)).collect();
        Self {
            buf: buf.into_boxed_slice(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Samples buffered and not yet consumed.
    pub(crate) fn len(&self) -> usize {
        self.tail
            .load(Ordering::Acquire)
            .wrapping_sub(self.head.load(Ordering::Acquire))
    }

    /// Producer side: append as many of `samples` as fit and return
    /// that count. When the consumer has fallen a whole ring behind
    /// the newest samples are the ones dropped.
    pub(crate) fn push_slice(&self, samples: &[i16]) -> usize {
        let cap = self.buf.len();
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let free = cap - tail.wrapping_sub(head);
        let n = samples.len().min(free);
        for (i, &s) in samples[..n].iter().enumerate() {
            self.buf[tail.wrapping_add(i) % cap].store(s, Ordering::Relaxed);
        }
        self.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer side: fill `out` with up to `out.len` samples and
    /// return how many were written.
    pub(crate) fn pop_slice(&self, out: &mut [i16]) -> usize {
        let cap = self.buf.len();
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let n = out.len().min(tail.wrapping_sub(head));
        for (i, slot) in out[..n].iter_mut().enumerate() {
            *slot = self.buf[head.wrapping_add(i) % cap].load(Ordering::Relaxed);
        }
        self.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }
}

/// Shared 8 kHz mono sample ring between the audio thread and the
/// async side.
pub(crate) type Samples = Arc<SpscRing>;

/// Owns the live cpal streams (which must outlive playback) plus the
/// two sample rings the RTP loops read/write. `cpal::Stream` is not
/// `Send`, so an `AudioIo` value must stay on the thread that built it
/// — the async RTP tasks only ever touch the `Arc<SpscRing>` handles,
/// which are `Send`.
pub(crate) struct AudioIo {
    capture: Samples,
    playback: Samples,
    _input: Stream,
    _output: Stream,
}

impl AudioIo {
    /// Open the default input + output devices and start streaming.
    #[allow(deprecated)] // `Device::name` is deprecated in cpal 0.17 but fine for a log line
    pub(crate) fn start() -> Result<Self> {
        let host = cpal::default_host();
        let input = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input (microphone) device"))?;
        let output = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output (speaker) device"))?;

        let in_cfg = input
            .default_input_config()
            .context("default input config")?;
        let out_cfg = output
            .default_output_config()
            .context("default output config")?;
        info!(
            in_dev = input.name().unwrap_or_default(),
            out_dev = output.name().unwrap_or_default(),
            in_rate = in_cfg.sample_rate(),
            out_rate = out_cfg.sample_rate(),
            "audio devices opened"
        );

        let capture: Samples = Arc::new(SpscRing::with_capacity(RING_CAPACITY));
        let playback: Samples = Arc::new(SpscRing::with_capacity(RING_CAPACITY));

        let input_stream = build_input(
            &input,
            &in_cfg.config(),
            in_cfg.sample_format(),
            in_cfg.channels(),
            Arc::clone(&capture),
        )?;
        let output_stream = build_output(
            &output,
            &out_cfg.config(),
            out_cfg.sample_format(),
            out_cfg.channels(),
            Arc::clone(&playback),
        )?;

        input_stream.play().context("start input stream")?;
        output_stream.play().context("start output stream")?;

        Ok(Self {
            capture,
            playback,
            _input: input_stream,
            _output: output_stream,
        })
    }

    /// Ring mic samples are appended to (8 kHz mono).
    #[must_use]
    pub(crate) fn capture(&self) -> Samples {
        Arc::clone(&self.capture)
    }

    /// Ring speaker samples are drained from (8 kHz mono).
    #[must_use]
    pub(crate) fn playback(&self) -> Samples {
        Arc::clone(&self.playback)
    }
}

/// Pop exactly one 20 ms frame (`FRAME_SAMPLES`) from `q`, or `None`
/// if not enough is buffered yet. Runs on the async side, where an
/// allocation per frame is fine.
#[must_use]
pub(crate) fn pop_frame(q: &Samples) -> Option<Vec<i16>> {
    if q.len() < FRAME_SAMPLES {
        return None;
    }
    let mut frame = vec![0i16; FRAME_SAMPLES];
    let got = q.pop_slice(&mut frame);
    frame.truncate(got);
    Some(frame)
}

/// Append `samples` to `q`. Samples that don't fit (the consumer is a
/// full second behind) are dropped.
pub(crate) fn push_samples(q: &Samples, samples: &[i16]) {
    let _ = q.push_slice(samples);
}

// ---------------------------------------------------------------------------
// Stateful linear resampler with anti-alias / anti-image filtering.
// ---------------------------------------------------------------------------

/// Second-order IIR section (RBJ cookbook low-pass), transposed
/// direct form II so the state is two floats.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// Low-pass at `cutoff` Hz for a stream sampled at `rate` Hz with
    /// quality factor `q` (0.707 = Butterworth).
    fn lowpass(rate: f64, cutoff: f64, q: f64) -> Self {
        let w0 = std::f64::consts::TAU * cutoff / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self {
            b0: ((1.0 - cos) / 2.0 / a0) as f32,
            b1: ((1.0 - cos) / a0) as f32,
            b2: ((1.0 - cos) / 2.0 / a0) as f32,
            a1: (-2.0 * cos / a0) as f32,
            a2: ((1.0 - alpha) / a0) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Two cascaded Butterworth sections: 24 dB/octave.
#[derive(Clone, Copy)]
struct Lowpass([Biquad; 2]);

impl Lowpass {
    fn new(rate: f64, cutoff: f64) -> Self {
        let q = std::f64::consts::FRAC_1_SQRT_2;
        Self([
            Biquad::lowpass(rate, cutoff, q),
            Biquad::lowpass(rate, cutoff, q),
        ])
    }

    fn process(&mut self, x: f32) -> f32 {
        let first = self.0[0].process(x);
        self.0[1].process(first)
    }
}

/// Fraction of the lower rate's Nyquist frequency the filters cut at.
/// 0.9 × 4 kHz = 3.6 kHz for the 8 kHz side — above the G.711 voice
/// band, well inside the transition band of the two-section filter.
const CUTOFF_FRACTION: f64 = 0.9;

/// Resamples a continuous mono stream by linear interpolation. `step`
/// is input-samples-per-output-sample (`in_rate / out_rate`); `pos`
/// and `prev` carry interpolation state across callback boundaries so
/// there's no discontinuity at buffer edges. When decimating, the
/// input is low-passed first so nothing above the output's Nyquist
/// frequency folds back; when interpolating, the output is
/// low-passed to remove the images linear interpolation leaves.
struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
    pre: Option<Lowpass>,
    post: Option<Lowpass>,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        let (pre, post) = match in_rate.cmp(&out_rate) {
            std::cmp::Ordering::Greater => (
                Some(Lowpass::new(
                    f64::from(in_rate),
                    CUTOFF_FRACTION * f64::from(out_rate) / 2.0,
                )),
                None,
            ),
            std::cmp::Ordering::Less => (
                None,
                Some(Lowpass::new(
                    f64::from(out_rate),
                    CUTOFF_FRACTION * f64::from(in_rate) / 2.0,
                )),
            ),
            std::cmp::Ordering::Equal => (None, None),
        };
        Self {
            step: f64::from(in_rate) / f64::from(out_rate),
            pos: 0.0,
            prev: 0.0,
            pre,
            post,
        }
    }

    /// Resample one buffer into `out` (cleared first). `input` is
    /// filtered in place when decimating. Indices are into the virtual
    /// sequence `[prev, input[0], input[1], …]`, so `-1` reads `prev`.
    fn process_into(&mut self, input: &mut [f32], out: &mut Vec<f32>) {
        if let Some(f) = &mut self.pre {
            for x in input.iter_mut() {
                *x = f.process(*x);
            }
        }
        out.clear();
        let len = input.len() as f64;
        while self.pos < len {
            let index = self.pos.floor() as isize;
            let frac = self.pos - index as f64;
            let left = self.sample_at(index, input);
            let right = self.sample_at(index + 1, input);
            let interpolated = (f64::from(left) * (1.0 - frac) + f64::from(right) * frac) as f32;
            out.push(match &mut self.post {
                Some(filter) => filter.process(interpolated),
                None => interpolated,
            });
            self.pos += self.step;
        }
        self.pos -= len;
        if let Some(&last) = input.last() {
            self.prev = last;
        }
    }

    fn sample_at(&self, i: isize, input: &[f32]) -> f32 {
        if i < 0 {
            self.prev
        } else {
            let idx = i as usize;
            input
                .get(idx)
                .copied()
                .or_else(|| input.last().copied())
                .unwrap_or(self.prev)
        }
    }
}

// ---------------------------------------------------------------------------
// Stream builders (one monomorphization per cpal sample format).
// ---------------------------------------------------------------------------

/// Scratch capacity reserved up front so the first callbacks don't
/// grow buffers on the audio thread; cpal blocks are a few hundred to
/// a few thousand frames.
const SCRATCH_SAMPLES: usize = 8192;

fn build_input(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    channels: u16,
    capture: Samples,
) -> Result<Stream> {
    match format {
        SampleFormat::F32 => input_stream::<f32>(device, config, channels, capture),
        SampleFormat::I16 => input_stream::<i16>(device, config, channels, capture),
        SampleFormat::U16 => input_stream::<u16>(device, config, channels, capture),
        other => Err(anyhow!("unsupported input sample format: {other:?}")),
    }
}

fn input_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: u16,
    capture: Samples,
) -> Result<Stream>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let channels = channels.max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate, RTP_SAMPLE_RATE);
    let mut mono: Vec<f32> = Vec::with_capacity(SCRATCH_SAMPLES);
    let mut resampled: Vec<f32> = Vec::with_capacity(SCRATCH_SAMPLES);
    let mut pcm: Vec<i16> = Vec::with_capacity(SCRATCH_SAMPLES);
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                // Average interleaved channels down to mono f32.
                mono.clear();
                mono.extend(data.chunks(channels).map(|frame| {
                    let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                    sum / channels as f32
                }));
                resampler.process_into(&mut mono, &mut resampled);
                pcm.clear();
                pcm.extend(resampled.iter().map(|s| to_i16(*s)));
                let _ = capture.push_slice(&pcm);
            },
            |e| warn!(?e, "input stream error"),
            None,
        )
        .context("build input stream")?;
    Ok(stream)
}

fn build_output(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    channels: u16,
    playback: Samples,
) -> Result<Stream> {
    match format {
        SampleFormat::F32 => output_stream::<f32>(device, config, channels, playback),
        SampleFormat::I16 => output_stream::<i16>(device, config, channels, playback),
        SampleFormat::U16 => output_stream::<u16>(device, config, channels, playback),
        other => Err(anyhow!("unsupported output sample format: {other:?}")),
    }
}

fn output_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: u16,
    playback: Samples,
) -> Result<Stream>
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let channels = channels.max(1) as usize;
    let out_rate = config.sample_rate;
    let mut resampler = Resampler::new(RTP_SAMPLE_RATE, out_rate);
    // Device-rate mono carry buffer: the resampler emits a variable
    // count per pull, so leftovers wait here for the next callback.
    let mut dev_buf: VecDeque<f32> = VecDeque::with_capacity(SCRATCH_SAMPLES);
    let mut pcm: Vec<i16> = Vec::with_capacity(SCRATCH_SAMPLES);
    let mut mono: Vec<f32> = Vec::with_capacity(SCRATCH_SAMPLES);
    let mut resampled: Vec<f32> = Vec::with_capacity(SCRATCH_SAMPLES);
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let frames = data.len() / channels;
                // Pull enough 8 kHz samples to (over-)fill this block.
                if dev_buf.len() < frames {
                    let need_8k = ((frames - dev_buf.len()) as f64 * f64::from(RTP_SAMPLE_RATE)
                        / f64::from(out_rate))
                    .ceil() as usize
                        + 1;
                    pcm.clear();
                    pcm.resize(need_8k, 0);
                    let got = playback.pop_slice(&mut pcm);
                    // Underrun: fewer samples than needed; the block is
                    // padded with silence below.
                    pcm.truncate(got);
                    mono.clear();
                    mono.extend(pcm.iter().map(|s| f32::from(*s) / 32768.0));
                    resampler.process_into(&mut mono, &mut resampled);
                    dev_buf.extend(resampled.iter().copied());
                }
                for frame in data.chunks_mut(channels) {
                    let value = dev_buf.pop_front().unwrap_or(0.0);
                    let sample = T::from_sample(value);
                    for slot in frame.iter_mut() {
                        *slot = sample;
                    }
                }
            },
            |e| warn!(?e, "output stream error"),
            None,
        )
        .context("build output stream")?;
    Ok(stream)
}

/// Clamp an f32 in roughly `[-1, 1]` to `i16` PCM.
fn to_i16(s: f32) -> i16 {
    let scaled = (s * 32767.0).round();
    scaled.clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_round_trips_across_the_wrap_point() {
        let ring = SpscRing::with_capacity(8);
        assert_eq!(ring.push_slice(&[1, 2, 3, 4, 5, 6]), 6);
        let mut out = [0i16; 4];
        assert_eq!(ring.pop_slice(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);
        // 2 left, 6 free: this write wraps around the end of storage.
        assert_eq!(ring.push_slice(&[7, 8, 9, 10, 11, 12]), 6);
        assert_eq!(ring.len(), 8);
        let mut out = [0i16; 8];
        assert_eq!(ring.pop_slice(&mut out), 8);
        assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.pop_slice(&mut out), 0, "empty ring yields nothing");
    }

    #[test]
    fn ring_drops_newest_when_full() {
        let ring = SpscRing::with_capacity(4);
        assert_eq!(ring.push_slice(&[1, 2, 3]), 3);
        assert_eq!(ring.push_slice(&[4, 5, 6]), 1, "only one slot was free");
        let mut out = [0i16; 4];
        assert_eq!(ring.pop_slice(&mut out), 4);
        assert_eq!(out, [1, 2, 3, 4]);
    }

    #[test]
    fn pop_frame_needs_a_whole_frame() {
        let q: Samples = Arc::new(SpscRing::with_capacity(RING_CAPACITY));
        push_samples(&q, &[1i16; FRAME_SAMPLES - 1]);
        assert!(pop_frame(&q).is_none());
        push_samples(&q, &[2i16; 1]);
        let frame = pop_frame(&q).expect("one frame buffered");
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert_eq!(frame[FRAME_SAMPLES - 1], 2);
        assert!(pop_frame(&q).is_none());
    }

    /// RMS of a resampled sine at `hz`, generated at `in_rate` for one
    /// second and resampled to `out_rate`. The first 100 ms are
    /// skipped so filter warm-up doesn't skew the measurement.
    fn resampled_rms(in_rate: u32, out_rate: u32, hz: f64) -> (f32, usize) {
        let mut r = Resampler::new(in_rate, out_rate);
        let mut input: Vec<f32> = (0..in_rate)
            .map(|n| (std::f64::consts::TAU * hz * f64::from(n) / f64::from(in_rate)).sin() as f32)
            .collect();
        let mut out = Vec::new();
        r.process_into(&mut input, &mut out);
        let skip = out_rate as usize / 10;
        let tail = &out[skip..];
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        (rms, out.len())
    }

    #[test]
    fn decimation_keeps_voice_band_and_rejects_aliases() {
        // 1 kHz sits in the pass band: RMS ≈ 1/√2.
        let (voice, n) = resampled_rms(48_000, 8_000, 1_000.0);
        assert!((voice - 0.707).abs() < 0.05, "1 kHz RMS {voice}");
        assert!(
            (n as i64 - 8_000).abs() <= 2,
            "48k→8k yields ~8000 samples, got {n}"
        );
        // 10 kHz would alias to 2 kHz without the low-pass; with it the
        // residual must be at least 30 dB down.
        let (alias, _) = resampled_rms(48_000, 8_000, 10_000.0);
        assert!(
            alias < voice / 30.0,
            "10 kHz leaked through: {alias} vs {voice}"
        );
    }

    #[test]
    fn interpolation_produces_the_rate_ratio_and_keeps_level() {
        let (voice, n) = resampled_rms(8_000, 48_000, 1_000.0);
        assert!(
            (n as i64 - 48_000).abs() <= 8,
            "8k→48k yields ~48000 samples, got {n}"
        );
        assert!((voice - 0.707).abs() < 0.05, "1 kHz RMS {voice}");
    }

    #[test]
    fn same_rate_is_a_passthrough() {
        let mut r = Resampler::new(8_000, 8_000);
        let mut input = vec![0.25f32, -0.5, 0.75, 1.0];
        let mut out = Vec::new();
        r.process_into(&mut input, &mut out);
        assert_eq!(out, vec![0.25, -0.5, 0.75, 1.0]);
    }
}
