//! STUN Binding codec, RFC 8489, with the ICE attributes of RFC 8445
//! §16 and the short-term credential mechanism (RFC 8489 §9.1).
//!
//! What a connectivity check needs on the wire:
//!
//! - `USERNAME` (`remote-ufrag:local-ufrag`), `PRIORITY`,
//!   `USE-CANDIDATE`, `ICE-CONTROLLING` / `ICE-CONTROLLED`.
//! - `MESSAGE-INTEGRITY` — HMAC-SHA1 keyed with the peer's
//!   `ice-pwd`, covering the message up to and including the
//!   attribute itself (§14.5).
//! - `FINGERPRINT` — CRC-32 of everything before it, XOR
//!   `0x5354554e`, always the last attribute (§14.7).
//! - `XOR-MAPPED-ADDRESS` on responses, `ERROR-CODE` on error
//!   responses.
//!
//! [`StunMessage::encode`] emits a bare message (what a public STUN
//! server expects from [`binding_ping`]); [`StunMessage::encode_with`]
//! adds integrity + fingerprint for ICE. Verification runs on the raw
//! datagram — [`verify_message_integrity`] and [`verify_fingerprint`]
//! — because both hashes cover the bytes as sent.
//!
//! Hand-rolled because the third-party STUN crates all pull in
//! heavier ICE stacks; for a 20-byte header + a dozen attributes the
//! codec is small enough to own.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use hmac::{Hmac, Mac as _};
use rand::RngExt as _;
use sha1::Sha1;
use thiserror::Error;
use tokio::net::UdpSocket;
use tracing::debug;

/// STUN magic cookie (RFC 8489 §6).
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// STUN header length in bytes (RFC 8489 §6).
pub const HEADER_LEN: usize = 20;
/// STUN Binding method code (RFC 8489 §12.1). The method is the low
/// 12 bits of the `type` field; the class takes the other 2.
pub const METHOD_BINDING: u16 = 0x0001;
/// Attribute type for `USERNAME` (RFC 8489 §14.3).
pub const ATTR_USERNAME: u16 = 0x0006;
/// Attribute type for `MESSAGE-INTEGRITY` (RFC 8489 §14.5).
pub const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
/// Attribute type for `ERROR-CODE` (RFC 8489 §14.8).
pub const ATTR_ERROR_CODE: u16 = 0x0009;
/// Attribute type for `XOR-MAPPED-ADDRESS` (RFC 8489 §14.2).
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
/// Attribute type for `PRIORITY` (RFC 8445 §16.1).
pub const ATTR_PRIORITY: u16 = 0x0024;
/// Attribute type for `USE-CANDIDATE` (RFC 8445 §16.1).
pub const ATTR_USE_CANDIDATE: u16 = 0x0025;
/// Attribute type for `FINGERPRINT` (RFC 8489 §14.7).
pub const ATTR_FINGERPRINT: u16 = 0x8028;
/// Attribute type for `ICE-CONTROLLED` (RFC 8445 §16.1).
pub const ATTR_ICE_CONTROLLED: u16 = 0x8029;
/// Attribute type for `ICE-CONTROLLING` (RFC 8445 §16.1).
pub const ATTR_ICE_CONTROLLING: u16 = 0x802A;
/// XOR mask applied to the CRC-32 in `FINGERPRINT` (RFC 8489 §14.7).
pub const FINGERPRINT_XOR: u32 = 0x5354_554e;

/// STUN error code 400 (RFC 8489 §14.8).
pub const ERR_BAD_REQUEST: u16 = 400;
/// STUN error code 401 (RFC 8489 §14.8).
pub const ERR_UNAUTHENTICATED: u16 = 401;
/// ICE error code 487 Role Conflict (RFC 8445 §7.3.1.1).
pub const ERR_ROLE_CONFLICT: u16 = 487;

/// Length of a MESSAGE-INTEGRITY value (HMAC-SHA1).
const INTEGRITY_LEN: usize = 20;

/// `true` when `bytes` looks like a STUN message rather than RTP /
/// DTLS on a shared socket (RFC 7983 §7): first byte in `0..=3` and
/// the magic cookie in place.
#[must_use]
pub fn is_stun(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN
        && bytes[0] < 4
        && u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) == MAGIC_COOKIE
}

/// Transaction id — 96 bits of random per RFC 8489 §6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TransactionId(pub [u8; 12]);

impl TransactionId {
    /// Fresh random transaction id.
    #[must_use]
    pub fn random() -> Self {
        // `rand::rng` is the thread-local CSPRNG — it pulls from
        // the OS entropy pool on first use and is re-seeded
        // periodically. 96 bits from that is more than enough for
        // ICE retransmit dedupe.
        let mut buf = [0u8; 12];
        let mut rng = rand::rng();
        for b in &mut buf {
            *b = rng.random();
        }
        Self(buf)
    }
}

/// STUN message method — `Binding` is the only one the ICE codec
/// emits or accepts (TURN has its own parser in `crate::turn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StunMethod {
    /// `Binding` (RFC 8489 §12.1).
    Binding,
}

/// STUN message class — request, response, error, or indication
/// (RFC 8489 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StunClass {
    /// Request class — expects a paired response.
    Request,
    /// Successful response.
    SuccessResponse,
    /// Error response.
    ErrorResponse,
    /// Indication — fire-and-forget.
    Indication,
}

impl StunClass {
    const REQUEST_BITS: u16 = 0x0000;
    const INDICATION_BITS: u16 = 0x0010;
    const SUCCESS_RESPONSE_BITS: u16 = 0x0100;
    const ERROR_RESPONSE_BITS: u16 = 0x0110;

    fn encode(self) -> u16 {
        match self {
            Self::Request => Self::REQUEST_BITS,
            Self::Indication => Self::INDICATION_BITS,
            Self::SuccessResponse => Self::SUCCESS_RESPONSE_BITS,
            Self::ErrorResponse => Self::ERROR_RESPONSE_BITS,
        }
    }

    fn from_bits(ty: u16) -> Self {
        match ty & 0x0110 {
            Self::INDICATION_BITS => Self::Indication,
            Self::SUCCESS_RESPONSE_BITS => Self::SuccessResponse,
            Self::ERROR_RESPONSE_BITS => Self::ErrorResponse,
            _ => Self::Request,
        }
    }
}

/// `ERROR-CODE` attribute (RFC 8489 §14.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorCode {
    /// Three-digit code, e.g. `401`.
    pub code: u16,
    /// Reason phrase.
    pub reason: String,
}

/// A parsed STUN message — the Binding request/response/indication
/// grammar plus the ICE attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunMessage {
    /// Message class (request / response / indication / error).
    pub class: StunClass,
    /// Method — always [`StunMethod::Binding`].
    pub method: StunMethod,
    /// Transaction id (96 bits, correlated across request + response).
    pub transaction_id: TransactionId,
    /// `XOR-MAPPED-ADDRESS` — present on Binding success responses.
    pub xor_mapped_address: Option<SocketAddr>,
    /// `USERNAME` — `remote-ufrag:local-ufrag` on ICE checks.
    pub username: Option<String>,
    /// `PRIORITY` attribute (ICE).
    pub priority: Option<u32>,
    /// `USE-CANDIDATE` flag (ICE nomination).
    pub use_candidate: bool,
    /// `ICE-CONTROLLING` tie-breaker.
    pub ice_controlling: Option<u64>,
    /// `ICE-CONTROLLED` tie-breaker.
    pub ice_controlled: Option<u64>,
    /// `ERROR-CODE` on error responses.
    pub error_code: Option<ErrorCode>,
    /// `true` when the decoded datagram carried `MESSAGE-INTEGRITY`.
    /// Use [`verify_message_integrity`] on the raw bytes to check it.
    pub has_message_integrity: bool,
    /// `true` when the decoded datagram carried `FINGERPRINT`. Use
    /// [`verify_fingerprint`] on the raw bytes to check it.
    pub has_fingerprint: bool,
}

impl StunMessage {
    fn bare(class: StunClass, transaction_id: TransactionId) -> Self {
        Self {
            class,
            method: StunMethod::Binding,
            transaction_id,
            xor_mapped_address: None,
            username: None,
            priority: None,
            use_candidate: false,
            ice_controlling: None,
            ice_controlled: None,
            error_code: None,
            has_message_integrity: false,
            has_fingerprint: false,
        }
    }

    /// Build a Binding Request with a fresh transaction id.
    #[must_use]
    pub fn new_binding_request() -> Self {
        Self::bare(StunClass::Request, TransactionId::random())
    }

    /// Build a Binding Indication with a fresh transaction id — the
    /// ICE keepalive (RFC 8445 §11), which carries no credentials.
    #[must_use]
    pub fn new_binding_indication() -> Self {
        Self::bare(StunClass::Indication, TransactionId::random())
    }

    /// Build a Binding Success Response echoing `request`'s transaction
    /// id and carrying `observed` in `XOR-MAPPED-ADDRESS`. `observed`
    /// is the socket address the server saw the request come from.
    #[must_use]
    pub fn new_binding_response(request: &Self, observed: SocketAddr) -> Self {
        let mut resp = Self::bare(StunClass::SuccessResponse, request.transaction_id);
        resp.xor_mapped_address = Some(observed);
        resp
    }

    /// Build a Binding Error Response for `request` with `code` /
    /// `reason` in `ERROR-CODE`.
    #[must_use]
    pub fn new_binding_error(request: &Self, code: u16, reason: &str) -> Self {
        let mut resp = Self::bare(StunClass::ErrorResponse, request.transaction_id);
        resp.error_code = Some(ErrorCode {
            code,
            reason: reason.to_owned(),
        });
        resp
    }

    /// Serialize the message without integrity or fingerprint —
    /// what a plain STUN server round trip uses.
    pub fn encode(&self) -> Result<Vec<u8>, StunError> {
        self.encode_with(None, false)
    }

    /// Serialize the message, appending `MESSAGE-INTEGRITY` keyed
    /// with `integrity_key` (the peer's `ice-pwd` bytes for ICE
    /// checks) when given, and `FINGERPRINT` last when `fingerprint`
    /// is set.
    pub fn encode_with(
        &self,
        integrity_key: Option<&[u8]>,
        fingerprint: bool,
    ) -> Result<Vec<u8>, StunError> {
        let mut attrs: Vec<u8> = Vec::new();
        if let Some(addr) = self.xor_mapped_address {
            write_xor_mapped_address(&mut attrs, addr, &self.transaction_id)?;
        }
        if let Some(user) = &self.username {
            write_attr(&mut attrs, ATTR_USERNAME, user.as_bytes())?;
        }
        if let Some(err) = &self.error_code {
            // RFC 8489 §14.8: class in the low 3 bits of byte 2,
            // number (0..=99) in byte 3.
            let class = u8::try_from(err.code / 100).unwrap_or(u8::MAX) & 0x07;
            let number = u8::try_from(err.code % 100).unwrap_or(0);
            let mut value = vec![0u8, 0u8, class, number];
            value.extend_from_slice(err.reason.as_bytes());
            write_attr(&mut attrs, ATTR_ERROR_CODE, &value)?;
        }
        if let Some(prio) = self.priority {
            write_u32_attr(&mut attrs, ATTR_PRIORITY, prio)?;
        }
        if self.use_candidate {
            write_attr(&mut attrs, ATTR_USE_CANDIDATE, &[])?;
        }
        if let Some(tie) = self.ice_controlling {
            write_u64_attr(&mut attrs, ATTR_ICE_CONTROLLING, tie)?;
        }
        if let Some(tie) = self.ice_controlled {
            write_u64_attr(&mut attrs, ATTR_ICE_CONTROLLED, tie)?;
        }

        let mut out = Vec::with_capacity(HEADER_LEN + attrs.len() + 32);
        let type_bits = self.method.to_bits() | self.class.encode();
        out.extend_from_slice(&type_bits.to_be_bytes());
        out.extend_from_slice(&[0, 0]); // length, patched below
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id.0);
        out.extend_from_slice(&attrs);

        if let Some(key) = integrity_key {
            // RFC 8489 §14.5: the HMAC covers the message with the
            // length field already counting the MESSAGE-INTEGRITY
            // attribute (4 + 20 bytes).
            let covered_len = out.len() + 4 + INTEGRITY_LEN - HEADER_LEN;
            set_length(&mut out, covered_len)?;
            let mac = hmac_sha1(key, &out);
            write_attr(&mut out, ATTR_MESSAGE_INTEGRITY, &mac)?;
        }
        if fingerprint {
            // RFC 8489 §14.7: CRC-32 over everything before the
            // FINGERPRINT attribute, with the length field counting
            // it (4 + 4 bytes).
            let covered_len = out.len() + 8 - HEADER_LEN;
            set_length(&mut out, covered_len)?;
            let crc = crc32(&out) ^ FINGERPRINT_XOR;
            write_u32_attr(&mut out, ATTR_FINGERPRINT, crc)?;
        }
        let body_len = out.len() - HEADER_LEN;
        set_length(&mut out, body_len)?;
        Ok(out)
    }

    /// Parse a message off the wire. Returns [`StunError::Malformed`]
    /// for anything short of a well-formed STUN header + attribute
    /// block. Integrity and fingerprint are only *noted* here — see
    /// [`verify_message_integrity`] / [`verify_fingerprint`].
    pub fn decode(bytes: &[u8]) -> Result<Self, StunError> {
        if bytes.len() < HEADER_LEN {
            return Err(StunError::Malformed("header < 20 bytes".into()));
        }
        // High 2 bits must be zero per RFC 8489 §6.
        if bytes[0] & 0xC0 != 0 {
            return Err(StunError::Malformed(
                "non-zero high-2-bits in first byte".into(),
            ));
        }
        let type_bits = u16::from_be_bytes([bytes[0], bytes[1]]);
        let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        let cookie = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if cookie != MAGIC_COOKIE {
            return Err(StunError::Malformed("wrong magic cookie".into()));
        }
        let mut tid = [0u8; 12];
        tid.copy_from_slice(&bytes[8..20]);
        let transaction_id = TransactionId(tid);
        let method = StunMethod::from_bits(type_bits).ok_or_else(|| {
            StunError::Malformed(format!("unknown method bits: {type_bits:#06x}"))
        })?;
        let class = StunClass::from_bits(type_bits);
        let attrs_raw = bytes
            .get(HEADER_LEN..HEADER_LEN + length)
            .ok_or_else(|| StunError::Malformed("attribute block truncated".into()))?;

        let mut msg = Self::bare(class, transaction_id);
        msg.method = method;

        for attr in AttrIter::new(attrs_raw) {
            let (ty, value) = attr?;
            match ty {
                ATTR_XOR_MAPPED_ADDRESS => {
                    msg.xor_mapped_address =
                        Some(decode_xor_mapped_address(value, &transaction_id)?);
                }
                ATTR_USERNAME => {
                    msg.username = Some(
                        std::str::from_utf8(value)
                            .map_err(|_| StunError::Malformed("USERNAME is not UTF-8".into()))?
                            .to_owned(),
                    );
                }
                ATTR_ERROR_CODE if value.len() >= 4 => {
                    let code = u16::from(value[2] & 0x07) * 100 + u16::from(value[3]);
                    let reason = String::from_utf8_lossy(&value[4..]).into_owned();
                    msg.error_code = Some(ErrorCode { code, reason });
                }
                ATTR_PRIORITY if value.len() == 4 => {
                    msg.priority =
                        Some(u32::from_be_bytes([value[0], value[1], value[2], value[3]]));
                }
                ATTR_USE_CANDIDATE => msg.use_candidate = true,
                ATTR_ICE_CONTROLLING if value.len() == 8 => {
                    msg.ice_controlling =
                        Some(u64::from_be_bytes(value.try_into().unwrap_or([0; 8])));
                }
                ATTR_ICE_CONTROLLED if value.len() == 8 => {
                    msg.ice_controlled =
                        Some(u64::from_be_bytes(value.try_into().unwrap_or([0; 8])));
                }
                ATTR_MESSAGE_INTEGRITY => msg.has_message_integrity = true,
                ATTR_FINGERPRINT => msg.has_fingerprint = true,
                _ => {}
            }
        }
        Ok(msg)
    }
}

impl StunMethod {
    fn to_bits(self) -> u16 {
        match self {
            Self::Binding => METHOD_BINDING,
        }
    }

    fn from_bits(bits: u16) -> Option<Self> {
        // Method bits per RFC 8489 §5 are the low 12 with the class
        // bits interleaved; Binding's method bits come out as 0x001.
        let method_bits = (bits & 0x000F) | ((bits & 0x00E0) >> 1) | ((bits & 0x3E00) >> 2);
        (method_bits == METHOD_BINDING).then_some(Self::Binding)
    }
}

/// Errors surfaced by the STUN parser / encoder.
#[derive(Debug, Error)]
pub enum StunError {
    /// Input bytes don't decode to a STUN message.
    #[error("malformed STUN: {0}")]
    Malformed(String),
    /// Requested address family is neither IPv4 nor IPv6.
    #[error("unsupported address family")]
    UnsupportedFamily,
    /// Attribute value is too large for STUN's 16-bit length field.
    #[error("attribute too large for STUN")]
    TooLarge,
    /// Underlying transport error during a check.
    #[error("io: {0}")]
    Io(#[from] io::Error),
    /// Request timed out waiting for a response.
    #[error("stun timeout after {millis} ms")]
    Timeout {
        /// Budget that expired.
        millis: u64,
    },
}

/// Iterator over `(type, value)` attribute pairs in a STUN body.
struct AttrIter<'a> {
    body: &'a [u8],
    cursor: usize,
}

impl<'a> AttrIter<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, cursor: 0 }
    }
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<(u16, &'a [u8]), StunError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor + 4 > self.body.len() {
            return None;
        }
        let ty = u16::from_be_bytes([self.body[self.cursor], self.body[self.cursor + 1]]);
        let len = usize::from(u16::from_be_bytes([
            self.body[self.cursor + 2],
            self.body[self.cursor + 3],
        ]));
        let value_start = self.cursor + 4;
        let Some(value) = self.body.get(value_start..value_start + len) else {
            self.cursor = self.body.len();
            return Some(Err(StunError::Malformed(
                "attribute value truncated".into(),
            )));
        };
        self.cursor = value_start + len + ((4 - (len % 4)) % 4);
        Some(Ok((ty, value)))
    }
}

/// Byte offset of the first attribute of type `ty` inside `raw`
/// (pointing at its 4-byte attribute header), if present.
fn find_attr_offset(raw: &[u8], ty: u16) -> Option<usize> {
    let length = usize::from(u16::from_be_bytes([*raw.get(2)?, *raw.get(3)?]));
    let body = raw.get(HEADER_LEN..HEADER_LEN + length)?;
    let mut cursor = 0;
    while cursor + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
        let len = usize::from(u16::from_be_bytes([body[cursor + 2], body[cursor + 3]]));
        if kind == ty {
            return Some(HEADER_LEN + cursor);
        }
        cursor += 4 + len + ((4 - (len % 4)) % 4);
    }
    None
}

/// Check the `MESSAGE-INTEGRITY` of a raw datagram against `key`
/// (RFC 8489 §14.5). `false` when the attribute is absent, truncated
/// or the HMAC doesn't match.
#[must_use]
pub fn verify_message_integrity(raw: &[u8], key: &[u8]) -> bool {
    let Some(offset) = find_attr_offset(raw, ATTR_MESSAGE_INTEGRITY) else {
        return false;
    };
    let Some(observed) = raw.get(offset + 4..offset + 4 + INTEGRITY_LEN) else {
        return false;
    };
    // The HMAC input is the message up to the attribute, with the
    // length field counting the attribute itself and nothing after it.
    let mut covered = raw[..offset].to_vec();
    if set_length(&mut covered, offset + 4 + INTEGRITY_LEN - HEADER_LEN).is_err() {
        return false;
    }
    ct_eq(observed, &hmac_sha1(key, &covered))
}

/// Check the `FINGERPRINT` of a raw datagram (RFC 8489 §14.7).
/// `false` when the attribute is absent, not last, or the CRC
/// doesn't match.
#[must_use]
pub fn verify_fingerprint(raw: &[u8]) -> bool {
    let Some(offset) = find_attr_offset(raw, ATTR_FINGERPRINT) else {
        return false;
    };
    let Some(value) = raw.get(offset + 4..offset + 8) else {
        return false;
    };
    if raw.len() != offset + 8 {
        return false;
    }
    let observed = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
    let mut covered = raw[..offset].to_vec();
    if set_length(&mut covered, offset + 8 - HEADER_LEN).is_err() {
        return false;
    }
    observed == (crc32(&covered) ^ FINGERPRINT_XOR)
}

fn set_length(buf: &mut [u8], body_len: usize) -> Result<(), StunError> {
    let len = u16::try_from(body_len).map_err(|_| StunError::TooLarge)?;
    buf[2..4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; INTEGRITY_LEN] {
    // HMAC-SHA1 accepts any key length; `new_from_slice` never
    // fails for it, so an empty-key fallback only guards the type
    // signature — verification against any real peer would fail
    // loudly rather than silently pass.
    let mut mac = Hmac::<Sha1>::new_from_slice(key)
        .unwrap_or_else(|_| Hmac::<Sha1>::new_from_slice(&[]).unwrap_or_else(|_| unreachable!()));
    mac.update(msg);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; INTEGRITY_LEN];
    out.copy_from_slice(&tag);
    out
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// CRC-32 (IEEE 802.3, reflected, polynomial `0xEDB88320`) — the
/// variant `FINGERPRINT` specifies. Bitwise rather than table-driven:
/// STUN messages are a few hundred bytes at most.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn write_xor_mapped_address(
    out: &mut Vec<u8>,
    addr: SocketAddr,
    tid: &TransactionId,
) -> Result<(), StunError> {
    // Value layout per RFC 8489 §14.2:
    //   0: reserved (0)
    //   1: family (0x01 IPv4, 0x02 IPv6)
    //   2-3: X-Port (port XOR magic-cookie high 16 bits)
    //   4-: X-Address (address XORed against magic-cookie + transaction id)
    let mut value: Vec<u8> = Vec::new();
    value.push(0);
    let xor_port = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    match addr.ip() {
        IpAddr::V4(v4) => {
            value.push(0x01);
            value.extend_from_slice(&xor_port.to_be_bytes());
            for (octet, c) in v4.octets().iter().zip(cookie) {
                value.push(octet ^ c);
            }
        }
        IpAddr::V6(v6) => {
            value.push(0x02);
            value.extend_from_slice(&xor_port.to_be_bytes());
            let mask: Vec<u8> = cookie.iter().chain(tid.0.iter()).copied().collect();
            for (octet, m) in v6.octets().iter().zip(mask) {
                value.push(octet ^ m);
            }
        }
    }
    write_attr(out, ATTR_XOR_MAPPED_ADDRESS, &value)
}

fn write_u32_attr(out: &mut Vec<u8>, attr_type: u16, value: u32) -> Result<(), StunError> {
    write_attr(out, attr_type, &value.to_be_bytes())
}

fn write_u64_attr(out: &mut Vec<u8>, attr_type: u16, value: u64) -> Result<(), StunError> {
    write_attr(out, attr_type, &value.to_be_bytes())
}

fn write_attr(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) -> Result<(), StunError> {
    let len = u16::try_from(value.len()).map_err(|_| StunError::TooLarge)?;
    out.extend_from_slice(&attr_type.to_be_bytes());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    // Pad to 4-byte boundary.
    let pad = (4 - (value.len() % 4)) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
    Ok(())
}

fn decode_xor_mapped_address(value: &[u8], tid: &TransactionId) -> Result<SocketAddr, StunError> {
    if value.len() < 4 {
        return Err(StunError::Malformed("XOR-MAPPED-ADDRESS < 4 bytes".into()));
    }
    let family = value[1];
    let xor_port = u16::from_be_bytes([value[2], value[3]]);
    let port = xor_port ^ ((MAGIC_COOKIE >> 16) as u16);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    match family {
        0x01 => {
            if value.len() < 8 {
                return Err(StunError::Malformed(
                    "XOR-MAPPED-ADDRESS IPv4 truncated".into(),
                ));
            }
            let mut octets = [0u8; 4];
            for (i, o) in octets.iter_mut().enumerate() {
                *o = value[4 + i] ^ cookie[i];
            }
            Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
        }
        0x02 => {
            if value.len() < 20 {
                return Err(StunError::Malformed(
                    "XOR-MAPPED-ADDRESS IPv6 truncated".into(),
                ));
            }
            let mut octets = [0u8; 16];
            for (i, o) in octets.iter_mut().enumerate() {
                let mask = if i < 4 { cookie[i] } else { tid.0[i - 4] };
                *o = value[4 + i] ^ mask;
            }
            Ok(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(octets)), port))
        }
        other => Err(StunError::Malformed(format!(
            "unknown XOR-MAPPED address family {other:#04x}"
        ))),
    }
}

/// Send a single STUN Binding Request over `socket` to `peer` and
/// wait up to `timeout` for the paired Binding Response. Returns the
/// remote's `XOR-MAPPED-ADDRESS` — i.e., what the peer observes as
/// our socket address. On loopback this will equal our `local_addr`.
///
/// One round trip, no retransmit: this is the diagnostic / gathering
/// primitive. Connectivity checks with retransmits live in
/// [`crate::agent::IceAgent`].
pub async fn binding_ping(
    socket: &UdpSocket,
    peer: SocketAddr,
    timeout: Duration,
) -> Result<SocketAddr, StunError> {
    let request = StunMessage::new_binding_request();
    let bytes = request.encode()?;
    socket.send_to(&bytes, peer).await?;
    debug!(%peer, "STUN Binding Request sent");

    let mut buf = [0u8; 1500];
    let (n, from) = match tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await {
        Ok(res) => res?,
        Err(_) => {
            return Err(StunError::Timeout {
                millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
    };
    let response = StunMessage::decode(&buf[..n])?;
    if response.class != StunClass::SuccessResponse {
        return Err(StunError::Malformed(format!(
            "expected success response, got {:?}",
            response.class
        )));
    }
    if response.transaction_id != request.transaction_id {
        return Err(StunError::Malformed(
            "transaction id mismatch — spurious STUN reply".into(),
        ));
    }
    debug!(%from, observed = ?response.xor_mapped_address, "STUN Binding Response received");
    response
        .xor_mapped_address
        .ok_or_else(|| StunError::Malformed("Binding Response missing XOR-MAPPED-ADDRESS".into()))
}

/// Gather server-reflexive addresses for `socket` by sending one
/// Binding Request to every STUN server in `servers` and collecting
/// the responses that arrive within `timeout`. Returns the distinct
/// observed external addresses; servers that don't answer are
/// skipped with a debug log.
///
/// Requests go out together and responses are matched by transaction
/// id, so the servers are queried concurrently over the single
/// socket.
///
/// This is what [`crate::CandidateGatherer::gather_all`] runs when
/// `webrtc.ice.stun_servers` is configured; the CLI's WebRTC
/// signaling handler is the intended caller, feeding the result into
/// the answer's `a=candidate` lines as `srflx` candidates.
pub async fn gather_srflx_candidates(
    socket: &UdpSocket,
    servers: &[SocketAddr],
    timeout: Duration,
) -> Vec<SocketAddr> {
    let mut pending: std::collections::HashMap<TransactionId, SocketAddr> =
        std::collections::HashMap::with_capacity(servers.len());
    for &server in servers {
        let request = StunMessage::new_binding_request();
        match request.encode() {
            Ok(bytes) => match socket.send_to(&bytes, server).await {
                Ok(_) => {
                    pending.insert(request.transaction_id, server);
                }
                Err(e) => debug!(%server, ?e, "STUN gathering: send failed"),
            },
            Err(e) => debug!(?e, "STUN gathering: encode failed"),
        }
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 1500];
    while !pending.is_empty() {
        let (n, from) = match tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                // An ICMP unreachable for one server surfaces here on
                // some platforms; keep waiting for the others.
                debug!(?e, "STUN gathering: recv error");
                continue;
            }
            Err(_) => break,
        };
        let Ok(response) = StunMessage::decode(&buf[..n]) else {
            continue;
        };
        if response.class != StunClass::SuccessResponse
            || pending.remove(&response.transaction_id).is_none()
        {
            continue;
        }
        if let Some(addr) = response.xor_mapped_address {
            debug!(%from, %addr, "STUN gathering: observed address");
            if !out.contains(&addr) {
                out.push(addr);
            }
        } else {
            debug!(%from, "STUN gathering: response without XOR-MAPPED-ADDRESS");
        }
    }
    for server in pending.values() {
        debug!(%server, "STUN gathering: no response");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn binding_request_encodes_to_20_byte_header() {
        let req = StunMessage::new_binding_request();
        let bytes = req.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN);
        // Message type bits = 0x0001 Binding, class 0 → `0x00 0x01`.
        assert_eq!(&bytes[0..2], &[0x00, 0x01]);
        // Length 0 → `0x00 0x00`.
        assert_eq!(&bytes[2..4], &[0x00, 0x00]);
        // Magic cookie.
        assert_eq!(&bytes[4..8], &MAGIC_COOKIE.to_be_bytes());
    }

    #[test]
    fn binding_request_roundtrips() {
        let original = StunMessage::new_binding_request();
        let bytes = original.encode().unwrap();
        let parsed = StunMessage::decode(&bytes).unwrap();
        assert_eq!(parsed.class, StunClass::Request);
        assert_eq!(parsed.method, StunMethod::Binding);
        assert_eq!(parsed.transaction_id, original.transaction_id);
        assert!(parsed.xor_mapped_address.is_none());
        assert!(is_stun(&bytes));
        assert!(!is_stun(&[
            0x80, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]));
    }

    #[test]
    fn binding_response_roundtrips_with_xor_mapped_address() {
        let req = StunMessage::new_binding_request();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)), 40_000);
        let resp = StunMessage::new_binding_response(&req, addr);
        let bytes = resp.encode().unwrap();
        let parsed = StunMessage::decode(&bytes).unwrap();
        assert_eq!(parsed.class, StunClass::SuccessResponse);
        assert_eq!(parsed.transaction_id, req.transaction_id);
        assert_eq!(parsed.xor_mapped_address, Some(addr));
    }

    #[test]
    fn ipv6_xor_mapped_address_roundtrips() {
        let req = StunMessage::new_binding_request();
        let addr: SocketAddr = "[2001:db8::7]:4242".parse().unwrap();
        let resp = StunMessage::new_binding_response(&req, addr);
        let parsed = StunMessage::decode(&resp.encode().unwrap()).unwrap();
        assert_eq!(parsed.xor_mapped_address, Some(addr));
    }

    #[test]
    fn ice_attributes_roundtrip() {
        let mut req = StunMessage::new_binding_request();
        req.username = Some("abcd:efgh".into());
        req.priority = Some(1_845_501_695);
        req.use_candidate = true;
        req.ice_controlling = Some(0x0123_4567_89ab_cdef);
        let parsed = StunMessage::decode(&req.encode().unwrap()).unwrap();
        assert_eq!(parsed.username.as_deref(), Some("abcd:efgh"));
        assert_eq!(parsed.priority, Some(1_845_501_695));
        assert!(parsed.use_candidate);
        assert_eq!(parsed.ice_controlling, Some(0x0123_4567_89ab_cdef));
        assert_eq!(parsed.ice_controlled, None);
    }

    #[test]
    fn error_response_roundtrips_code_and_reason() {
        let req = StunMessage::new_binding_request();
        let err = StunMessage::new_binding_error(&req, ERR_ROLE_CONFLICT, "Role Conflict");
        let parsed = StunMessage::decode(&err.encode().unwrap()).unwrap();
        assert_eq!(parsed.class, StunClass::ErrorResponse);
        assert_eq!(
            parsed.error_code,
            Some(ErrorCode {
                code: 487,
                reason: "Role Conflict".into()
            })
        );
    }

    #[test]
    fn crc32_matches_known_vector() {
        // "123456789" → 0xCBF43926 (the classic CRC-32 check value).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn integrity_and_fingerprint_verify_and_reject_tampering() {
        let mut req = StunMessage::new_binding_request();
        req.username = Some("peer:me".into());
        req.priority = Some(7);
        let key = b"the-peers-ice-pwd-22chars";
        let bytes = req.encode_with(Some(key), true).unwrap();
        let parsed = StunMessage::decode(&bytes).unwrap();
        assert!(parsed.has_message_integrity);
        assert!(parsed.has_fingerprint);
        assert!(verify_fingerprint(&bytes));
        assert!(verify_message_integrity(&bytes, key));
        assert!(!verify_message_integrity(&bytes, b"wrong-password"));

        // Flip a byte inside PRIORITY: both checks fail.
        let mut tampered = bytes.clone();
        let prio_off = find_attr_offset(&bytes, ATTR_PRIORITY).unwrap();
        tampered[prio_off + 7] ^= 0x01;
        assert!(!verify_fingerprint(&tampered));
        assert!(!verify_message_integrity(&tampered, key));

        // Without integrity the check reports absence, not a match.
        let plain = req.encode_with(None, true).unwrap();
        assert!(!verify_message_integrity(&plain, key));
        assert!(verify_fingerprint(&plain));
        let bare = req.encode().unwrap();
        assert!(!verify_fingerprint(&bare));
    }

    #[test]
    fn decode_rejects_wrong_magic_cookie() {
        let mut bytes = StunMessage::new_binding_request().encode().unwrap();
        bytes[4] = 0; // trash first byte of magic cookie
        bytes[5] = 0;
        bytes[6] = 0;
        bytes[7] = 0;
        match StunMessage::decode(&bytes) {
            Err(StunError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_truncated_header() {
        let short = [0u8; 10];
        assert!(matches!(
            StunMessage::decode(&short),
            Err(StunError::Malformed(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_attribute() {
        let mut req = StunMessage::new_binding_request();
        req.username = Some("abcdefgh".into());
        let mut bytes = req.encode().unwrap();
        // Claim a longer USERNAME than the body carries.
        bytes[HEADER_LEN + 3] = 0x40;
        assert!(matches!(
            StunMessage::decode(&bytes),
            Err(StunError::Malformed(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_ping_loopback_returns_source_address() {
        // Spin up a mini STUN server: reads a Binding Request, echoes
        // XOR-MAPPED-ADDRESS of the caller. Covers the full roundtrip:
        // encode → send → parse → encode → send → parse.
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            let req = StunMessage::decode(&buf[..n]).unwrap();
            let resp = StunMessage::new_binding_response(&req, from);
            let bytes = resp.encode().unwrap();
            server.send_to(&bytes, from).await.unwrap();
        });

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let observed = binding_ping(&client, server_addr, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(observed, client_addr);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn binding_ping_times_out_against_silent_peer() {
        // No server bound on this loopback port — the request should
        // sit forever, and the helper must surface Timeout.
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Pick a port we're not listening on.
        let silent = "127.0.0.1:1".parse().unwrap();
        match binding_ping(&client, silent, Duration::from_millis(150)).await {
            // Some platforms surface ICMP unreachable as an io error
            // on the recv_from side — treat Io as timeout-equivalent.
            Err(StunError::Timeout { .. } | StunError::Io(_)) => {}
            other => panic!("expected Timeout / Io, got {other:?}"),
        }
    }
}
