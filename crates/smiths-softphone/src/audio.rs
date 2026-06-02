//! Live mic capture and speaker playback via `cpal`, bridged to the
//! 8 kHz mono PCM the SIP/RTP side speaks.
//!
//! Two lock-guarded queues carry 8 kHz mono `i16` between the audio
//! thread and the async RTP loops:
//! - `capture`  — mic → (mono-ize + resample to 8 kHz) → queue → RTP send loop
//! - `playback` — RTP recv loop → queue → (resample to device rate) → speaker
//!
//! Devices typically run at 44.1/48 kHz with ≥1 channel, so each
//! direction carries a simple stateful linear resampler. Linear is
//! crude but adequate for an 8 kHz G.711 voice MVP; a polyphase
//! resampler (`rubato`) is the documented later-polish step.

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
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use tracing::{info, warn};

use crate::codec::{FRAME_SAMPLES, RTP_SAMPLE_RATE};

/// Shared 8 kHz mono sample queue between the audio thread and the
/// async side. `i16` PCM, little work per sample so a `Mutex` is fine.
pub(crate) type Samples = Arc<Mutex<VecDeque<i16>>>;

/// Cap each queue at ~1 s of audio. Overflow drops the oldest samples
/// — the queues are a jitter cushion, not a recording buffer, so
/// bounding them keeps latency and memory in check.
const MAX_BUFFERED: usize = RTP_SAMPLE_RATE as usize;

/// Owns the live cpal streams (which must outlive playback) plus the
/// two sample queues the RTP loops read/write. `cpal::Stream` is not
/// `Send`, so an `AudioIo` value must stay on the thread that built it
/// — the async RTP tasks only ever touch the `Arc<Mutex<…>>` queues,
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

        let capture: Samples = Arc::new(Mutex::new(VecDeque::new()));
        let playback: Samples = Arc::new(Mutex::new(VecDeque::new()));

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

    /// Queue mic samples are appended to (8 kHz mono).
    #[must_use]
    pub(crate) fn capture(&self) -> Samples {
        Arc::clone(&self.capture)
    }

    /// Queue speaker samples are drained from (8 kHz mono).
    #[must_use]
    pub(crate) fn playback(&self) -> Samples {
        Arc::clone(&self.playback)
    }
}

/// Pop exactly one 20 ms frame (`FRAME_SAMPLES`) from `q`, or `None`
/// if not enough is buffered yet.
#[must_use]
pub(crate) fn pop_frame(q: &Samples) -> Option<Vec<i16>> {
    let mut guard = q.lock().expect("samples mutex");
    if guard.len() < FRAME_SAMPLES {
        return None;
    }
    Some(guard.drain(..FRAME_SAMPLES).collect())
}

/// Append `samples` to `q`, dropping the oldest if it would exceed the
/// 1 s cap.
pub(crate) fn push_samples(q: &Samples, samples: &[i16]) {
    let mut guard = q.lock().expect("samples mutex");
    guard.extend(samples.iter().copied());
    let overflow = guard.len().saturating_sub(MAX_BUFFERED);
    if overflow > 0 {
        guard.drain(..overflow);
    }
}

// ---------------------------------------------------------------------------
// Stateful linear resampler (mono f32).
// ---------------------------------------------------------------------------

/// Resamples a continuous mono stream by linear interpolation. `step`
/// is input-samples-per-output-sample (`in_rate / out_rate`); `pos`
/// and `prev` carry interpolation state across callback boundaries so
/// there's no discontinuity at buffer edges.
struct Resampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            step: f64::from(in_rate) / f64::from(out_rate),
            pos: 0.0,
            prev: 0.0,
        }
    }

    /// Resample one buffer. Indices are into the virtual sequence
    /// `[prev, input[0], input[1], …]`, so `-1` reads `prev`.
    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        let len = input.len() as f64;
        while self.pos < len {
            let i = self.pos.floor() as isize;
            let frac = self.pos - i as f64;
            let a = self.sample_at(i, input);
            let b = self.sample_at(i + 1, input);
            out.push((f64::from(a) * (1.0 - frac) + f64::from(b) * frac) as f32);
            self.pos += self.step;
        }
        self.pos -= len;
        if let Some(&last) = input.last() {
            self.prev = last;
        }
        out
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
    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                // Average interleaved channels down to mono f32.
                let mono: Vec<f32> = data
                    .chunks(channels)
                    .map(|frame| {
                        let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                        sum / channels as f32
                    })
                    .collect();
                let resampled = resampler.process(&mono);
                let pcm: Vec<i16> = resampled.iter().map(|s| to_i16(*s)).collect();
                push_samples(&capture, &pcm);
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
    let mut dev_buf: VecDeque<f32> = VecDeque::new();
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
                    let pcm = drain_n(&playback, need_8k);
                    let mono: Vec<f32> = pcm.iter().map(|s| f32::from(*s) / 32768.0).collect();
                    dev_buf.extend(resampler.process(&mono));
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

/// Drain up to `n` samples from `q` (fewer if it underruns).
fn drain_n(q: &Samples, n: usize) -> Vec<i16> {
    let mut guard = q.lock().expect("samples mutex");
    let take = n.min(guard.len());
    guard.drain(..take).collect()
}

/// Clamp an f32 in roughly `[-1, 1]` to `i16` PCM.
fn to_i16(s: f32) -> i16 {
    let scaled = (s * 32767.0).round();
    scaled.clamp(-32768.0, 32767.0) as i16
}
