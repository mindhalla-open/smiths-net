//! Minimal RTP (RFC 3550 §5.1) packet encode/decode.
//!
//! Two views of a packet:
//!
//! - [`RtpHeader`] is a zero-copy parse of the fixed header plus the
//!   CSRC list, header extension and padding trailer. Hot paths (the
//!   bridge's DTMF sniffer, the RTCP stats collector, the mixer's
//!   ingress) use it to check the payload type or locate the payload
//!   without allocating.
//! - [`RtpPacket`] is the owned form the test harness, the DTMF
//!   generator and the softphone build packets with. Its encoder
//!   always emits the fixed 12-byte header (`CC=0`, `X=0`, `P=0`);
//!   its decoder accepts any well-formed header and hands back only
//!   the payload bytes.

/// Length of the fixed RTP header (no CSRCs, no extension).
pub const RTP_FIXED_HEADER_LEN: usize = 12;

/// Borrowed view of an RTP header. `payload_start..payload_end`
/// locates the payload inside the datagram the header was parsed
/// from, excluding CSRCs, any header extension and any padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpHeader {
    /// Marker bit — set on the first packet of a talkspurt.
    pub marker: bool,
    /// Payload type (e.g. `0` for PCMU).
    pub payload_type: u8,
    /// Sequence number.
    pub sequence: u16,
    /// Media clock timestamp.
    pub timestamp: u32,
    /// Synchronization source identifier.
    pub ssrc: u32,
    /// Number of CSRC identifiers following the fixed header.
    pub csrc_count: u8,
    /// `true` when an RFC 3550 §5.3.1 header extension is present.
    pub has_extension: bool,
    /// Byte offset of the first payload byte.
    pub payload_start: usize,
    /// Byte offset one past the last payload byte (padding excluded).
    pub payload_end: usize,
}

impl RtpHeader {
    /// Parse the header of `bytes`. Returns `None` when the datagram
    /// is too short, isn't RTP version 2, or declares CSRCs, an
    /// extension or padding that the datagram doesn't actually carry.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < RTP_FIXED_HEADER_LEN {
            return None;
        }
        let b0 = bytes[0];
        if (b0 >> 6) != 2 {
            return None;
        }
        let padding = b0 & 0x20 != 0;
        let has_extension = b0 & 0x10 != 0;
        let csrc_count = b0 & 0x0F;
        let mut start = RTP_FIXED_HEADER_LEN + 4 * usize::from(csrc_count);
        if bytes.len() < start {
            return None;
        }
        if has_extension {
            // Extension header: 16-bit profile id, 16-bit length in
            // 32-bit words (excluding the 4-byte extension header).
            if bytes.len() < start + 4 {
                return None;
            }
            let words = usize::from(u16::from_be_bytes([bytes[start + 2], bytes[start + 3]]));
            start += 4 + 4 * words;
            if bytes.len() < start {
                return None;
            }
        }
        let mut end = bytes.len();
        if padding {
            // Last octet holds the padding length including itself.
            let pad = usize::from(bytes[end - 1]);
            if pad == 0 || pad > end - start {
                return None;
            }
            end -= pad;
        }
        let b1 = bytes[1];
        Some(Self {
            marker: (b1 & 0x80) != 0,
            payload_type: b1 & 0x7F,
            sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
            timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            ssrc: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            csrc_count,
            has_extension,
            payload_start: start,
            payload_end: end,
        })
    }

    /// Payload type of `bytes` if it looks like an RTP packet. Cheaper
    /// than [`Self::parse`] — only the first two octets are inspected —
    /// so callers can gate on the codec before doing any real work.
    #[must_use]
    pub fn payload_type_of(bytes: &[u8]) -> Option<u8> {
        if bytes.len() < RTP_FIXED_HEADER_LEN || (bytes[0] >> 6) != 2 {
            return None;
        }
        Some(bytes[1] & 0x7F)
    }

    /// Slice the payload out of the datagram this header was parsed
    /// from.
    #[must_use]
    pub fn payload<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        &bytes[self.payload_start..self.payload_end]
    }
}

/// An owned RTP packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtpPacket {
    /// Marker bit — set on the first packet of a talkspurt.
    pub marker: bool,
    /// Payload type (e.g. `0` for PCMU).
    pub payload_type: u8,
    /// Monotonically increasing sequence number.
    pub sequence: u16,
    /// Media clock timestamp.
    pub timestamp: u32,
    /// Synchronization source identifier.
    pub ssrc: u32,
    /// Media payload bytes.
    pub payload: Vec<u8>,
}

impl RtpPacket {
    /// Serialize to wire bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(RTP_FIXED_HEADER_LEN + self.payload.len());
        // Version=2, no padding, no extension, CC=0.
        out.push(0b1000_0000);
        out.push((u8::from(self.marker) << 7) | (self.payload_type & 0x7F));
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse wire bytes. CSRCs, a header extension and padding are
    /// consumed and dropped so `payload` holds media bytes only.
    /// Returns `None` if the header is malformed (see
    /// [`RtpHeader::parse`]).
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let hdr = RtpHeader::parse(bytes)?;
        Some(Self {
            marker: hdr.marker,
            payload_type: hdr.payload_type,
            sequence: hdr.sequence,
            timestamp: hdr.timestamp,
            ssrc: hdr.ssrc,
            payload: hdr.payload(bytes).to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RtpPacket {
        RtpPacket {
            marker: true,
            payload_type: 0,
            sequence: 42,
            timestamp: 12_345,
            ssrc: 0xDEAD_BEEF,
            payload: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn round_trip_preserves_fields() {
        let p = sample();
        let bytes = p.encode();
        let back = RtpPacket::decode(&bytes).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn decodes_csrcs_extension_and_padding() {
        // CC=2, X=1, P=1: fixed header, two CSRCs, a one-word
        // extension, 4 payload bytes, then 3 bytes of padding whose
        // last octet says "3".
        let mut bytes = vec![0b1011_0010, 0x00];
        bytes.extend_from_slice(&7u16.to_be_bytes());
        bytes.extend_from_slice(&100u32.to_be_bytes());
        bytes.extend_from_slice(&0xAAAA_AAAAu32.to_be_bytes());
        bytes.extend_from_slice(&0x1111_1111u32.to_be_bytes()); // CSRC 1
        bytes.extend_from_slice(&0x2222_2222u32.to_be_bytes()); // CSRC 2
        bytes.extend_from_slice(&0xBEDEu16.to_be_bytes()); // ext profile
        bytes.extend_from_slice(&1u16.to_be_bytes()); // ext length = 1 word
        bytes.extend_from_slice(&[9, 9, 9, 9]); // ext data
        bytes.extend_from_slice(&[1, 2, 3, 4]); // payload
        bytes.extend_from_slice(&[0, 0, 3]); // padding

        let hdr = RtpHeader::parse(&bytes).expect("parse");
        assert_eq!(hdr.csrc_count, 2);
        assert!(hdr.has_extension);
        assert_eq!(hdr.sequence, 7);
        assert_eq!(hdr.ssrc, 0xAAAA_AAAA);
        assert_eq!(hdr.payload(&bytes), &[1, 2, 3, 4]);

        let pkt = RtpPacket::decode(&bytes).expect("decode");
        assert_eq!(pkt.payload, vec![1, 2, 3, 4]);
        assert_eq!(pkt.sequence, 7);
    }

    #[test]
    fn rejects_truncated_variants() {
        // Extension bit set but no extension header present.
        let mut bad = sample().encode();
        bad.truncate(12);
        bad[0] |= 0b0001_0000;
        assert!(RtpPacket::decode(&bad).is_none());

        // CC=3 but only one CSRC's worth of bytes follow.
        let mut short_csrc = sample().encode();
        short_csrc[0] |= 0b0000_0011;
        short_csrc.truncate(16);
        assert!(RtpHeader::parse(&short_csrc).is_none());

        // Padding flag with a padding count larger than the packet.
        let mut bad_pad = sample().encode();
        bad_pad[0] |= 0b0010_0000;
        *bad_pad.last_mut().unwrap() = 200;
        assert!(RtpHeader::parse(&bad_pad).is_none());

        // Wrong version.
        let mut v1 = sample().encode();
        v1[0] = 0x40;
        assert!(RtpHeader::parse(&v1).is_none());
        assert!(RtpHeader::payload_type_of(&v1).is_none());
    }

    #[test]
    fn payload_type_of_reads_only_the_second_octet() {
        let mut p = sample();
        p.payload_type = 101;
        let bytes = p.encode();
        assert_eq!(RtpHeader::payload_type_of(&bytes), Some(101));
        assert_eq!(RtpHeader::payload_type_of(&bytes[..8]), None);
    }
}
