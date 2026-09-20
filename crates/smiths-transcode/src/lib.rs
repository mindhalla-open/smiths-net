//! Audio transcoding for smiths-net.
//!
//! The engine's default posture is passthrough — when both legs of
//! a call speak the same codec the bridge forwards RTP bytes
//! unchanged. This crate covers the minority case: legs with
//! different codecs. Two impls ship today:
//!
//! - [`G711Codec`] — PCMU (μ-law) ↔ PCM16 LE, always available.
//! - [`OpusCodec`] — Opus ↔ PCM16 LE, behind the `opus` Cargo
//!   feature so deployments without libopus installed still
//!   compile.
//!
//! Transcoding is **expensive by telephony standards**. Opus
//! encode on a single Opus peer leg at 20 ms cadence runs
//! ~50 calls/sec/leg × ~0.5 ms/encode ≈ 25 ms/sec of CPU per
//! call, or 2.5 % of a single core. A 40-call deployment burns
//! one core just on transcoding. That's why this crate's other
//! half — [`CpuBudget`] + admission control — is load-bearing:
//! operators cap concurrency before the engine's scheduler
//! starts dropping RTP frames.
//!
//! ## Admission-control contract
//!
//! The UAS calls [`CpuBudget::try_admit`] at INVITE time whenever
//! the call requires transcoding. If the budget has headroom, the
//! call is admitted; if not, the UAS responds `488 Not Acceptable
//! Here` with a `Warning: 370 transcode budget exhausted` header
//! (RFC 3261 §20.43 — the 370 code reserved for "insufficient
//! bandwidth / resources"). The admission counter decrements on BYE.
//!
//! ## Where the live path lives
//!
//! The per-packet pipeline (RTP in → decode → jitter buffer →
//! re-pace → encode → RTP out) is `smiths_media::transcoded`; this
//! crate provides the codecs, the budget and the metrics it uses.
//!
//! ## What this crate does NOT do
//!
//! - **Video transcoding**. Video is passthrough only.
//! - **Resampling**. PCMU is 8 kHz, Opus at 48 kHz. The
//!   [`OpusCodec`] does its own rate conversion via libopus's
//!   built-in resampler; callers don't have to interleave a
//!   separate resampler.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod budget;
pub mod codec;
pub mod metrics;
pub mod transcoder;

pub use budget::{AdmissionError, CpuBudget, CpuBudgetConfig, TranscodeLease};
pub use codec::{
    Codec, CodecKind, G711Codec, G711Variant, TranscodeError, pcm16_to_pcma, pcma_to_pcm16,
};
pub use metrics::TranscodeMetrics;
pub use transcoder::{CallTranscoder, CpuClock};

#[cfg(feature = "opus")]
pub use codec::OpusCodec;
