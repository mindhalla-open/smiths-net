//! RTCP packet builder and parser (RFC 3550 §6).
//!
//! The bridge terminates RTCP: it emits its own compound packets
//! (Sender Report + SDES CNAME) on every leg and consumes whatever the
//! peer sends back. This module has the wire formats for both sides:
//!
//! - builders for SR (with or without a report block), RR and SDES,
//!   plus [`build_compound`] to concatenate them;
//! - [`parse_compound`], which walks a compound datagram packet by
//!   packet using the length field and returns typed [`RtcpPacket`]s
//!   for SR, RR, SDES and BYE (anything else is reported by type);
//! - [`is_rtcp`], the RFC 5761 demux test the RTP forwarder uses to
//!   spot RTCP multiplexed onto the RTP port;
//! - [`round_trip_time`], the `LSR` / `DLSR` arithmetic behind the
//!   RTT the bridge derives from a peer's report block.
//!
//! APP packets are parsed by type only; the engine has no use for
//! their payload.

use std::time::Duration;

/// RTCP Packet Type for Sender Report.
pub const PT_SENDER_REPORT: u8 = 200;

/// RTCP Packet Type for Receiver Report.
pub const PT_RECEIVER_REPORT: u8 = 201;

/// RTCP Packet Type for Source Description.
pub const PT_SDES: u8 = 202;

/// RTCP Packet Type for Goodbye.
pub const PT_BYE: u8 = 203;

/// RTCP Packet Type for Application-defined packets.
pub const PT_APP: u8 = 204;

/// SDES item type for the canonical name.
pub const SDES_CNAME: u8 = 1;

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

/// Build an SDES packet with a single chunk carrying one CNAME item
/// (RFC 3550 §6.5). The chunk is padded with zero octets to a 32-bit
/// boundary as the wire format requires; `cname` is truncated at 255
/// bytes (the item length field is one octet).
#[must_use]
pub fn build_sdes_cname(ssrc: u32, cname: &str) -> Vec<u8> {
    let name = &cname.as_bytes()[..cname.len().min(255)];
    // chunk = ssrc(4) + item type(1) + len(1) + name + terminator(1),
    // then pad to a multiple of 4.
    let chunk_len = 4 + 2 + name.len() + 1;
    let padded = chunk_len.div_ceil(4) * 4;
    let mut out = Vec::with_capacity(4 + padded);
    out.push(0x81); // V=2, P=0, SC=1
    out.push(PT_SDES);
    // length = words - 1, and the header word counts too.
    let words = u16::try_from(padded / 4).unwrap_or(u16::MAX);
    out.extend_from_slice(&words.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out.push(SDES_CNAME);
    #[allow(clippy::cast_possible_truncation)] // clamped to 255 above
    out.push(name.len() as u8);
    out.extend_from_slice(name);
    out.resize(4 + padded, 0);
    out
}

/// Build a BYE packet for one SSRC with an optional reason string
/// (RFC 3550 §6.6).
#[must_use]
pub fn build_bye(ssrc: u32, reason: Option<&str>) -> Vec<u8> {
    let mut out = vec![0x81, PT_BYE, 0, 0];
    out.extend_from_slice(&ssrc.to_be_bytes());
    if let Some(reason) = reason {
        let text = &reason.as_bytes()[..reason.len().min(255)];
        #[allow(clippy::cast_possible_truncation)] // clamped to 255 above
        out.push(text.len() as u8);
        out.extend_from_slice(text);
        let padded = out.len().div_ceil(4) * 4;
        out.resize(padded, 0);
    }
    let words = u16::try_from(out.len() / 4 - 1).unwrap_or(u16::MAX);
    out[2..4].copy_from_slice(&words.to_be_bytes());
    out
}

/// Concatenate already-built RTCP packets into one compound datagram
/// (RFC 3550 §6.1: SR/RR first, SDES second).
#[must_use]
pub fn build_compound(packets: &[&[u8]]) -> Vec<u8> {
    let total: usize = packets.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(total);
    for p in packets {
        out.extend_from_slice(p);
    }
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

/// RFC 5761 §4 demux test: an RTP-or-RTCP datagram whose second
/// octet (RTP: `M` + `PT`; RTCP: `PT`) falls in 192..=223 is RTCP.
/// RTP payload types 64–95 are unassignable precisely so this check
/// is unambiguous.
#[must_use]
pub fn is_rtcp(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && (bytes[0] >> 6) == 2 && (192..=223).contains(&bytes[1])
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
    Some((sender_ssrc, read_report_blocks(&bytes[8..], rc)))
}

/// Read up to `rc` report blocks from `bytes`, stopping early if the
/// buffer runs out.
fn read_report_blocks(bytes: &[u8], rc: usize) -> Vec<ReportBlock> {
    let mut blocks = Vec::with_capacity(rc.min(bytes.len() / REPORT_BLOCK_LEN));
    for chunk in bytes.chunks_exact(REPORT_BLOCK_LEN).take(rc) {
        blocks.push(read_report_block(chunk));
    }
    blocks
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
    /// SSRC of the sender the report describes.
    pub sender_ssrc: u32,
    /// 64-bit NTP timestamp of the report.
    pub ntp_ts: u64,
    /// RTP timestamp corresponding to `ntp_ts`.
    pub rtp_ts: u32,
    /// Sender's packet count.
    pub packet_count: u32,
    /// Sender's payload octet count.
    pub octet_count: u32,
}

/// Parse an SR packet's sender-info block. Returns `None` if the bytes
/// are malformed or not a sender report. Report blocks after the
/// sender info are ignored; use [`parse_sr_with_blocks`] to get them.
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

/// Parse an SR packet including its report blocks (as many as the RC
/// field claims and the buffer actually holds).
#[must_use]
pub fn parse_sr_with_blocks(bytes: &[u8]) -> Option<(ParsedSr, Vec<ReportBlock>)> {
    let sr = parse_sr(bytes)?;
    let rc = (bytes[0] & 0x1F) as usize;
    Some((sr, read_report_blocks(&bytes[SR_WIRE_LEN..], rc)))
}

/// One SDES chunk: the source it describes plus its CNAME, if any.
/// Other item types are skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdesChunk {
    /// Source the items describe.
    pub ssrc: u32,
    /// Canonical name, when the chunk carried one.
    pub cname: Option<String>,
}

/// One RTCP packet out of a compound datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtcpPacket {
    /// Sender Report with its report blocks.
    SenderReport {
        /// Sender info.
        sr: ParsedSr,
        /// Reception reports the sender is making about streams it
        /// receives.
        blocks: Vec<ReportBlock>,
    },
    /// Receiver Report.
    ReceiverReport {
        /// SSRC of the reporting receiver.
        sender_ssrc: u32,
        /// Reception reports.
        blocks: Vec<ReportBlock>,
    },
    /// Source Description.
    SourceDescription(Vec<SdesChunk>),
    /// Goodbye.
    Bye {
        /// Sources leaving.
        ssrcs: Vec<u32>,
        /// Optional reason for leaving.
        reason: Option<String>,
    },
    /// Any other packet type (APP, feedback, …), reported by type.
    Other {
        /// RTCP packet type.
        payload_type: u8,
    },
}

/// Walk a compound RTCP datagram (RFC 3550 §6.1) and parse every
/// packet in it. Parsing stops at the first packet whose length field
/// runs past the datagram; packets already parsed are still returned.
/// A non-RTCP datagram yields an empty vector.
#[must_use]
pub fn parse_compound(bytes: &[u8]) -> Vec<RtcpPacket> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while rest.len() >= 4 {
        if rest[0] >> 6 != 2 {
            break;
        }
        let words = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        let len = (words + 1) * 4;
        if len > rest.len() {
            break;
        }
        let (pkt, tail) = rest.split_at(len);
        // Padding (P bit) only applies to the last packet; the length
        // field already includes it, so no adjustment is needed for
        // the walk itself.
        if let Some(parsed) = parse_one(pkt) {
            out.push(parsed);
        }
        rest = tail;
    }
    out
}

fn parse_one(pkt: &[u8]) -> Option<RtcpPacket> {
    let count = usize::from(pkt[0] & 0x1F);
    match pkt[1] {
        PT_SENDER_REPORT => {
            let (sr, blocks) = parse_sr_with_blocks(pkt)?;
            Some(RtcpPacket::SenderReport { sr, blocks })
        }
        PT_RECEIVER_REPORT => {
            let (sender_ssrc, blocks) = parse_rr(pkt)?;
            Some(RtcpPacket::ReceiverReport {
                sender_ssrc,
                blocks,
            })
        }
        PT_SDES => Some(RtcpPacket::SourceDescription(parse_sdes_chunks(
            &pkt[4..],
            count,
        ))),
        PT_BYE => {
            let mut ssrcs = Vec::with_capacity(count);
            let mut pos = 4;
            for _ in 0..count {
                if pos + 4 > pkt.len() {
                    break;
                }
                ssrcs.push(u32::from_be_bytes([
                    pkt[pos],
                    pkt[pos + 1],
                    pkt[pos + 2],
                    pkt[pos + 3],
                ]));
                pos += 4;
            }
            let reason = (pos < pkt.len()).then(|| {
                let n = usize::from(pkt[pos]).min(pkt.len() - pos - 1);
                String::from_utf8_lossy(&pkt[pos + 1..pos + 1 + n]).into_owned()
            });
            Some(RtcpPacket::Bye { ssrcs, reason })
        }
        other => Some(RtcpPacket::Other {
            payload_type: other,
        }),
    }
}

/// Parse `count` SDES chunks. Each chunk is `SSRC` followed by items
/// (`type`, `len`, `text`) terminated by a zero type octet and padded
/// to a 32-bit boundary.
fn parse_sdes_chunks(mut bytes: &[u8], count: usize) -> Vec<SdesChunk> {
    let mut chunks = Vec::with_capacity(count);
    for _ in 0..count {
        if bytes.len() < 4 {
            break;
        }
        let ssrc = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let mut pos = 4;
        let mut cname = None;
        while let Some(&item_type) = bytes.get(pos) {
            if item_type == 0 {
                pos += 1;
                break;
            }
            let Some(&len) = bytes.get(pos + 1) else {
                break;
            };
            let start = pos + 2;
            let end = start + usize::from(len);
            if end > bytes.len() {
                break;
            }
            if item_type == SDES_CNAME && cname.is_none() {
                cname = Some(String::from_utf8_lossy(&bytes[start..end]).into_owned());
            }
            pos = end;
        }
        chunks.push(SdesChunk { ssrc, cname });
        // Items end on a 32-bit boundary (the terminator is followed by
        // zero padding up to it).
        let padded = pos.div_ceil(4) * 4;
        bytes = bytes.get(padded..).unwrap_or(&[]);
    }
    chunks
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

/// Middle 32 bits of a 64-bit NTP timestamp — the compact form the
/// `LSR` field and RTT arithmetic use (units of 1/65536 s).
#[must_use]
#[allow(clippy::cast_possible_truncation)] // middle 32 bits by construction
pub fn ntp_middle(ntp: u64) -> u32 {
    (ntp >> 16) as u32
}

/// Express a delay as `DLSR` units (1/65536 s), saturating at `u32::MAX`.
#[must_use]
pub fn to_dlsr(delay: Duration) -> u32 {
    u32::try_from(delay.as_micros() * 65_536 / 1_000_000).unwrap_or(u32::MAX)
}

/// RFC 3550 §6.4.1 round-trip time: `A − LSR − DLSR`, where `A` is
/// the arrival time of the report block in NTP-middle units. Returns
/// `None` when the block never saw one of our SRs (`LSR == 0`) or the
/// arithmetic is nonsensical (a stale echo older than 18 hours).
#[must_use]
pub fn round_trip_time(arrival_ntp_mid: u32, rb: &ReportBlock) -> Option<Duration> {
    if rb.last_sr == 0 {
        return None;
    }
    let ticks = arrival_ntp_mid
        .wrapping_sub(rb.last_sr)
        .wrapping_sub(rb.delay_since_last_sr);
    // A genuine RTT is a fraction of a second; anything that wraps
    // into the top bit is a clock or echo error, not a measurement.
    if ticks & 0x8000_0000 != 0 {
        return None;
    }
    Some(Duration::from_micros(u64::from(ticks) * 1_000_000 / 65_536))
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

        let (parsed, blocks) = parse_sr_with_blocks(&bytes).expect("parse SR");
        assert_eq!(parsed.sender_ssrc, 0x1111_1111);
        assert_eq!(parsed.packet_count, 100);
        assert_eq!(blocks, vec![rb]);
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
    fn sdes_cname_round_trips_with_padding() {
        for cname in ["a", "ab", "abc", "abcd", "smiths-net@0xdeadbeef"] {
            let bytes = build_sdes_cname(0xDEAD_BEEF, cname);
            assert_eq!(bytes.len() % 4, 0, "SDES must be 32-bit aligned");
            let words = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
            assert_eq!((words + 1) * 4, bytes.len(), "length field matches");
            let parsed = parse_compound(&bytes);
            assert_eq!(
                parsed,
                vec![RtcpPacket::SourceDescription(vec![SdesChunk {
                    ssrc: 0xDEAD_BEEF,
                    cname: Some(cname.to_owned()),
                }])]
            );
        }
    }

    #[test]
    fn bye_round_trips_with_and_without_reason() {
        let plain = build_bye(7, None);
        assert_eq!(plain.len(), 8);
        assert_eq!(
            parse_compound(&plain),
            vec![RtcpPacket::Bye {
                ssrcs: vec![7],
                reason: None
            }]
        );
        let with_reason = build_bye(7, Some("teardown"));
        assert_eq!(with_reason.len() % 4, 0);
        assert_eq!(
            parse_compound(&with_reason),
            vec![RtcpPacket::Bye {
                ssrcs: vec![7],
                reason: Some("teardown".into())
            }]
        );
    }

    #[test]
    fn compound_walk_yields_every_packet_in_order() {
        let rb = sample_rb();
        let sr = build_sr_with_rb(1, 2, 3, 4, 5, &rb);
        let sdes = build_sdes_cname(1, "one");
        let rr = build_rr(9, &rb);
        let bye = build_bye(1, Some("bye"));
        let app = [0x80, PT_APP, 0, 2, 0, 0, 0, 1, b'n', b'a', b'm', b'e'];
        let compound = build_compound(&[&sr, &sdes, &rr, &bye, &app]);
        let parsed = parse_compound(&compound);
        assert_eq!(parsed.len(), 5);
        assert!(matches!(
            &parsed[0],
            RtcpPacket::SenderReport { sr, blocks } if sr.sender_ssrc == 1 && blocks.len() == 1
        ));
        assert!(matches!(&parsed[1], RtcpPacket::SourceDescription(c) if c.len() == 1));
        assert!(matches!(
            &parsed[2],
            RtcpPacket::ReceiverReport { sender_ssrc: 9, blocks } if blocks[0] == rb
        ));
        assert!(matches!(&parsed[3], RtcpPacket::Bye { ssrcs, .. } if ssrcs == &[1]));
        assert_eq!(
            parsed[4],
            RtcpPacket::Other {
                payload_type: PT_APP
            }
        );
    }

    #[test]
    fn compound_walk_stops_at_truncation_and_ignores_garbage() {
        let sr = build_sr(1, 2, 3, 4, 5);
        let sdes = build_sdes_cname(1, "one");
        let mut compound = build_compound(&[&sr, &sdes]);
        compound.truncate(sr.len() + 6); // second packet cut short
        let parsed = parse_compound(&compound);
        assert_eq!(parsed.len(), 1, "only the intact SR is returned");
        assert!(parse_compound(b"not rtcp at all").is_empty());
        assert!(parse_compound(&[]).is_empty());
    }

    #[test]
    fn is_rtcp_separates_rtcp_from_rtp() {
        assert!(is_rtcp(&build_sr(1, 2, 3, 4, 5)));
        assert!(is_rtcp(&build_rr(1, &sample_rb())));
        assert!(is_rtcp(&build_bye(1, None)));
        // RTP with PT 0 and marker set: byte1 = 0x80, not RTCP.
        let rtp = [0x80, 0x80, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        assert!(!is_rtcp(&rtp));
        // RTP with PT 101 (telephone-event): byte1 = 101.
        let rtp_dtmf = [0x80, 101, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        assert!(!is_rtcp(&rtp_dtmf));
        assert!(!is_rtcp(&[0x80, 200]), "too short to be either");
    }

    #[test]
    fn round_trip_time_from_lsr_dlsr() {
        // Our SR left at NTP-middle 1_000_000; the peer echoed it after
        // holding it for 0.25 s (16_384 units); the RR arrived at
        // 1_000_000 + 65_536 (1 s later) → RTT = 0.75 s.
        let rb = ReportBlock {
            last_sr: 1_000_000,
            delay_since_last_sr: 16_384,
            ..sample_rb()
        };
        let rtt = round_trip_time(1_000_000 + 65_536, &rb).expect("rtt");
        assert_eq!(rtt, Duration::from_millis(750));
        // No SR echoed → no RTT.
        let none = ReportBlock {
            last_sr: 0,
            ..sample_rb()
        };
        assert!(round_trip_time(5, &none).is_none());
        // Echo from the "future" (clock skew) → rejected.
        let skew = ReportBlock {
            last_sr: 2_000_000,
            delay_since_last_sr: 0,
            ..sample_rb()
        };
        assert!(round_trip_time(1_000_000, &skew).is_none());
    }

    #[test]
    fn dlsr_and_ntp_middle_units() {
        assert_eq!(to_dlsr(Duration::from_secs(1)), 65_536);
        assert_eq!(to_dlsr(Duration::from_millis(500)), 32_768);
        assert_eq!(ntp_middle(0x1234_5678_9ABC_DEF0), 0x5678_9ABC);
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
