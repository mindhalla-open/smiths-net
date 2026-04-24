//! Codec abstractions + concrete implementations.
//!
//! The [`Codec`] trait is deliberately narrow: encode one RTP
//! payload's worth of samples to wire bytes, decode one wire payload
//! back to samples. Framing (RTP header, SSRC rewrite, timestamp
//! stepping) is the bridge's job — this trait is pure audio.
//!
//! PCM representation on the codec boundary is **16-bit signed
//! little-endian**. That matches every `smiths-core` helper and the
//! `opus` crate's `encode`/`decode` signature, so the three
//! integrations line up with zero copies.

use thiserror::Error;

use smiths_core::codec::{pcm16_to_pcmu, pcmu_to_pcm16};

/// Wire-level codec identifier, used by the bridge to route a
/// transcode job and by the admission layer to look up a CPU cost
/// estimate.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CodecKind {
    /// ITU-T G.711 μ-law, 8 kHz. Payload type 0.
    Pcmu,
    /// ITU-T G.711 A-law, 8 kHz. Payload type 8.
    Pcma,
    /// Opus, 48 kHz (internal). Payload type is dynamic.
    Opus,
    /// Linear 16-bit PCM. Never appears on the wire — used as the
    /// internal exchange format between two codec directions inside
    /// a [`CallTranscoder`](crate::CallTranscoder).
    Pcm16,
}

impl CodecKind {
    /// Short human label used in metrics and tracing spans.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pcmu => "pcmu",
            Self::Pcma => "pcma",
            Self::Opus => "opus",
            Self::Pcm16 => "pcm16",
        }
    }
}

/// Which G.711 variant a [`G711Codec`] instance speaks.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum G711Variant {
    /// μ-law (North America, Japan). Payload type 0.
    Pcmu,
    /// A-law (Europe, most of the world). Payload type 8.
    Pcma,
}

impl G711Variant {
    /// The [`CodecKind`] this variant represents on the wire.
    #[must_use]
    pub fn kind(self) -> CodecKind {
        match self {
            Self::Pcmu => CodecKind::Pcmu,
            Self::Pcma => CodecKind::Pcma,
        }
    }
}

/// Errors a codec can raise on encode or decode.
///
/// These bubble up into a SIP `500 Server Internal Error` when they
/// fire mid-call — a transcoding path can't silently drop frames and
/// still be useful — but most of them are boot-time failures
/// ([`CodecUnavailable`](Self::CodecUnavailable)) or frame-shape
/// errors ([`InvalidFrame`](Self::InvalidFrame)) that ring alarms
/// long before a live call hits them.
#[derive(Debug, Error)]
pub enum TranscodeError {
    /// The requested codec needs a feature that wasn't compiled in —
    /// today this is specifically `--features opus` for Opus.
    #[error("codec {0:?} not built in (missing Cargo feature)")]
    CodecUnavailable(CodecKind),
    /// Payload failed codec-specific structural validation
    /// (odd-length PCM16 byte slice, truncated Opus packet, …).
    #[error("invalid frame: {0}")]
    InvalidFrame(&'static str),
    /// The underlying libopus returned an error on encode or decode.
    /// Wrapped as a string so the error type stays feature-agnostic.
    #[error("opus codec error: {0}")]
    Opus(String),
}

/// Encode 16-bit PCM samples to wire bytes / decode wire bytes back
/// to 16-bit PCM samples.
///
/// The trait is synchronous — codec work is pure CPU, never I/O. The
/// bridge calls into it from a Tokio task, but that task is scheduled
/// onto a blocking pool when the job is expensive (Opus); cheap codecs
/// (G.711) run inline on the existing per-bridge task.
pub trait Codec: Send + Sync {
    /// Codec this instance speaks on the wire.
    fn kind(&self) -> CodecKind;
    /// Encode one RTP payload worth of 16-bit PCM samples. The caller
    /// picks the frame size (160 samples @ 8 kHz = 20 ms for G.711;
    /// 960 samples @ 48 kHz = 20 ms for Opus); this method just
    /// converts whatever it's handed.
    ///
    /// # Errors
    /// - [`TranscodeError::InvalidFrame`] if the sample slice has the
    ///   wrong shape for the codec (Opus rejects non-standard frame
    ///   sizes).
    /// - [`TranscodeError::Opus`] when libopus rejects the input.
    fn encode(&mut self, samples: &[i16]) -> Result<Vec<u8>, TranscodeError>;
    /// Decode one RTP payload back to 16-bit PCM samples. See
    /// [`encode`](Self::encode) for framing constraints.
    ///
    /// # Errors
    /// - [`TranscodeError::InvalidFrame`] if the payload is malformed.
    /// - [`TranscodeError::Opus`] when libopus rejects the input.
    fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, TranscodeError>;
}

/// G.711 codec — μ-law or A-law, 8 kHz, 8-bit/sample.
///
/// Cheap: one lookup per sample, no state, no heap between frames.
/// Every production transcoding path must support this variant because
/// PSTN interop expects it and the `default` feature set compiles it
/// in. The A-law branch reuses the textbook bit-twiddling tables;
/// both round-trip cleanly (decode-then-encode is identity modulo
/// μ-law's two zero encodings).
#[derive(Clone, Copy, Debug)]
pub struct G711Codec {
    variant: G711Variant,
}

impl G711Codec {
    /// Build a codec for the given G.711 variant.
    #[must_use]
    pub fn new(variant: G711Variant) -> Self {
        Self { variant }
    }

    /// μ-law (PCMU) — payload type 0.
    #[must_use]
    pub fn pcmu() -> Self {
        Self::new(G711Variant::Pcmu)
    }

    /// A-law (PCMA) — payload type 8.
    #[must_use]
    pub fn pcma() -> Self {
        Self::new(G711Variant::Pcma)
    }
}

impl Codec for G711Codec {
    fn kind(&self) -> CodecKind {
        self.variant.kind()
    }

    fn encode(&mut self, samples: &[i16]) -> Result<Vec<u8>, TranscodeError> {
        Ok(match self.variant {
            G711Variant::Pcmu => pcm16_to_pcmu(samples),
            G711Variant::Pcma => samples.iter().copied().map(linear_to_alaw).collect(),
        })
    }

    fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, TranscodeError> {
        Ok(match self.variant {
            G711Variant::Pcmu => pcmu_to_pcm16(payload),
            G711Variant::Pcma => payload.iter().copied().map(alaw_to_linear).collect(),
        })
    }
}

// --- A-law lookup ------------------------------------------------
//
// μ-law implementation lives in `smiths-core::codec`; A-law doesn't
// because no other crate had a use for it before slice 5.3. The
// tables here are the ITU-T G.711 reference. Round-trip of any A-law
// byte through decode → encode is identity (A-law doesn't have μ-law's
// dual-zero quirk).

const ALAW_SIGN_BIT: u8 = 0x80;
const ALAW_MANTISSA_MASK: u8 = 0x0F;
const ALAW_EXPONENT_MASK: u8 = 0x70;
const ALAW_EXPONENT_SHIFT: u8 = 4;
const ALAW_EVEN_MASK: u8 = 0x55;

fn linear_to_alaw(mut pcm: i16) -> u8 {
    let sign: u8 = if pcm >= 0 {
        ALAW_SIGN_BIT
    } else {
        pcm = -(pcm + 1);
        0
    };
    let pcm = pcm.clamp(0, 32_767);
    let (exponent, mantissa) = if pcm < 256 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (0_u8, ((pcm >> 4) & 0x0F) as u8)
    } else {
        // seg = floor(log2(pcm >> 8)) + 1, clamped to [1, 7]
        let mut seg: u8 = 1;
        let mut scaled = pcm >> 8;
        while scaled > 1 && seg < 7 {
            scaled >>= 1;
            seg += 1;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mantissa = ((pcm >> (seg + 3)) & 0x0F) as u8;
        (seg, mantissa)
    };
    (sign | (exponent << ALAW_EXPONENT_SHIFT) | mantissa) ^ ALAW_EVEN_MASK
}

fn alaw_to_linear(alaw: u8) -> i16 {
    let a = alaw ^ ALAW_EVEN_MASK;
    let sign = a & ALAW_SIGN_BIT;
    let exponent = (a & ALAW_EXPONENT_MASK) >> ALAW_EXPONENT_SHIFT;
    let mantissa = a & ALAW_MANTISSA_MASK;
    let magnitude: i16 = if exponent == 0 {
        (i16::from(mantissa) << 4) | 0x08
    } else {
        ((i16::from(mantissa) << 4) | 0x108) << (exponent - 1)
    };
    if sign != 0 { magnitude } else { -magnitude }
}

/// Opus codec — 48 kHz internal, supports 8/12/16/24/48 kHz input
/// rates through libopus's built-in resampler.
///
/// Behind the `opus` Cargo feature; absent that, [`OpusCodec::new`]
/// returns [`TranscodeError::CodecUnavailable`] so the failure
/// surfaces at admission time rather than silently miscompiling. The
/// encoder is configured for VoIP (lowest-latency mode, 20 ms frames)
/// which is what an RTP bridge wants — music/archival streaming would
/// pick different knobs.
#[cfg(feature = "opus")]
pub struct OpusCodec {
    encoder: opus::Encoder,
    decoder: opus::Decoder,
    sample_rate_hz: u32,
    channels: u32,
}

#[cfg(feature = "opus")]
impl OpusCodec {
    /// Build an Opus codec at the given sample rate and channel count.
    /// Sample rate must be 8000, 12000, 16000, 24000, or 48000 Hz;
    /// channels must be 1 or 2. Other values error at construction.
    ///
    /// # Errors
    /// [`TranscodeError::Opus`] if libopus rejects the parameters.
    pub fn new(sample_rate_hz: u32, channels: u32) -> Result<Self, TranscodeError> {
        let channels_enum = match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => {
                return Err(TranscodeError::InvalidFrame(
                    "opus supports 1 or 2 channels only",
                ));
            }
        };
        let encoder = opus::Encoder::new(sample_rate_hz, channels_enum, opus::Application::Voip)
            .map_err(|e| TranscodeError::Opus(e.to_string()))?;
        let decoder = opus::Decoder::new(sample_rate_hz, channels_enum)
            .map_err(|e| TranscodeError::Opus(e.to_string()))?;
        Ok(Self {
            encoder,
            decoder,
            sample_rate_hz,
            channels,
        })
    }

    /// Sample rate the codec was built with (Hz).
    #[must_use]
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Channel count the codec was built with.
    #[must_use]
    pub fn channels(&self) -> u32 {
        self.channels
    }
}

#[cfg(feature = "opus")]
impl Codec for OpusCodec {
    fn kind(&self) -> CodecKind {
        CodecKind::Opus
    }

    fn encode(&mut self, samples: &[i16]) -> Result<Vec<u8>, TranscodeError> {
        // Upper bound per RFC 6716 §3.2: 4000 bytes for a 120 ms frame.
        // We only ever feed 20 ms frames, so this is a generous ceiling.
        let mut buf = vec![0_u8; 4000];
        let n = self
            .encoder
            .encode(samples, &mut buf)
            .map_err(|e| TranscodeError::Opus(e.to_string()))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, TranscodeError> {
        // Max frame size libopus can return is 120 ms * 48 kHz = 5760
        // samples per channel.
        let max_samples = 5760 * self.channels as usize;
        let mut out = vec![0_i16; max_samples];
        let n = self
            .decoder
            .decode(payload, &mut out, false)
            .map_err(|e| TranscodeError::Opus(e.to_string()))?;
        out.truncate(n * self.channels as usize);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g711_pcmu_round_trip() {
        let mut c = G711Codec::pcmu();
        assert_eq!(c.kind(), CodecKind::Pcmu);
        // Arbitrary PCM frame; round-trip decode(encode(.)) is lossy
        // but idempotent on the *decoded* value (μ-law has dual zero).
        let samples: Vec<i16> = (0_i16..160).map(|i| (i - 80) * 200).collect();
        let bytes = c.encode(&samples).unwrap();
        assert_eq!(bytes.len(), samples.len());
        let back = c.decode(&bytes).unwrap();
        let re = c.encode(&back).unwrap();
        let back2 = c.decode(&re).unwrap();
        assert_eq!(back, back2, "μ-law round-trip not idempotent on PCM");
    }

    #[test]
    fn g711_pcma_round_trip_is_idempotent_on_pcm() {
        let mut c = G711Codec::pcma();
        assert_eq!(c.kind(), CodecKind::Pcma);
        let samples: Vec<i16> = (0_i16..160).map(|i| (i - 80) * 256).collect();
        let bytes = c.encode(&samples).unwrap();
        assert_eq!(bytes.len(), samples.len());
        let back = c.decode(&bytes).unwrap();
        let re = c.encode(&back).unwrap();
        let back2 = c.decode(&re).unwrap();
        assert_eq!(back, back2, "A-law round-trip not idempotent on PCM");
    }

    #[test]
    fn alaw_decode_encode_is_byte_identity() {
        // A-law has no dual-zero quirk, so `encode(decode(b)) == b` for
        // every possible byte.
        for b in 0..=255u8 {
            let pcm = alaw_to_linear(b);
            let back = linear_to_alaw(pcm);
            assert_eq!(b, back, "A-law byte {b:#04x} round-trip failed");
        }
    }

    #[cfg(feature = "opus")]
    #[test]
    fn opus_round_trip_preserves_frame_shape() {
        // 20 ms @ 48 kHz mono = 960 samples.
        let mut c = OpusCodec::new(48_000, 1).unwrap();
        assert_eq!(c.kind(), CodecKind::Opus);
        let samples = vec![0_i16; 960];
        let bytes = c.encode(&samples).unwrap();
        assert!(!bytes.is_empty());
        let back = c.decode(&bytes).unwrap();
        assert_eq!(back.len(), 960);
    }
}
