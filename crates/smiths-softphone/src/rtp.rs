//! Minimal RTP (RFC 3550) — the same subset the Python demo client
//! speaks (`examples/python-client/smiths_client.py`): version 2, no
//! extensions, no CSRCs, one payload type per session. Just enough to
//! carry G.711 μ-law between the softphone and the engine's media
//! bridge.

/// PCMU (G.711 μ-law) static payload type, per RFC 3551.
pub(crate) const PT_PCMU: u8 = 0;

/// A decoded/encodable RTP packet header + payload.
#[derive(Debug, Clone)]
pub(crate) struct RtpPacket {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    /// Serialize to wire bytes: a 12-byte fixed header followed by the
    /// payload. `V=2, P=0, X=0, CC=0`.
    #[must_use]
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.payload.len());
        out.push(0b1000_0000); // V=2, P=0, X=0, CC=0
        out.push((u8::from(self.marker) << 7) | (self.payload_type & 0x7F));
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Parse wire bytes. Returns `None` for anything that isn't a
    /// plain V=2 packet with no CSRCs (the only shape this client
    /// emits or expects back from the engine).
    #[must_use]
    pub(crate) fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < 12 {
            return None;
        }
        let b0 = buf[0];
        // Require V=2 and CC=0 / X=0 / P=0 (low 6 bits zero).
        if (b0 >> 6) != 2 || (b0 & 0b0011_1111) != 0 {
            return None;
        }
        let b1 = buf[1];
        let sequence = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        Some(Self {
            marker: (b1 & 0x80) != 0,
            payload_type: b1 & 0x7F,
            sequence,
            timestamp,
            ssrc,
            payload: buf[12..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let pkt = RtpPacket {
            marker: true,
            payload_type: PT_PCMU,
            sequence: 4242,
            timestamp: 1_600_000,
            ssrc: 0xDEAD_BEEF,
            payload: vec![1, 2, 3, 4, 5],
        };
        let wire = pkt.encode();
        assert_eq!(wire.len(), 12 + 5);
        let back = RtpPacket::decode(&wire).expect("decode");
        assert_eq!(back.sequence, 4242);
        assert_eq!(back.timestamp, 1_600_000);
        assert_eq!(back.ssrc, 0xDEAD_BEEF);
        assert!(back.marker);
        assert_eq!(back.payload_type, PT_PCMU);
        assert_eq!(back.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn rejects_short_and_malformed() {
        assert!(RtpPacket::decode(&[0u8; 8]).is_none());
        // Version 1 in the top two bits.
        let mut bad = [0u8; 12];
        bad[0] = 0b0100_0000;
        assert!(RtpPacket::decode(&bad).is_none());
    }
}
