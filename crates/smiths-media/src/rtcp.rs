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

/// RTCP Packet Type for Receiver Report.
pub const PT_RECEIVER_REPORT: u8 = 201;

/// Length on the wire of one Report Block (RFC 3550 §6.4.1): 24 bytes.
pub const REPORT_BLOCK_LEN: usize = 24;

/// Wire length of an SR with a single embedded Report Block: 28 + 24.
pub const SR_WITH_ONE_RB_LEN: usize = SR_WIRE_LEN + REPORT_BLOCK_LEN;

/// Receiver-Report block fields. One per sender the receiver has been
/// listening to; our bridge emits one per direction (the peer whose
/// stream we're reporting on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportBlock {
    /// SSRC of the stream this block reports on.
    pub ssrc: u32,
    /// Fraction of packets lost since last report, scaled 0–255.
    pub fraction_lost: u8,
    /// Cumulative packets lost (24-bit signed on the wire).
    pub cumulative_lost: i32,
    /// Extended highest sequence received (top 16 bits = cycle count).
    pub extended_highest_seq: u32,
    /// Interarrival jitter in RTP timestamp units.
    pub jitter: u32,
    /// Last SR NTP timestamp (middle 32 bits of the 64-bit NTP), or
    /// 0 if no SR has been received yet.
    pub last_sr: u32,
    /// Delay since last SR, in units of 1/65536 seconds. `0` when no
    /// SR received.
    pub delay_since_last_sr: u32,
}

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

/// Build a Sender Report with one embedded Receiver-Report block.
/// `length` field in the header becomes `6 + 6 = 12` (length-1 of a
/// 52-byte packet).
#[must_use]
pub fn build_sr_with_rb(
    sender_ssrc: u32,
    ntp_ts: u64,
    rtp_ts: u32,
    packet_count: u32,
    octet_count: u32,
    rb: &ReportBlock,
) -> [u8; SR_WITH_ONE_RB_LEN] {
    let mut out = [0u8; SR_WITH_ONE_RB_LEN];
    out[0] = 0x81; // V=2, P=0, RC=1
    out[1] = PT_SENDER_REPORT;
    out[2..4].copy_from_slice(&12u16.to_be_bytes());
    out[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    out[8..16].copy_from_slice(&ntp_ts.to_be_bytes());
    out[16..20].copy_from_slice(&rtp_ts.to_be_bytes());
    out[20..24].copy_from_slice(&packet_count.to_be_bytes());
    out[24..28].copy_from_slice(&octet_count.to_be_bytes());
    write_report_block(&mut out[28..52], rb);
    out
}

/// Build a bare Receiver Report with one embedded Report Block.
/// Length field = (32/4) - 1 = 7.
#[must_use]
pub fn build_rr(sender_ssrc: u32, rb: &ReportBlock) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[0] = 0x81; // V=2, P=0, RC=1
    out[1] = PT_RECEIVER_REPORT;
    out[2..4].copy_from_slice(&7u16.to_be_bytes());
    out[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    write_report_block(&mut out[8..32], rb);
    out
}

fn write_report_block(buf: &mut [u8], rb: &ReportBlock) {
    debug_assert_eq!(buf.len(), REPORT_BLOCK_LEN);
    buf[0..4].copy_from_slice(&rb.ssrc.to_be_bytes());
    // cumulative_lost is stored as a 24-bit signed integer with an
    // 8-bit fraction_lost prefix; encode as big-endian three bytes.
    #[allow(clippy::cast_sign_loss)] // wire format treats the 24 bits as raw
    let cum = (rb.cumulative_lost as u32) & 0x00FF_FFFF;
    buf[4] = rb.fraction_lost;
    buf[5] = ((cum >> 16) & 0xFF) as u8;
    buf[6] = ((cum >> 8) & 0xFF) as u8;
    buf[7] = (cum & 0xFF) as u8;
    buf[8..12].copy_from_slice(&rb.extended_highest_seq.to_be_bytes());
    buf[12..16].copy_from_slice(&rb.jitter.to_be_bytes());
    buf[16..20].copy_from_slice(&rb.last_sr.to_be_bytes());
    buf[20..24].copy_from_slice(&rb.delay_since_last_sr.to_be_bytes());
}

/// Parse a Receiver Report packet (PT=201). Returns `(sender_ssrc,
/// Vec<ReportBlock>)`. Returns `None` on version/PT mismatch or
/// truncation. The RC field (bits 0..5 of byte 0) tells us how many
/// Report Blocks to parse; we cap at the number the wire actually
/// contains so a lying RC doesn't walk off the end.
#[must_use]
pub fn parse_rr(bytes: &[u8]) -> Option<(u32, Vec<ReportBlock>)> {
    if bytes.len() < 8 {
        return None;
    }
    if bytes[0] >> 6 != 2 {
        return None;
    }
    if bytes[1] != PT_RECEIVER_REPORT {
        return None;
    }
    let rc = (bytes[0] & 0x1F) as usize;
    let sender_ssrc = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let mut blocks = Vec::with_capacity(rc);
    for i in 0..rc {
        let start = 8 + i * REPORT_BLOCK_LEN;
        let end = start + REPORT_BLOCK_LEN;
        if end > bytes.len() {
            break;
        }
        blocks.push(read_report_block(&bytes[start..end]));
    }
    Some((sender_ssrc, blocks))
}

fn read_report_block(buf: &[u8]) -> ReportBlock {
    debug_assert_eq!(buf.len(), REPORT_BLOCK_LEN);
    let ssrc = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let fraction_lost = buf[4];
    // 24-bit signed: sign-extend the top bit.
    let cum_u = (u32::from(buf[5]) << 16) | (u32::from(buf[6]) << 8) | u32::from(buf[7]);
    let cumulative_lost = if cum_u & 0x0080_0000 == 0 {
        #[allow(clippy::cast_possible_wrap)] // 24-bit value fits in i32
        {
            cum_u as i32
        }
    } else {
        #[allow(clippy::cast_possible_wrap)] // sign-extend 24-bit negatives
        {
            (cum_u | 0xFF00_0000) as i32
        }
    };
    ReportBlock {
        ssrc,
        fraction_lost,
        cumulative_lost,
        extended_highest_seq: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        jitter: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
        last_sr: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
        delay_since_last_sr: u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]),
    }
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

    fn sample_rb() -> ReportBlock {
        ReportBlock {
            ssrc: 0xAABB_CCDD,
            fraction_lost: 17,
            cumulative_lost: -42,
            extended_highest_seq: 0x0001_1234,
            jitter: 99,
            last_sr: 0xDEAD_BEEF,
            delay_since_last_sr: 65_536, // 1 second
        }
    }

    #[test]
    fn sr_with_one_rb_round_trips() {
        let rb = sample_rb();
        let bytes = build_sr_with_rb(0x1111_1111, 0x2222_2222_3333_3333, 42, 100, 20_000, &rb);
        assert_eq!(bytes.len(), SR_WITH_ONE_RB_LEN);
        // Header: V=2, RC=1, PT=200, length field should be 12.
        assert_eq!(bytes[0] & 0x1F, 1, "RC must be 1");
        assert_eq!(bytes[1], PT_SENDER_REPORT);
        let len_field = u16::from_be_bytes([bytes[2], bytes[3]]);
        assert_eq!(len_field, 12, "length field (words - 1) must be 12");

        // The underlying SR fields decode with `parse_sr`, which
        // intentionally ignores report blocks past the 28-byte header.
        let parsed = parse_sr(&bytes[..SR_WIRE_LEN]).expect("parse SR prefix");
        assert_eq!(parsed.sender_ssrc, 0x1111_1111);
        assert_eq!(parsed.packet_count, 100);
    }

    #[test]
    fn rr_round_trips_signed_cumulative_lost() {
        let rb = sample_rb();
        let bytes = build_rr(0x9999_9999, &rb);
        let (sender, blocks) = parse_rr(&bytes).expect("parse RR");
        assert_eq!(sender, 0x9999_9999);
        assert_eq!(blocks.len(), 1);
        let got = blocks[0];
        assert_eq!(got.ssrc, rb.ssrc);
        assert_eq!(got.fraction_lost, rb.fraction_lost);
        assert_eq!(
            got.cumulative_lost, rb.cumulative_lost,
            "24-bit signed must round-trip (including negatives)"
        );
        assert_eq!(got.extended_highest_seq, rb.extended_highest_seq);
        assert_eq!(got.jitter, rb.jitter);
        assert_eq!(got.last_sr, rb.last_sr);
        assert_eq!(got.delay_since_last_sr, rb.delay_since_last_sr);
    }

    #[test]
    fn parse_rr_rejects_wrong_pt() {
        let rb = sample_rb();
        let mut bytes = build_rr(1, &rb);
        bytes[1] = PT_SENDER_REPORT;
        assert!(parse_rr(&bytes).is_none());
    }

    #[test]
    fn parse_rr_caps_rc_at_wire_size() {
        let rb = sample_rb();
        let mut bytes = build_rr(1, &rb);
        // Lie: claim 5 blocks, but the packet only has one.
        bytes[0] = (bytes[0] & !0x1F) | 5;
        let (_, blocks) = parse_rr(&bytes).expect("parser must tolerate lying RC");
        assert_eq!(
            blocks.len(),
            1,
            "parser caps at what the wire actually carries"
        );
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
