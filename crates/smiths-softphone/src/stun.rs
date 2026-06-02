//! Minimal STUN binding client (RFC 5389) for NAT traversal.
//!
//! The softphone sends a Binding Request **from its RTP socket** to a
//! STUN server and reads back the server-reflexive (public) address the
//! server saw. Because the request and the subsequent RTP leave the
//! same socket, a cone NAT maps them to the same public `ip:port` — so
//! advertising that address in the SDP offer lets the engine's return
//! RTP reach us through the NAT, with no engine-side changes.
//!
//! This is STUN-assisted address discovery, not full ICE: there are no
//! candidate pairs or connectivity checks. It covers the common case
//! (client behind a cone NAT, engine reachable). Symmetric NATs still
//! need a relay (TURN) — out of scope here.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::UdpSocket;
use tokio::time::timeout;

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Discover this socket's public address via `server`. Sends one
/// Binding Request and waits up to 3 s for the response.
pub(crate) async fn discover(sock: &UdpSocket, server: SocketAddr) -> Result<SocketAddr> {
    // 96-bit transaction id. Not security-critical here, but it must
    // echo back in the response so we don't accept a stray datagram.
    let txid: [u8; 12] = std::array::from_fn(|_| rand::random::<u8>());
    let req = encode_request(&txid);
    sock.send_to(&req, server).await.context("STUN send_to")?;

    let mut buf = [0u8; 512];
    let n = timeout(Duration::from_secs(3), sock.recv(&mut buf))
        .await
        .context("STUN response timed out")?
        .context("STUN recv")?;
    parse_response(&buf[..n], &txid)
}

/// Build a 20-byte Binding Request with no attributes.
fn encode_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut out = [0u8; 20];
    out[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    out[2..4].copy_from_slice(&0u16.to_be_bytes()); // message length
    out[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    out[8..20].copy_from_slice(txid);
    out
}

/// Parse a Binding Success Response and extract the mapped address.
/// Prefers `XOR-MAPPED-ADDRESS`, falls back to legacy `MAPPED-ADDRESS`.
fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Result<SocketAddr> {
    if buf.len() < 20 {
        bail!("STUN response too short ({} bytes)", buf.len());
    }
    let msg_type = u16::from_be_bytes([buf[0], buf[1]]);
    if msg_type != BINDING_SUCCESS {
        bail!("STUN response type 0x{msg_type:04x} is not a success response");
    }
    if buf[8..20] != txid[..] {
        bail!("STUN transaction id mismatch");
    }
    let attrs_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = 20usize
        .checked_add(attrs_len)
        .unwrap_or(buf.len())
        .min(buf.len());

    let mut fallback: Option<SocketAddr> = None;
    let mut pos = 20;
    while pos + 4 <= end {
        let atype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let alen = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]) as usize;
        let vstart = pos + 4;
        let vend = vstart + alen;
        if vend > end {
            break;
        }
        let value = &buf[vstart..vend];
        match atype {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = decode_addr(value, true) {
                    return Ok(addr);
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if let Some(addr) = decode_addr(value, false) {
                    fallback = Some(addr);
                }
            }
            _ => {}
        }
        // Attributes are padded to a 4-byte boundary.
        pos = vend + ((4 - (alen % 4)) % 4);
    }
    fallback.ok_or_else(|| anyhow!("STUN response had no MAPPED-ADDRESS attribute"))
}

/// Decode a `(XOR-)MAPPED-ADDRESS` attribute value (IPv4 only). When
/// `xor` is set, the port and address are XOR'd with the magic cookie.
fn decode_addr(value: &[u8], xor: bool) -> Option<SocketAddr> {
    // [0]=reserved, [1]=family (0x01 = IPv4), [2..4]=port, [4..8]=addr
    if value.len() < 8 || value[1] != 0x01 {
        return None;
    }
    let mut port = u16::from_be_bytes([value[2], value[3]]);
    let mut octets = [value[4], value[5], value[6], value[7]];
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
        let cookie = MAGIC_COOKIE.to_be_bytes();
        for (o, c) in octets.iter_mut().zip(cookie.iter()) {
            *o ^= *c;
        }
    }
    Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_well_formed() {
        let txid = [1u8; 12];
        let req = encode_request(&txid);
        assert_eq!(u16::from_be_bytes([req[0], req[1]]), BINDING_REQUEST);
        assert_eq!(
            u32::from_be_bytes([req[4], req[5], req[6], req[7]]),
            MAGIC_COOKIE
        );
        assert_eq!(&req[8..20], &txid);
    }

    #[test]
    fn parses_xor_mapped_address() {
        // Build a success response advertising 203.0.113.7:50000.
        let txid = [9u8; 12];
        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 50000);
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&12u16.to_be_bytes()); // one 8-byte attr + 4-byte header
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&txid);
        // XOR-MAPPED-ADDRESS attribute
        msg.extend_from_slice(&ATTR_XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&8u16.to_be_bytes());
        msg.push(0x00);
        msg.push(0x01); // IPv4
        let xport = public.port() ^ (MAGIC_COOKIE >> 16) as u16;
        msg.extend_from_slice(&xport.to_be_bytes());
        let cookie = MAGIC_COOKIE.to_be_bytes();
        for (o, c) in [203u8, 0, 113, 7].iter().zip(cookie.iter()) {
            msg.push(o ^ c);
        }

        let got = parse_response(&msg, &txid).expect("parse");
        assert_eq!(got, public);
    }

    #[test]
    fn rejects_txid_mismatch() {
        let txid = [9u8; 12];
        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        msg.extend_from_slice(&0u16.to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(&[7u8; 12]); // wrong txid
        assert!(parse_response(&msg, &txid).is_err());
    }
}
