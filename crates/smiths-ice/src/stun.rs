//! STUN Binding encoder + parser, RFC 8489.
//!
//! Single-purpose: carry Binding Requests and Responses over UDP. The
//! only attribute we inspect is `XOR-MAPPED-ADDRESS` (RFC 8489 §14.2).
//! Other attributes — `USERNAME`, `MESSAGE-INTEGRITY`, `FINGERPRINT`,
//! `PRIORITY`, `USE-CANDIDATE` — are out of scope for MVP host-only
//! candidates; RFC 8445 authentication is a follow-on slice.
//!
//! Hand-rolled because the third-party STUN crates all pull in
//! heavier ICE stacks; for a 20-byte header + one attribute we can
//! afford the ~100 LOC.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rand::RngExt as _;
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
/// Attribute type for `XOR-MAPPED-ADDRESS` (RFC 8489 §14.2).
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Transaction id — 96 bits of random per RFC 8489 §6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionId(pub [u8; 12]);

impl TransactionId {
    /// Fresh random transaction id.
    #[must_use]
    pub fn random() -> Self {
        // `rand::rng()` is the thread-local CSPRNG (rand 0.10) — it
        // pulls from the OS entropy pool on first use and is
        // re-seeded periodically. 96 bits of transaction id from
        // that is more than enough for ICE retransmit dedupe.
        let mut buf = [0u8; 12];
        let mut rng = rand::rng();
        for b in &mut buf {
            *b = rng.random();
        }
        Self(buf)
    }
}

/// STUN message method — extended as ICE grows, today `Binding` is
/// the only one we emit or accept.
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

/// A parsed STUN message — just enough of the grammar for Binding
/// request/response flows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunMessage {
    /// Message class (request / response / indication / error).
    pub class: StunClass,
    /// Method — today always [`StunMethod::Binding`].
    pub method: StunMethod,
    /// Transaction id (96 bits, correlated across request + response).
    pub transaction_id: TransactionId,
    /// Optional `XOR-MAPPED-ADDRESS` attribute — present on Binding
    /// responses, absent on requests (MVP doesn't use `MAPPED-ADDRESS`
    /// or server-reflexive attributes).
    pub xor_mapped_address: Option<SocketAddr>,
}

impl StunMessage {
    /// Build a Binding Request with a fresh transaction id.
    #[must_use]
    pub fn new_binding_request() -> Self {
        Self {
            class: StunClass::Request,
            method: StunMethod::Binding,
            transaction_id: TransactionId::random(),
            xor_mapped_address: None,
        }
    }

    /// Build a Binding Success Response echoing `request`'s transaction
    /// id and carrying `observed` in `XOR-MAPPED-ADDRESS`. `observed`
    /// is the socket address the server saw the request come from —
    /// on loopback this is just the request's source addr.
    #[must_use]
    pub fn new_binding_response(request: &Self, observed: SocketAddr) -> Self {
        Self {
            class: StunClass::SuccessResponse,
            method: StunMethod::Binding,
            transaction_id: request.transaction_id,
            xor_mapped_address: Some(observed),
        }
    }

    /// Serialize the message into a `Vec<u8>` ready for `send_to`.
    pub fn encode(&self) -> Result<Vec<u8>, StunError> {
        let mut attrs: Vec<u8> = Vec::new();
        if let Some(addr) = self.xor_mapped_address {
            write_xor_mapped_address(&mut attrs, addr, &self.transaction_id)?;
        }
        // STUN attributes are 32-bit padded, so the outer length
        // field is always a multiple of 4. `write_xor_mapped_address`
        // already pads.
        let message_length = u16::try_from(attrs.len()).map_err(|_| StunError::TooLarge)?;

        let mut out = Vec::with_capacity(HEADER_LEN + attrs.len());
        let type_bits = self.method.to_bits() | self.class.encode();
        out.extend_from_slice(&type_bits.to_be_bytes());
        out.extend_from_slice(&message_length.to_be_bytes());
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&self.transaction_id.0);
        out.extend_from_slice(&attrs);
        Ok(out)
    }

    /// Parse a message off the wire. Returns [`StunError::Malformed`]
    /// for anything short of a well-formed STUN header + attribute
    /// block.
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
        let length = u16::from_be_bytes([bytes[2], bytes[3]]) as usize;
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
        let attrs = bytes
            .get(HEADER_LEN..HEADER_LEN + length)
            .ok_or_else(|| StunError::Malformed("attribute block truncated".into()))?;

        let xor_mapped_address = parse_xor_mapped_address(attrs, &transaction_id)?;
        Ok(Self {
            class,
            method,
            transaction_id,
            xor_mapped_address,
        })
    }
}

impl StunMethod {
    fn to_bits(self) -> u16 {
        match self {
            Self::Binding => METHOD_BINDING,
        }
    }

    fn from_bits(bits: u16) -> Option<Self> {
        // Method bits per RFC 8489 §6 are the low 12 with the class
        // bits interleaved; for our MVP Binding is the only valid
        // method, and its method bits come out as 0x001.
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
    match addr.ip() {
        IpAddr::V4(v4) => {
            value.push(0x01);
            let xor_port = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
            value.extend_from_slice(&xor_port.to_be_bytes());
            let octets = v4.octets();
            let cookie = MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 {
                value.push(octets[i] ^ cookie[i]);
            }
        }
        IpAddr::V6(v6) => {
            value.push(0x02);
            let xor_port = addr.port() ^ ((MAGIC_COOKIE >> 16) as u16);
            value.extend_from_slice(&xor_port.to_be_bytes());
            let octets = v6.octets();
            let cookie = MAGIC_COOKIE.to_be_bytes();
            for i in 0..4 {
                value.push(octets[i] ^ cookie[i]);
            }
            for i in 0..12 {
                value.push(octets[i + 4] ^ tid.0[i]);
            }
        }
    }
    write_attr(out, ATTR_XOR_MAPPED_ADDRESS, &value)
}

fn write_attr(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) -> Result<(), StunError> {
    let len = u16::try_from(value.len()).map_err(|_| StunError::TooLarge)?;
    out.extend_from_slice(&attr_type.to_be_bytes());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    // Pad to 4-byte boundary.
    let pad = (4 - (value.len() % 4)) % 4;
    for _ in 0..pad {
        out.push(0);
    }
    Ok(())
}

fn parse_xor_mapped_address(
    attrs: &[u8],
    tid: &TransactionId,
) -> Result<Option<SocketAddr>, StunError> {
    let mut cursor = 0;
    while cursor + 4 <= attrs.len() {
        let ty = u16::from_be_bytes([attrs[cursor], attrs[cursor + 1]]);
        let len = u16::from_be_bytes([attrs[cursor + 2], attrs[cursor + 3]]) as usize;
        let value_start = cursor + 4;
        let value_end = value_start + len;
        let value = attrs
            .get(value_start..value_end)
            .ok_or_else(|| StunError::Malformed("attribute value truncated".into()))?;
        if ty == ATTR_XOR_MAPPED_ADDRESS {
            return Ok(Some(decode_xor_mapped_address(value, tid)?));
        }
        // Skip to next attribute with padding.
        let pad = (4 - (len % 4)) % 4;
        cursor = value_end + pad;
    }
    Ok(None)
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
            for i in 0..4 {
                octets[i] = value[4 + i] ^ cookie[i];
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
            for i in 0..4 {
                octets[i] = value[4 + i] ^ cookie[i];
            }
            for i in 0..12 {
                octets[i + 4] = value[8 + i] ^ tid.0[i];
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
/// our socket address. On loopback this will equal our `local_addr()`.
///
/// Strictly one retry — ICE proper (RFC 8445 §14) does T1-doubling
/// retransmits; the MVP keeps it to one round trip so the helper
/// stays diagnostic.
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
