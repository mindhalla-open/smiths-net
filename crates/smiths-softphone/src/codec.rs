//! G.711 μ-law framing for the softphone.
//!
//! The math itself lives in `smiths-core` (`pcm16_to_pcmu` /
//! `pcmu_to_pcm16`) and is the bit-exact implementation the engine
//! uses, so client and engine agree on every sample. This module is a
//! thin re-export plus the one constant the whole RTP/audio pipeline
//! is pinned to: a 20 ms frame at 8 kHz mono is exactly 160 samples
//! (and, after μ-law encode, 160 bytes).

pub(crate) use smiths_core::{pcm16_to_pcmu, pcmu_to_pcm16};

/// Samples per 20 ms frame at 8 kHz. PCMU is 1 byte/sample, so this is
/// also the μ-law payload size of one RTP packet.
pub(crate) const FRAME_SAMPLES: usize = 160;

/// Sample rate the SIP/RTP side of the pipeline runs at. G.711 is
/// always 8 kHz; the audio device side resamples to/from this.
pub(crate) const RTP_SAMPLE_RATE: u32 = 8_000;

/// Frame cadence — one RTP packet every 20 ms.
pub(crate) const FRAME_MS: u64 = 20;
