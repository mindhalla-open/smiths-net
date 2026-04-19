//! Shared SIP stream-framing logic for TCP and TLS transports.
//!
//! Both stream transports frame messages the same way (RFC 3261 §7.5):
//! headers are terminated by a double CRLF, body length is whatever
//! `Content-Length` claims. UDP has its own 1-datagram-per-message
//! framing and does not use this module.

use bytes::{Bytes, BytesMut};

/// Cap on a single SIP message — guardrail against runaway framing.
/// Matches the UDP ceiling so cross-transport behaviour stays
/// symmetric.
pub const MAX_MESSAGE_BYTES: usize = 65_535;

/// Result of trying to peel one SIP message off the accumulated buffer.
pub enum FrameOutcome {
    /// One full message extracted; `buf` now starts at the next one.
    Complete(Bytes),
    /// Not enough bytes yet — wait for more.
    Partial,
    /// Buffer growing past the per-message cap without a complete frame.
    Overflow,
    /// `Content-Length` header present but unparseable / absurd.
    BadLength,
}

/// Try to extract exactly one SIP message from the front of `buf`.
pub fn take_one_message(buf: &mut BytesMut) -> FrameOutcome {
    if buf.len() > MAX_MESSAGE_BYTES {
        return FrameOutcome::Overflow;
    }
    let Some(header_end) = find_double_crlf(buf) else {
        return FrameOutcome::Partial;
    };
    let headers = &buf[..header_end];
    let body_start = header_end + 4;

    let Ok(body_len) = content_length(headers) else {
        return FrameOutcome::BadLength;
    };
    let total = body_start + body_len;
    if total > MAX_MESSAGE_BYTES {
        return FrameOutcome::Overflow;
    }
    if buf.len() < total {
        return FrameOutcome::Partial;
    }
    let msg = buf.split_to(total).freeze();
    FrameOutcome::Complete(msg)
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse `Content-Length:` from the header block. Missing header → 0.
/// Malformed value → `Err`.
fn content_length(headers: &[u8]) -> Result<usize, ()> {
    let text = std::str::from_utf8(headers).map_err(|_| ())?;
    for line in text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("Content-Length") || name.eq_ignore_ascii_case("l") {
            return value.trim().parse::<usize>().map_err(|_| ());
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_buf(data: &[u8]) -> BytesMut {
        let mut b = BytesMut::new();
        b.extend_from_slice(data);
        b
    }

    #[test]
    fn frames_single_options_request() {
        let raw = b"OPTIONS sip:a@b SIP/2.0\r\nContent-Length: 0\r\n\r\n";
        let mut buf = mk_buf(raw);
        match take_one_message(&mut buf) {
            FrameOutcome::Complete(bytes) => assert_eq!(&bytes[..], raw),
            _ => panic!("expected Complete"),
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn frames_request_with_sdp_body() {
        let body = "v=0\r\no=x 1 1 IN IP4 127.0.0.1\r\ns=-\r\n";
        let raw = format!(
            "INVITE sip:a@b SIP/2.0\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let mut buf = mk_buf(raw.as_bytes());
        match take_one_message(&mut buf) {
            FrameOutcome::Complete(bytes) => assert_eq!(&bytes[..], raw.as_bytes()),
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn partial_headers_returns_partial() {
        let raw = b"OPTIONS sip:a@b SIP/2.0\r\nContent-Length: 0\r\n";
        let mut buf = mk_buf(raw);
        assert!(matches!(take_one_message(&mut buf), FrameOutcome::Partial));
    }

    #[test]
    fn partial_body_returns_partial() {
        let raw = b"INVITE sip:a@b SIP/2.0\r\nContent-Length: 20\r\n\r\nshort";
        let mut buf = mk_buf(raw);
        assert!(matches!(take_one_message(&mut buf), FrameOutcome::Partial));
    }

    #[test]
    fn missing_content_length_treated_as_zero() {
        let raw = b"OPTIONS sip:a@b SIP/2.0\r\nVia: SIP/2.0/TCP a;branch=z\r\n\r\n";
        let mut buf = mk_buf(raw);
        match take_one_message(&mut buf) {
            FrameOutcome::Complete(bytes) => assert_eq!(&bytes[..], raw),
            _ => panic!("expected Complete"),
        }
    }

    #[test]
    fn bad_content_length_rejected() {
        let raw = b"OPTIONS sip:a@b SIP/2.0\r\nContent-Length: NaN\r\n\r\n";
        let mut buf = mk_buf(raw);
        assert!(matches!(
            take_one_message(&mut buf),
            FrameOutcome::BadLength
        ));
    }

    #[test]
    fn back_to_back_messages_all_extracted() {
        let raw1 = b"OPTIONS sip:a@b SIP/2.0\r\nContent-Length: 0\r\n\r\n";
        let raw2 = b"OPTIONS sip:c@d SIP/2.0\r\nContent-Length: 0\r\n\r\n";
        let mut buf = mk_buf(raw1);
        buf.extend_from_slice(raw2);
        let FrameOutcome::Complete(m1) = take_one_message(&mut buf) else {
            panic!()
        };
        let FrameOutcome::Complete(m2) = take_one_message(&mut buf) else {
            panic!()
        };
        assert_eq!(&m1[..], raw1);
        assert_eq!(&m2[..], raw2);
        assert!(buf.is_empty());
    }

    #[test]
    fn compact_content_length_header_name_respected() {
        let raw = b"INVITE sip:a@b SIP/2.0\r\nl: 3\r\n\r\nabc";
        let mut buf = mk_buf(raw);
        match take_one_message(&mut buf) {
            FrameOutcome::Complete(bytes) => assert_eq!(&bytes[..], raw),
            _ => panic!("expected Complete"),
        }
    }
}
