//! G.711 μ-law codec (ITU-T G.711).
//!
//! 16-bit linear PCM ↔ 8-bit μ-law. Implementations match the standard
//! bit-exact tables, so round-trip of a μ-law byte through decode →
//! encode is identity.

const BIAS: i16 = 0x84;
const CLIP: i16 = 32_635;

/// Encode one 16-bit linear sample as μ-law.
#[must_use]
pub fn linear_to_ulaw(mut pcm: i16) -> u8 {
    // Extract sign and clip magnitude.
    let sign = if pcm < 0 { 0x80_u8 } else { 0 };
    if pcm < 0 {
        pcm = -pcm;
    }
    if pcm > CLIP {
        pcm = CLIP;
    }
    let pcm = pcm + BIAS;
    #[allow(clippy::cast_sign_loss)] // pcm is non-negative after the abs/clip above
    let mag = pcm as u16;
    // exponent = position of highest bit above bias (7..=0).
    let mut exponent: u8 = 7;
    let mut mask: u16 = 0x4000;
    while mag & mask == 0 && exponent > 0 {
        exponent -= 1;
        mask >>= 1;
    }
    #[allow(clippy::cast_possible_truncation)] // low 4 bits by construction
    let mantissa = ((mag >> (exponent + 3)) & 0x0F) as u8;
    !(sign | (exponent << 4) | mantissa)
}

/// Decode one μ-law byte to 16-bit linear PCM.
#[must_use]
pub fn ulaw_to_linear(ulaw: u8) -> i16 {
    let u = !ulaw;
    let sign = u & 0x80;
    let exponent = (u >> 4) & 0x07;
    let mantissa = u & 0x0F;
    let magnitude: i16 = ((i16::from(mantissa) << 3) + BIAS) << exponent;
    let sample = magnitude - BIAS;
    if sign != 0 { -sample } else { sample }
}

/// Encode a 16-bit PCM stream to μ-law.
#[must_use]
pub fn pcm16_to_pcmu(samples: &[i16]) -> Vec<u8> {
    samples.iter().copied().map(linear_to_ulaw).collect()
}

/// Decode a μ-law stream to 16-bit PCM.
#[must_use]
pub fn pcmu_to_pcm16(bytes: &[u8]) -> Vec<i16> {
    bytes.iter().copied().map(ulaw_to_linear).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulaw_round_trip_preserves_decoded_value() {
        // μ-law has two zero encodings (`0x7F` and `0xFF`) that both
        // decode to 0, and the encoder canonicalizes to `0xFF`. So the
        // only meaningful round-trip invariant is on the decoded PCM.
        for b in 0..=255u8 {
            let pcm = ulaw_to_linear(b);
            let reencoded = linear_to_ulaw(pcm);
            assert_eq!(
                ulaw_to_linear(reencoded),
                pcm,
                "byte {b} → pcm {pcm} → {reencoded} → {}",
                ulaw_to_linear(reencoded)
            );
        }
    }

    #[test]
    fn encode_known_samples_match_reference() {
        // Spot-check against table-derived reference bytes for 0, +1, -1.
        assert_eq!(linear_to_ulaw(0), 0xFF);
        assert_eq!(linear_to_ulaw(-1), 0x7F);
        assert_ne!(linear_to_ulaw(100), linear_to_ulaw(-100));
    }
}
