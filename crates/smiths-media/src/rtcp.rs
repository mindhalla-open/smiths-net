//! RTCP packet builder (RFC 3550 §6.4).
//!
//! Covers the Sender Report (SR, PT=200) since our engine is always a
//! sender in the passthrough bridge. The Receiver Report (RR, PT=201)
//! lands when we split the emission path per direction. Packet parse
//! has a matching `parse_sr` for tests and for future RTCP passthrough.
//!
//! We deliberately don't implement SDES, BYE, or APP here — the MVP
//! bridge just needs receivers to know we're alive and to get
//! sender-side stats. Those can land in follow-on slices when we
//! integrate with external monitoring.

/// RTCP Packet Type for Sender Report.
pub const PT_SENDER_REPORT: u8 = 200;

/// Byte layout of a minimal Sender Report with **no** report blocks:
/// 28 bytes, version=2, padding=0, RC=0, length = (28/4 - 1) = 6.
///
/// ```text
/// 0                   1                   2                   3
/// 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |V=2|P| RC=0 |   PT=200      |           length=6              |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                         SSRC of sender                         |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |              NTP timestamp, most significant word              |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |             NTP timestamp, least significant word              |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                         RTP timestamp                          |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                     sender's packet count                      |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                      sender's octet count                      |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
pub const SR_WIRE_LEN: usize = 28;

/// Build a bare Sender Report (no report blocks).
#[must_use]
pub fn build_sr(
    sender_ssrc: u32,
    ntp_ts: u64,
    rtp_ts: u32,
    packet_count: u32,
    octet_count: u32,
) -> [u8; SR_WIRE_LEN] {
    let mut out = [0u8; SR_WIRE_LEN];
    // First word: V=2, P=0, RC=0, PT=200, length=6 (length-1 = 6).
    out[0] = 0x80; // V=2, P=0, RC=0
    out[1] = PT_SENDER_REPORT;
    out[2..4].copy_from_slice(&6u16.to_be_bytes()); // length
    out[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    out[8..16].copy_from_slice(&ntp_ts.to_be_bytes());
    out[16..20].copy_from_slice(&rtp_ts.to_be_bytes());
    out[20..24].copy_from_slice(&packet_count.to_be_bytes());
    out[24..28].copy_from_slice(&octet_count.to_be_bytes());
    out
}

/// Parsed Sender Report fields (without report blocks). Returned
/// without error for valid input; `None` on any header mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedSr {
    pub sender_ssrc: u32,
    pub ntp_ts: u64,
    pub rtp_ts: u32,
    pub packet_count: u32,
    pub octet_count: u32,
}

/// Parse an SR packet. Returns `None` if the bytes are malformed or
/// not a sender report.
#[must_use]
pub fn parse_sr(bytes: &[u8]) -> Option<ParsedSr> {
    if bytes.len() < SR_WIRE_LEN {
        return None;
    }
    // V=2 in top two bits of byte 0.
    if bytes[0] >> 6 != 2 {
        return None;
    }
    if bytes[1] != PT_SENDER_REPORT {
        return None;
    }
    Some(ParsedSr {
        sender_ssrc: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        ntp_ts: u64::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]),
        rtp_ts: u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
        packet_count: u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        octet_count: u32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
    })
}

/// Encode the current wall-clock as a 64-bit NTP timestamp: the upper
/// 32 bits are seconds since 1900-01-01 (NTP epoch), the lower 32 bits
/// are a fractional second scaled to 2^32.
///
/// RFC 3550 SR fields use this exact encoding.
#[must_use]
pub fn ntp_now() -> u64 {
    // 2_208_988_800 seconds between 1900-01-01 and 1970-01-01 (UNIX).
    const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() + NTP_EPOCH_OFFSET;
    // Scale nanoseconds into a 32-bit fractional second.
    let frac = ((u64::from(d.subsec_nanos())) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sr_round_trips() {
        let bytes = build_sr(0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0, 42, 100, 20_000);
        assert_eq!(bytes.len(), SR_WIRE_LEN);
        let parsed = parse_sr(&bytes).expect("parse");
        assert_eq!(
            parsed,
            ParsedSr {
                sender_ssrc: 0xDEAD_BEEF,
                ntp_ts: 0x1234_5678_9ABC_DEF0,
                rtp_ts: 42,
                packet_count: 100,
                octet_count: 20_000,
            }
        );
    }

    #[test]
    fn parse_rejects_wrong_version() {
        let mut bytes = build_sr(1, 2, 3, 4, 5);
        bytes[0] = 0x40; // V=1
        assert!(parse_sr(&bytes).is_none());
    }

    #[test]
    fn parse_rejects_wrong_pt() {
        let mut bytes = build_sr(1, 2, 3, 4, 5);
        bytes[1] = 201; // RR, not SR
        assert!(parse_sr(&bytes).is_none());
    }

    #[test]
    fn parse_rejects_short_input() {
        assert!(parse_sr(&[0x80, 200, 0, 6]).is_none());
    }

    #[test]
    fn ntp_now_past_2020() {
        // 2020-01-01 in NTP seconds is 3_786_825_600. Any real-world
        // clock should comfortably exceed that.
        let ntp = ntp_now();
        let secs = ntp >> 32;
        assert!(secs > 3_786_825_600, "NTP timestamp looks wrong: {secs}");
    }
}
