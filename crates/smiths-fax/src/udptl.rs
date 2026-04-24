//! UDPTL framing (ITU-T T.38 Annex A).
//!
//! Each UDPTL datagram carries:
//!
//! ```text
//! +-----------+-------------------+---------------------------+
//! | Seq (u16) | Primary IFP field | Error Recovery field      |
//! +-----------+-------------------+---------------------------+
//! ```
//!
//! Sequence number is a big-endian `u16` that wraps (same semantics
//! as RTP's). The primary IFP field is a length-prefixed opaque
//! bytestring — the length uses T.38's "OpenType length" encoding:
//! 1 byte when the length is ≤ 127, or 2 bytes big-endian (high bit
//! of the first byte set) for lengths 128..=16383. The error-recovery
//! field is either "secondary packets" (T.38 Annex A.1) — 1-byte
//! count followed by that many length-prefixed IFP copies of the
//! most-recent packets — or "FEC" (Annex A.2), discriminated by a
//! single type byte.
//!
//! This module implements the secondary-packets form. FEC is rarer
//! and, for a relay that only forwards datagrams, indistinguishable
//! on-the-wire byte-for-byte — the FEC payload is still opaque
//! "repair bytes" that the far-end terminal reconstructs with. When
//! the engine needs to *parse* FEC (for instance to re-transmit a
//! recovered primary), this module is where that parser would land.

use thiserror::Error;

/// Errors returned by the UDPTL parser.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UdptlError {
    /// Datagram shorter than the mandatory 2-byte sequence number +
    /// 1-byte primary length prefix + 1-byte error-recovery count.
    #[error("udptl datagram truncated (need at least 4 bytes, got {0})")]
    Truncated(usize),
    /// A length prefix pointed past the end of the input slice.
    #[error("udptl length prefix overran the datagram")]
    LengthOverflow,
    /// Secondary-packet count did not match the number of length-
    /// prefixed frames in the error-recovery field.
    #[error("udptl error-recovery field malformed")]
    MalformedErrorRecovery,
    /// Primary IFP payload exceeded the 16 383-byte ceiling the
    /// 2-byte length prefix can express.
    #[error("udptl payload exceeds 16383 bytes")]
    PayloadTooLarge,
}

/// A parsed UDPTL datagram.
///
/// `primary` holds the most-recent IFP packet; `secondary` holds
/// older copies in T.38-order — `secondary[0]` is the last-but-one
/// primary, `secondary[1]` the one before that, and so on. The
/// receiver uses this for loss recovery: if primary packet N is lost
/// and packet N+1 arrives, it finds N under `secondary[0]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdptlPacket {
    /// Sequence number, wrapping at 2^16 — exactly like RTP.
    pub sequence: u16,
    /// Primary IFP payload (opaque to this crate).
    pub primary: Vec<u8>,
    /// Secondary (redundancy) IFP payloads. Zero-length in the
    /// no-redundancy mode.
    pub secondary: Vec<Vec<u8>>,
}

impl UdptlPacket {
    /// Minimum viable datagram size: seq(2) + primary-length(1) +
    /// error-recovery-count(1). A fully-empty-primary, zero-redundancy
    /// UDPTL datagram is 4 bytes.
    pub const MIN_LEN: usize = 4;

    /// Parse a UDPTL datagram. Borrows into the payloads on success.
    ///
    /// # Errors
    /// Any [`UdptlError`] variant, depending on the malformation.
    pub fn parse(bytes: &[u8]) -> Result<Self, UdptlError> {
        if bytes.len() < Self::MIN_LEN {
            return Err(UdptlError::Truncated(bytes.len()));
        }
        let sequence = u16::from_be_bytes([bytes[0], bytes[1]]);
        let mut cursor = 2;

        let (primary, adv) = read_len_prefixed(&bytes[cursor..])?;
        cursor += adv;

        if bytes.len() <= cursor {
            return Err(UdptlError::MalformedErrorRecovery);
        }
        let count = bytes[cursor];
        cursor += 1;
        let mut secondary = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let (entry, adv) = read_len_prefixed(&bytes[cursor..])?;
            cursor += adv;
            secondary.push(entry.to_vec());
        }

        // Tolerate trailing bytes rather than rejecting — some real
        // gateways pad datagrams to a fixed length. The receiver
        // doesn't care; we shouldn't either.
        Ok(Self {
            sequence,
            primary: primary.to_vec(),
            secondary,
        })
    }

    /// Serialize the datagram to wire bytes.
    ///
    /// # Errors
    /// [`UdptlError::PayloadTooLarge`] if the primary or any
    /// secondary payload exceeds the 16 383-byte ceiling the length
    /// prefix can express.
    pub fn encode(&self) -> Result<Vec<u8>, UdptlError> {
        let mut out = Vec::with_capacity(self.estimate_len());
        out.extend_from_slice(&self.sequence.to_be_bytes());
        write_len_prefixed(&mut out, &self.primary)?;
        let count =
            u8::try_from(self.secondary.len()).map_err(|_| UdptlError::MalformedErrorRecovery)?;
        out.push(count);
        for s in &self.secondary {
            write_len_prefixed(&mut out, s)?;
        }
        Ok(out)
    }

    fn estimate_len(&self) -> usize {
        let mut n = 2 + 1 + self.primary.len() + 1;
        for s in &self.secondary {
            n += 1 + s.len();
        }
        n
    }
}

fn read_len_prefixed(bytes: &[u8]) -> Result<(&[u8], usize), UdptlError> {
    let (len, hdr) = read_length(bytes)?;
    let end = hdr.checked_add(len).ok_or(UdptlError::LengthOverflow)?;
    if end > bytes.len() {
        return Err(UdptlError::LengthOverflow);
    }
    Ok((&bytes[hdr..end], end))
}

/// Read T.38 OpenType length. Returns `(length, header_bytes)`.
fn read_length(bytes: &[u8]) -> Result<(usize, usize), UdptlError> {
    if bytes.is_empty() {
        return Err(UdptlError::LengthOverflow);
    }
    let b0 = bytes[0];
    if b0 & 0x80 == 0 {
        // short form — length in low 7 bits.
        Ok((usize::from(b0), 1))
    } else {
        if bytes.len() < 2 {
            return Err(UdptlError::LengthOverflow);
        }
        let hi = usize::from(b0 & 0x7F);
        let lo = usize::from(bytes[1]);
        Ok(((hi << 8) | lo, 2))
    }
}

fn write_len_prefixed(out: &mut Vec<u8>, payload: &[u8]) -> Result<(), UdptlError> {
    let len = payload.len();
    if len > 16_383 {
        return Err(UdptlError::PayloadTooLarge);
    }
    if len < 128 {
        #[allow(clippy::cast_possible_truncation)] // len < 128
        out.push(len as u8);
    } else {
        #[allow(clippy::cast_possible_truncation)] // bounded above
        let hi = ((len >> 8) as u8) | 0x80;
        #[allow(clippy::cast_possible_truncation)] // low byte
        let lo = len as u8;
        out.push(hi);
        out.push(lo);
    }
    out.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_length_round_trip() {
        let pkt = UdptlPacket {
            sequence: 0x1234,
            primary: b"hello".to_vec(),
            secondary: vec![b"older".to_vec(), b"even older".to_vec()],
        };
        let wire = pkt.encode().unwrap();
        assert_eq!(wire[0..2], [0x12, 0x34]);
        assert_eq!(wire[2], 5);
        assert_eq!(&wire[3..8], b"hello");
        assert_eq!(wire[8], 2); // secondary count
        let back = UdptlPacket::parse(&wire).unwrap();
        assert_eq!(back, pkt);
    }

    #[test]
    fn long_length_round_trip() {
        // A 200-byte primary forces the 2-byte length prefix.
        let primary = vec![0xAB_u8; 200];
        let pkt = UdptlPacket {
            sequence: 42,
            primary: primary.clone(),
            secondary: vec![],
        };
        let wire = pkt.encode().unwrap();
        // Length prefix: high bit set, length 200 = 0x00C8 → 0x80 0xC8.
        assert_eq!(wire[2], 0x80);
        assert_eq!(wire[3], 0xC8);
        assert_eq!(&wire[4..204], &primary[..]);
        assert_eq!(wire[204], 0); // no secondary
        let back = UdptlPacket::parse(&wire).unwrap();
        assert_eq!(back.sequence, 42);
        assert_eq!(back.primary, primary);
    }

    #[test]
    fn truncated_rejected() {
        // 3 bytes — below MIN_LEN.
        assert_eq!(
            UdptlPacket::parse(&[0x00, 0x01, 0x00]),
            Err(UdptlError::Truncated(3))
        );
    }

    #[test]
    fn length_overflow_rejected() {
        // 4 bytes clears MIN_LEN; primary-length claims 10 bytes
        // but only 1 byte follows the prefix.
        let bytes = [0x00, 0x01, 0x0A, 0xFF];
        assert_eq!(UdptlPacket::parse(&bytes), Err(UdptlError::LengthOverflow));
    }

    #[test]
    fn malformed_error_recovery_when_count_missing() {
        // seq(2) + primary-len=1 + primary(1) = 4 bytes total. No
        // count byte follows — the datagram is valid up to the end
        // of the primary and then stops.
        let bytes = [0x00, 0x01, 0x01, 0xAB];
        assert_eq!(
            UdptlPacket::parse(&bytes),
            Err(UdptlError::MalformedErrorRecovery)
        );
    }

    #[test]
    fn payload_too_large_rejected_on_encode() {
        let pkt = UdptlPacket {
            sequence: 0,
            primary: vec![0; 16_384],
            secondary: vec![],
        };
        assert_eq!(pkt.encode(), Err(UdptlError::PayloadTooLarge));
    }

    #[test]
    fn trailing_bytes_are_tolerated() {
        // Encode a valid datagram, append a few pad bytes, reparse.
        let pkt = UdptlPacket {
            sequence: 7,
            primary: b"x".to_vec(),
            secondary: vec![],
        };
        let mut wire = pkt.encode().unwrap();
        wire.extend_from_slice(&[0, 0, 0]);
        let back = UdptlPacket::parse(&wire).unwrap();
        assert_eq!(back, pkt);
    }

    #[test]
    fn boundary_length_127_uses_short_form() {
        let primary = vec![0x55_u8; 127];
        let pkt = UdptlPacket {
            sequence: 0,
            primary: primary.clone(),
            secondary: vec![],
        };
        let wire = pkt.encode().unwrap();
        // Short-form length prefix fits 127 in one byte.
        assert_eq!(wire[2], 0x7F);
        assert_eq!(&wire[3..130], &primary[..]);
    }

    #[test]
    fn boundary_length_128_uses_long_form() {
        let primary = vec![0x55_u8; 128];
        let pkt = UdptlPacket {
            sequence: 0,
            primary,
            secondary: vec![],
        };
        let wire = pkt.encode().unwrap();
        assert_eq!(wire[2], 0x80);
        assert_eq!(wire[3], 0x80);
    }
}
