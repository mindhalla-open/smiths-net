//! Minimal WAV writer — mono 16-bit PCM only.
//!
//! Enough for tests to write the received audio to disk so a human can
//! play it. Not a general-purpose WAV library.

use std::io::{self, Write};
use std::path::Path;

/// Write `samples` as a mono PCM-16 WAV at `sample_rate` to `path`.
pub fn write_mono_pcm16(path: &Path, sample_rate: u32, samples: &[i16]) -> io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    write_mono_pcm16_into(&mut f, sample_rate, samples)
}

/// Same as [`write_mono_pcm16`] but to any `Write` sink. Useful for
/// in-memory buffers in tests.
pub fn write_mono_pcm16_into<W: Write>(
    w: &mut W,
    sample_rate: u32,
    samples: &[i16],
) -> io::Result<()> {
    let byte_rate = sample_rate * 2;
    let data_bytes: u32 = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let riff_size = 36 + data_bytes;

    w.write_all(b"RIFF")?;
    w.write_all(&riff_size.to_le_bytes())?;
    w.write_all(b"WAVE")?;

    // fmt chunk
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // subchunk size
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // 1 channel
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?; // block align
    w.write_all(&16u16.to_le_bytes())?; // bits per sample

    // data chunk
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    for s in samples {
        w.write_all(&s.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_riff_wave_pcm() {
        let mut buf = Vec::new();
        write_mono_pcm16_into(&mut buf, 8_000, &[0, 1, -1]).unwrap();
        assert!(buf.starts_with(b"RIFF"));
        assert_eq!(&buf[8..12], b"WAVE");
        assert_eq!(&buf[12..16], b"fmt ");
        // 36-byte header + 6 bytes of samples
        assert_eq!(buf.len(), 36 + 8 + 6);
    }
}
