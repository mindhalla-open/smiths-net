//! Minimal RTP (RFC 3550) packet encode/decode.
//!
//! Enough for the audio call tests: version 2, no extensions, no
//! CSRCs, no padding. The engine's byte-level bridge doesn't inspect
//! RTP headers today; this type exists so the test harness can build
//! and verify packets.

/// A minimal RTP packet.
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
        let mut out = Vec::with_capacity(12 + self.payload.len());
        // Version=2, no padding, no extension, CC=0.
        out.push(0b1000_0000);
        out.push((u8::from(self.marker) << 7) | (self.payload_type & 0x7F));
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse wire bytes. Returns `None` if the header is malformed or
    /// reports an unsupported RTP variant (extensions, CSRCs, padding).
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 12 {
            return None;
        }
        let b0 = bytes[0];
        if (b0 >> 6) != 2 {
            return None; // version must be 2
        }
        // Strict: reject extensions, CSRCs, padding for this minimal
        // implementation. Real RTP stacks handle all three.
        if (b0 & 0b0011_1111) != 0 {
            return None;
        }
        let b1 = bytes[1];
        let marker = (b1 & 0x80) != 0;
        let payload_type = b1 & 0x7F;
        let sequence = u16::from_be_bytes([bytes[2], bytes[3]]);
        let timestamp = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let ssrc = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Some(Self {
            marker,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            payload: bytes[12..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_fields() {
        let p = RtpPacket {
            marker: true,
            payload_type: 0,
            sequence: 42,
            timestamp: 12_345,
            ssrc: 0xDEAD_BEEF,
            payload: vec![1, 2, 3, 4],
        };
        let bytes = p.encode();
        let back = RtpPacket::decode(&bytes).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rejects_unsupported_rtp_variants() {
        // Extension bit set (0b0001_0000).
        let mut bad = RtpPacket {
            marker: false,
            payload_type: 0,
            sequence: 0,
            timestamp: 0,
            ssrc: 0,
            payload: vec![],
        }
        .encode();
        bad[0] |= 0b0001_0000;
        assert!(RtpPacket::decode(&bad).is_none());
    }
}
