//! ACK construction for non-2xx final responses to INVITE.
//!
//! Per RFC 3261 §17.1.1.3, the client INVITE transaction layer (not
//! the TU) generates ACK for final responses in the 300–699 range.
//! The ACK is special: it reuses the INVITE's `Via` branch, its
//! `From` / `Call-ID` / `Request-URI`, and pulls `To` (with the
//! server's tag) from the response. It's **not** a new transaction
//! — it's terminal to the same INVITE transaction.
//!
//! We don't use `rsip` to build this — byte-level reuse keeps the
//! ACK exactly aligned with what the peer expects (same Via branch,
//! same casing, same header order if practical). The response is
//! only parsed far enough to extract the `To` header.

use bytes::Bytes;

/// Build an ACK request for a non-2xx final response to an INVITE.
///
/// `invite_bytes` is the original request that was sent. `response_bytes`
/// is the final response (300–699 status). The returned payload is a
/// complete SIP ACK ready for `transport.send`.
///
/// Failure modes (`None` return): the INVITE or response didn't parse
/// enough for us to extract the required headers. Real-world INVITEs
/// always have the fields below — a `None` signals either a bug in the
/// caller or a badly-formatted response peer, both of which the
/// transaction layer handles by logging and continuing.
#[must_use]
pub fn build_non_ok_ack(invite_bytes: &[u8], response_bytes: &[u8]) -> Option<Bytes> {
    let invite = std::str::from_utf8(invite_bytes).ok()?;
    let response = std::str::from_utf8(response_bytes).ok()?;

    let (invite_headers, _) = invite.split_once("\r\n\r\n").unwrap_or((invite, ""));
    let (response_headers, _) = response.split_once("\r\n\r\n").unwrap_or((response, ""));

    // Parse INVITE's Request-Line — we need the Request-URI for ACK.
    let mut invite_lines = invite_headers.split("\r\n");
    let request_line = invite_lines.next()?;
    let request_uri = extract_request_uri(request_line)?;

    // Pull what we need from the INVITE headers.
    let mut invite_via: Option<&str> = None;
    let mut invite_from: Option<&str> = None;
    let mut invite_call_id: Option<&str> = None;
    let mut invite_cseq_num: Option<&str> = None;
    let mut invite_max_forwards: Option<&str> = None;
    let mut route_headers: Vec<&str> = Vec::new();

    for line in invite_lines {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if invite_via.is_none() && (lower.starts_with("via:") || lower.starts_with("v:")) {
            invite_via = Some(line);
        } else if invite_from.is_none() && (lower.starts_with("from:") || lower.starts_with("f:")) {
            invite_from = Some(line);
        } else if invite_call_id.is_none()
            && (lower.starts_with("call-id:") || lower.starts_with("i:"))
        {
            invite_call_id = Some(line);
        } else if invite_cseq_num.is_none() && lower.starts_with("cseq:") {
            invite_cseq_num = parse_cseq_num(line);
        } else if invite_max_forwards.is_none() && lower.starts_with("max-forwards:") {
            invite_max_forwards = Some(line);
        } else if lower.starts_with("route:") {
            route_headers.push(line);
        }
    }

    // The response's To carries the server's tag — ACK must use it
    // verbatim.
    let response_to = response_headers.split("\r\n").find(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("to:") || lower.starts_with("t:")
    })?;

    let via = invite_via?;
    let from = invite_from?;
    let call_id = invite_call_id?;
    let cseq_num = invite_cseq_num?;
    let mut ack = String::with_capacity(512);
    ack.push_str("ACK ");
    ack.push_str(request_uri);
    ack.push_str(" SIP/2.0\r\n");
    ack.push_str(via);
    ack.push_str("\r\n");
    // Route headers preserved as they were — §17.1.1.3 allows this.
    for rh in &route_headers {
        ack.push_str(rh);
        ack.push_str("\r\n");
    }
    ack.push_str(from);
    ack.push_str("\r\n");
    ack.push_str(response_to);
    ack.push_str("\r\n");
    ack.push_str(call_id);
    ack.push_str("\r\n");
    ack.push_str("CSeq: ");
    ack.push_str(cseq_num);
    ack.push_str(" ACK\r\n");
    if let Some(mf) = invite_max_forwards {
        ack.push_str(mf);
        ack.push_str("\r\n");
    } else {
        ack.push_str("Max-Forwards: 70\r\n");
    }
    ack.push_str("Content-Length: 0\r\n\r\n");
    Some(Bytes::from(ack))
}

fn extract_request_uri(request_line: &str) -> Option<&str> {
    // "METHOD <uri> SIP/2.0"
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    let uri = parts.next()?;
    Some(uri)
}

fn parse_cseq_num(line: &str) -> Option<&str> {
    // "CSeq: <num> <METHOD>"
    let after_colon = line.split_once(':').map(|(_, v)| v.trim())?;
    let mut toks = after_colon.split_whitespace();
    toks.next() // number
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &[u8] = concat!(
        "INVITE sip:alice@127.0.0.1:5060 SIP/2.0\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-inv-1\r\n",
        "From: <sip:bob@10.0.0.1>;tag=bob-tag\r\n",
        "To: <sip:alice@127.0.0.1>\r\n",
        "Call-ID: cid-ack-1@10.0.0.1\r\n",
        "CSeq: 1 INVITE\r\n",
        "Max-Forwards: 70\r\n",
        "Contact: <sip:bob@10.0.0.1>\r\n",
        "Content-Type: application/sdp\r\n",
        "Content-Length: 0\r\n\r\n",
    )
    .as_bytes();

    const RESPONSE_404: &[u8] = concat!(
        "SIP/2.0 404 Not Found\r\n",
        "Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-inv-1;received=10.0.0.1\r\n",
        "From: <sip:bob@10.0.0.1>;tag=bob-tag\r\n",
        "To: <sip:alice@127.0.0.1>;tag=srv-tag-404\r\n",
        "Call-ID: cid-ack-1@10.0.0.1\r\n",
        "CSeq: 1 INVITE\r\n",
        "Content-Length: 0\r\n\r\n",
    )
    .as_bytes();

    #[test]
    fn builds_ack_with_correct_topline_and_headers() {
        let ack = build_non_ok_ack(INVITE, RESPONSE_404).expect("build_non_ok_ack");
        let text = std::str::from_utf8(&ack).unwrap();
        // Request line
        assert!(text.starts_with("ACK sip:alice@127.0.0.1:5060 SIP/2.0\r\n"));
        // Same Via branch as INVITE (§17.1.1.3)
        assert!(text.contains("Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-inv-1\r\n"));
        // From preserved
        assert!(text.contains("From: <sip:bob@10.0.0.1>;tag=bob-tag\r\n"));
        // To taken from response (with server tag)
        assert!(text.contains("To: <sip:alice@127.0.0.1>;tag=srv-tag-404\r\n"));
        // Call-ID preserved
        assert!(text.contains("Call-ID: cid-ack-1@10.0.0.1\r\n"));
        // CSeq number preserved, method flipped to ACK
        assert!(text.contains("CSeq: 1 ACK\r\n"));
        // Content-Length: 0 + empty body
        assert!(text.contains("Content-Length: 0\r\n\r\n"));
        // Should NOT have Contact (ACK for non-2xx has no Contact)
        assert!(!text.contains("Contact:"));
    }

    #[test]
    fn returns_none_on_unparseable_invite() {
        assert!(build_non_ok_ack(b"garbage", RESPONSE_404).is_none());
    }

    #[test]
    fn returns_none_when_response_has_no_to_header() {
        let bad = b"SIP/2.0 404 Not Found\r\n\r\n";
        assert!(build_non_ok_ack(INVITE, bad).is_none());
    }

    #[test]
    fn preserves_route_headers() {
        let invite_with_route = concat!(
            "INVITE sip:alice@127.0.0.1 SIP/2.0\r\n",
            "Via: SIP/2.0/UDP 10.0.0.1;branch=z9hG4bK-r\r\n",
            "Route: <sip:proxy1.example.com;lr>\r\n",
            "Route: <sip:proxy2.example.com;lr>\r\n",
            "From: <sip:b@10.0.0.1>;tag=t\r\n",
            "To: <sip:a@127.0.0.1>\r\n",
            "Call-ID: cid-r\r\n",
            "CSeq: 2 INVITE\r\n",
            "Max-Forwards: 70\r\n",
            "Content-Length: 0\r\n\r\n",
        )
        .as_bytes();
        let ack = build_non_ok_ack(invite_with_route, RESPONSE_404).unwrap();
        let text = std::str::from_utf8(&ack).unwrap();
        assert!(text.contains("Route: <sip:proxy1.example.com;lr>"));
        assert!(text.contains("Route: <sip:proxy2.example.com;lr>"));
    }
}
