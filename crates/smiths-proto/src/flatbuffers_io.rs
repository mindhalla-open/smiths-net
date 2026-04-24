//! FlatBuffers-style wire format (slice 5.2 / P18).
//!
//! Hand-rolled flat binary layouts for both [`RtpFrame`] and
//! [`Envelope`] — zero-copy reads via fixed field offsets, no
//! per-field tags, no `flatc` toolchain, no build.rs, no
//! `unsafe`. The "`FlatBuffers`" name is kept because the wire
//! shape matches the layout family (flat, offset-addressable,
//! schema out-of-band); the crate just dodges the official
//! runtime's `unsafe`-heavy raw-table API so smiths-net's
//! `unsafe_code = "deny"` lint stays honoured.
//!
//! ## Why this beats protobuf on the RTP hot path
//!
//! - **Zero-copy payload**: [`RtpFrameView::payload`] returns a
//!   `&[u8]` pointing into the original buffer — no allocation,
//!   no copy. Protobuf's prost decode always allocates a `Vec<u8>`
//!   for the `bytes` field. For a 160-byte PCMU frame at 50
//!   frames/sec/leg that's 8 KiB/s/leg of allocator pressure the
//!   flat path avoids.
//! - **No varint cost**: `ssrc/sequence/timestamp/payload_type` sit
//!   at fixed LE offsets. Protobuf varint encodes 32-bit fields
//!   as 1–5 bytes with a per-byte MSB-check on decode; the flat
//!   layout reads them in one aligned load.
//! - **Partial reads**: plugins that touch only `payload` skip
//!   every other field at zero cost. Protobuf's decode walks the
//!   entire message regardless.
//!
//! The in-tree `wire_format_throughput` bench in `tests/`
//! measures the real round-trip speedup on a 160-byte RTP frame.
//!
//! ## Layout — `RtpFrame`
//!
//! ```text
//! offset  size  field
//! ------  ----  ----------------------------------------------
//! 0       4     magic              = b"SMRF"  ("smiths rtp frame")
//! 4       2     version            = 1 (little-endian u16)
//! 6       2     reserved           = 0 (little-endian u16)
//! 8       4     ssrc               (LE u32)
//! 12      4     sequence           (LE u32)
//! 16      4     timestamp          (LE u32)
//! 20      4     payload_type       (LE u32)
//! 24      2     direction_len      (LE u16)
//! 26      2     call_id_len        (LE u16)
//! 28      4     payload_len        (LE u32)
//! 32      N     direction          (UTF-8, direction_len bytes)
//! 32+N    M     call_id            (UTF-8, call_id_len bytes)
//! 32+N+M  P     payload            (P bytes)
//! ```
//!
//! All three variable-length regions are byte-addressable; no
//! alignment padding. Direction + `call_id` are tiny (<= 40 bytes
//! typical); keeping them unaligned keeps the overall frame
//! compact and the reader trivial.

use crate::{Envelope, RtpFrame, WireFormat, WireFormatError, WireFormatKind, envelope};

/// FlatBuffers-backed [`WireFormat`]. Stateless; cheap to `Clone`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FlatbuffersWireFormat;

const RTP_MAGIC: [u8; 4] = *b"SMRF";
const RTP_VERSION: u16 = 1;
/// Minimum bytes before we start parsing variable-length regions.
const RTP_HEADER_LEN: usize = 32;

impl WireFormat for FlatbuffersWireFormat {
    fn kind(&self) -> WireFormatKind {
        WireFormatKind::Flatbuffers
    }

    fn encode_envelope(&self, envelope: &Envelope) -> Vec<u8> {
        encode_envelope_fb(envelope)
    }

    fn decode_envelope(&self, bytes: &[u8]) -> Result<Envelope, WireFormatError> {
        decode_envelope_fb(bytes)
    }

    fn encode_rtp_frame(&self, frame: &RtpFrame) -> Vec<u8> {
        encode_rtp_frame_fixed(frame)
    }

    fn decode_rtp_frame(&self, bytes: &[u8]) -> Result<RtpFrame, WireFormatError> {
        decode_rtp_frame_fixed(bytes)
    }
}

/// Zero-copy view over an encoded [`RtpFrame`]. Lets a plugin
/// read individual fields without allocating — the engine hands
/// the raw bytes over the host-function boundary and the plugin
/// wraps this view around them.
#[derive(Clone, Copy)]
pub struct RtpFrameView<'a> {
    bytes: &'a [u8],
    direction_start: usize,
    direction_end: usize,
    call_id_start: usize,
    call_id_end: usize,
    payload_start: usize,
    payload_end: usize,
}

impl<'a> RtpFrameView<'a> {
    /// Validate + construct. Returns [`WireFormatError::Malformed`]
    /// on anything the reader can't interpret.
    pub fn new(bytes: &'a [u8]) -> Result<Self, WireFormatError> {
        if bytes.len() < RTP_HEADER_LEN {
            return Err(WireFormatError::Malformed("rtp header too short"));
        }
        if bytes[..4] != RTP_MAGIC {
            return Err(WireFormatError::Malformed("rtp magic mismatch"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != RTP_VERSION {
            return Err(WireFormatError::Malformed("rtp version mismatch"));
        }
        let dir_len = u16::from_le_bytes([bytes[24], bytes[25]]) as usize;
        let call_id_len = u16::from_le_bytes([bytes[26], bytes[27]]) as usize;
        let payload_len = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]) as usize;
        let dir_start = RTP_HEADER_LEN;
        let dir_end = dir_start
            .checked_add(dir_len)
            .ok_or(WireFormatError::Malformed("rtp direction length overflow"))?;
        let call_id_start = dir_end;
        let call_id_end = call_id_start
            .checked_add(call_id_len)
            .ok_or(WireFormatError::Malformed("rtp call_id length overflow"))?;
        let payload_start = call_id_end;
        let payload_end = payload_start
            .checked_add(payload_len)
            .ok_or(WireFormatError::Malformed("rtp payload length overflow"))?;
        if payload_end > bytes.len() {
            return Err(WireFormatError::Malformed("rtp frame truncated"));
        }
        Ok(Self {
            bytes,
            direction_start: dir_start,
            direction_end: dir_end,
            call_id_start,
            call_id_end,
            payload_start,
            payload_end,
        })
    }

    /// RTP SSRC (zero-copy scalar read).
    #[inline]
    #[must_use]
    pub fn ssrc(&self) -> u32 {
        u32::from_le_bytes([self.bytes[8], self.bytes[9], self.bytes[10], self.bytes[11]])
    }

    /// RTP sequence number.
    #[inline]
    #[must_use]
    pub fn sequence(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[12],
            self.bytes[13],
            self.bytes[14],
            self.bytes[15],
        ])
    }

    /// RTP timestamp.
    #[inline]
    #[must_use]
    pub fn timestamp(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[16],
            self.bytes[17],
            self.bytes[18],
            self.bytes[19],
        ])
    }

    /// RTP payload type.
    #[inline]
    #[must_use]
    pub fn payload_type(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[20],
            self.bytes[21],
            self.bytes[22],
            self.bytes[23],
        ])
    }

    /// Direction label (`"a_to_b"` / `"b_to_a"`). Zero-copy
    /// borrow off the backing buffer.
    #[inline]
    #[must_use]
    pub fn direction(&self) -> &'a str {
        std::str::from_utf8(&self.bytes[self.direction_start..self.direction_end]).unwrap_or("")
    }

    /// Call-ID. Zero-copy borrow.
    #[inline]
    #[must_use]
    pub fn call_id(&self) -> &'a str {
        std::str::from_utf8(&self.bytes[self.call_id_start..self.call_id_end]).unwrap_or("")
    }

    /// Raw RTP payload. Zero-copy borrow — most plugins touch only
    /// this field.
    #[inline]
    #[must_use]
    pub fn payload(&self) -> &'a [u8] {
        &self.bytes[self.payload_start..self.payload_end]
    }
}

fn encode_rtp_frame_fixed(frame: &RtpFrame) -> Vec<u8> {
    let direction = frame.direction.as_bytes();
    let call_id = frame.call_id.as_bytes();
    let payload = frame.payload.as_slice();
    let total = RTP_HEADER_LEN
        .saturating_add(direction.len())
        .saturating_add(call_id.len())
        .saturating_add(payload.len());
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&RTP_MAGIC);
    out.extend_from_slice(&RTP_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&frame.ssrc.to_le_bytes());
    out.extend_from_slice(&frame.sequence.to_le_bytes());
    out.extend_from_slice(&frame.timestamp.to_le_bytes());
    out.extend_from_slice(&frame.payload_type.to_le_bytes());
    out.extend_from_slice(
        &u16::try_from(direction.len())
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u16::try_from(call_id.len())
            .unwrap_or(u16::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(direction);
    out.extend_from_slice(call_id);
    out.extend_from_slice(payload);
    out
}

fn decode_rtp_frame_fixed(bytes: &[u8]) -> Result<RtpFrame, WireFormatError> {
    let view = RtpFrameView::new(bytes)?;
    Ok(RtpFrame {
        call_id: view.call_id().to_owned(),
        ssrc: view.ssrc(),
        sequence: view.sequence(),
        timestamp: view.timestamp(),
        payload_type: view.payload_type(),
        direction: view.direction().to_owned(),
        payload: view.payload().to_vec(),
    })
}

// ---------------------------------------------------------------------
// Envelope — hand-rolled flat layout
// ---------------------------------------------------------------------
//
// Layout:
//   offset  size  field
//   ------  ----  --------------------------------------------------
//   0       4     magic = b"SMEV"  ("smiths envelope")
//   4       2     version = 1 (LE u16)
//   6       1     kind (0=none, 1=request, 2=response, 3=notification)
//   7       1     reserved
//   8       8     id (LE u64; request + response only, 0 otherwise)
//   16      4     error_code (LE i32; response only)
//   20      4     method_len (LE u32)
//   24      4     params_len
//   28      4     result_len
//   32      4     err_msg_len
//   36      N+M+R+E   method ++ params ++ result ++ err_msg (UTF-8)
//
// Fields not populated by a given kind are zero-length. No tags,
// no varints — matches the RtpFrame layout family.

const ENV_MAGIC: [u8; 4] = *b"SMEV";
const ENV_VERSION: u16 = 1;
const ENV_HEADER_LEN: usize = 36;

const ENV_KIND_NONE: u8 = 0;
const ENV_KIND_REQUEST: u8 = 1;
const ENV_KIND_RESPONSE: u8 = 2;
const ENV_KIND_NOTIFICATION: u8 = 3;

fn encode_envelope_fb(envelope: &Envelope) -> Vec<u8> {
    let (kind, id, method, params, result, err_msg, err_code) = split_envelope(envelope);
    let method_bytes = method.as_bytes();
    let params_bytes = params.as_bytes();
    let result_bytes = result.as_bytes();
    let err_bytes = err_msg.as_bytes();
    let total = ENV_HEADER_LEN
        .saturating_add(method_bytes.len())
        .saturating_add(params_bytes.len())
        .saturating_add(result_bytes.len())
        .saturating_add(err_bytes.len());
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&ENV_MAGIC);
    out.extend_from_slice(&ENV_VERSION.to_le_bytes());
    out.push(kind);
    out.push(0); // reserved
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&err_code.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(method_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(params_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(result_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(err_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(method_bytes);
    out.extend_from_slice(params_bytes);
    out.extend_from_slice(result_bytes);
    out.extend_from_slice(err_bytes);
    out
}

fn decode_envelope_fb(bytes: &[u8]) -> Result<Envelope, WireFormatError> {
    if bytes.len() < ENV_HEADER_LEN {
        return Err(WireFormatError::Malformed("envelope header too short"));
    }
    if bytes[..4] != ENV_MAGIC {
        return Err(WireFormatError::Malformed("envelope magic mismatch"));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != ENV_VERSION {
        return Err(WireFormatError::Malformed("envelope version mismatch"));
    }
    let kind = bytes[6];
    let id = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    let err_code = i32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let method_len = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize;
    let params_len = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize;
    let result_len = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]) as usize;
    let err_len = u32::from_le_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]) as usize;
    let method_start = ENV_HEADER_LEN;
    let method_end = method_start
        .checked_add(method_len)
        .ok_or(WireFormatError::Malformed("envelope method len overflow"))?;
    let params_end = method_end
        .checked_add(params_len)
        .ok_or(WireFormatError::Malformed("envelope params len overflow"))?;
    let result_end = params_end
        .checked_add(result_len)
        .ok_or(WireFormatError::Malformed("envelope result len overflow"))?;
    let err_end = result_end
        .checked_add(err_len)
        .ok_or(WireFormatError::Malformed("envelope err len overflow"))?;
    if err_end > bytes.len() {
        return Err(WireFormatError::Malformed("envelope truncated"));
    }
    let method = utf8(&bytes[method_start..method_end])?.to_owned();
    let params = utf8(&bytes[method_end..params_end])?.to_owned();
    let result = utf8(&bytes[params_end..result_end])?.to_owned();
    let err_msg = utf8(&bytes[result_end..err_end])?.to_owned();

    let kind_variant = match kind {
        ENV_KIND_NONE => None,
        ENV_KIND_REQUEST => Some(envelope::Kind::Request(crate::Request {
            id,
            method,
            params,
        })),
        ENV_KIND_RESPONSE => Some(envelope::Kind::Response(crate::Response {
            id,
            result,
            error_message: err_msg,
            error_code: err_code,
        })),
        ENV_KIND_NOTIFICATION => Some(envelope::Kind::Notification(crate::Notification {
            method,
            params,
        })),
        _ => return Err(WireFormatError::Malformed("envelope: unknown kind")),
    };
    Ok(Envelope { kind: kind_variant })
}

fn utf8(bytes: &[u8]) -> Result<&str, WireFormatError> {
    std::str::from_utf8(bytes).map_err(|_| WireFormatError::Malformed("envelope UTF-8"))
}

fn split_envelope(envelope: &Envelope) -> (u8, u64, String, String, String, String, i32) {
    match envelope.kind.as_ref() {
        None => (
            ENV_KIND_NONE,
            0,
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0,
        ),
        Some(envelope::Kind::Request(r)) => (
            ENV_KIND_REQUEST,
            r.id,
            r.method.clone(),
            r.params.clone(),
            String::new(),
            String::new(),
            0,
        ),
        Some(envelope::Kind::Response(r)) => (
            ENV_KIND_RESPONSE,
            r.id,
            String::new(),
            String::new(),
            r.result.clone(),
            r.error_message.clone(),
            r.error_code,
        ),
        Some(envelope::Kind::Notification(n)) => (
            ENV_KIND_NOTIFICATION,
            0,
            n.method.clone(),
            n.params.clone(),
            String::new(),
            String::new(),
            0,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Notification, Request, Response};

    fn sample_frame() -> RtpFrame {
        RtpFrame {
            call_id: "abc@smiths.local".into(),
            ssrc: 0xDEAD_BEEF,
            sequence: 12345,
            timestamp: 98_765_432,
            payload_type: 0,
            direction: "a_to_b".into(),
            payload: vec![0xAB; 160], // one PCMU frame at 20 ms / 8 kHz
        }
    }

    #[test]
    fn rtp_frame_round_trips() {
        let wire = FlatbuffersWireFormat;
        let frame = sample_frame();
        let bytes = wire.encode_rtp_frame(&frame);
        let back = wire.decode_rtp_frame(&bytes).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn rtp_frame_view_zero_copy() {
        let wire = FlatbuffersWireFormat;
        let frame = sample_frame();
        let bytes = wire.encode_rtp_frame(&frame);
        let view = RtpFrameView::new(&bytes).unwrap();
        assert_eq!(view.ssrc(), 0xDEAD_BEEF);
        assert_eq!(view.sequence(), 12345);
        assert_eq!(view.timestamp(), 98_765_432);
        assert_eq!(view.payload_type(), 0);
        assert_eq!(view.direction(), "a_to_b");
        assert_eq!(view.call_id(), "abc@smiths.local");
        assert_eq!(view.payload().len(), 160);
        // Prove zero-copy: the payload slice points inside `bytes`.
        let slice_ptr = view.payload().as_ptr() as usize;
        let buf_ptr = bytes.as_ptr() as usize;
        let buf_end = buf_ptr + bytes.len();
        assert!(slice_ptr >= buf_ptr && slice_ptr < buf_end);
    }

    #[test]
    fn rtp_frame_malformed_truncated_is_error() {
        let wire = FlatbuffersWireFormat;
        let bytes = wire.encode_rtp_frame(&sample_frame());
        let err = wire.decode_rtp_frame(&bytes[..16]).unwrap_err();
        assert!(matches!(err, WireFormatError::Malformed(_)));
    }

    #[test]
    fn rtp_frame_magic_mismatch_is_error() {
        let mut bytes = FlatbuffersWireFormat.encode_rtp_frame(&sample_frame());
        bytes[0] = b'X';
        let err = FlatbuffersWireFormat.decode_rtp_frame(&bytes).unwrap_err();
        assert!(matches!(err, WireFormatError::Malformed(_)));
    }

    #[test]
    fn envelope_request_round_trips() {
        let wire = FlatbuffersWireFormat;
        let env = Envelope {
            kind: Some(envelope::Kind::Request(Request {
                id: 42,
                method: "synthesize".into(),
                params: r#"{"text":"hi"}"#.into(),
            })),
        };
        let bytes = wire.encode_envelope(&env);
        let back = wire.decode_envelope(&bytes).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn envelope_response_round_trips() {
        let wire = FlatbuffersWireFormat;
        let env = Envelope {
            kind: Some(envelope::Kind::Response(Response {
                id: 7,
                result: String::new(),
                error_message: "nope".into(),
                error_code: -32601,
            })),
        };
        let bytes = wire.encode_envelope(&env);
        let back = wire.decode_envelope(&bytes).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn envelope_notification_round_trips() {
        let wire = FlatbuffersWireFormat;
        let env = Envelope {
            kind: Some(envelope::Kind::Notification(Notification {
                method: "emit_partial".into(),
                params: r#"{"text":"ho"}"#.into(),
            })),
        };
        let bytes = wire.encode_envelope(&env);
        let back = wire.decode_envelope(&bytes).unwrap();
        assert_eq!(back, env);
    }
}
