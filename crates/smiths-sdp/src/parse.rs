//! Line-oriented SDP parser.
//!
//! Accepts `\r\n` and bare `\n` line endings. Ignores blank lines and
//! `a=` attributes we don't model explicitly.

use std::net::IpAddr;
use std::str::FromStr;

use crate::error::ParseError;
use crate::srtp_attr::SdesCrypto;
use crate::types::{
    ConnectionInfo, Direction, DtlsSetup, Fingerprint, IceCandidate, IcePassword, MediaDescription,
    MediaKind, Origin, RtpMap, SessionDescription,
};

impl SessionDescription {
    /// Parse an SDP document.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut origin: Option<Origin> = None;
        let mut session_name: Option<String> = None;
        let mut session_connection: Option<ConnectionInfo> = None;
        let mut media: Vec<MediaDescription> = Vec::new();
        let mut cur: Option<MediaDescription> = None;
        let mut version_seen = false;

        for (idx0, raw) in input.split('\n').enumerate() {
            let line = raw.trim_end_matches('\r').trim_end_matches('\n');
            if line.is_empty() {
                continue;
            }
            let line_no = idx0 + 1;
            let (kind, value) = line.split_once('=').ok_or_else(|| ParseError::Malformed {
                line: line_no,
                reason: format!("missing '=' in `{line}`"),
            })?;

            match kind {
                "v" => {
                    if value != "0" {
                        return Err(ParseError::Unsupported(format!("SDP version {value}")));
                    }
                    version_seen = true;
                }
                "o" => origin = Some(parse_origin(value, line_no)?),
                "s" => session_name = Some(value.to_owned()),
                "c" => {
                    let info = parse_connection(value, line_no)?;
                    if let Some(m) = cur.as_mut() {
                        m.connection = Some(info);
                    } else {
                        session_connection = Some(info);
                    }
                }
                // `t=` is required by SDP but we render a fixed `t=0 0`
                // on emit, so parsing it is redundant.
                "m" => {
                    if let Some(finished) = cur.take() {
                        media.push(finished);
                    }
                    cur = Some(parse_media(value, line_no)?);
                }
                "a" => {
                    apply_attribute(value, &mut cur, line_no)?;
                }
                _ => { /* ignored: b=, k=, r=, z=, i=, u=, e=, p= */ }
            }
        }
        if let Some(finished) = cur.take() {
            media.push(finished);
        }

        if !version_seen {
            return Err(ParseError::MissingSessionLine("v"));
        }
        let origin = origin.ok_or(ParseError::MissingSessionLine("o"))?;
        let session_name = session_name.ok_or(ParseError::MissingSessionLine("s"))?;

        Ok(Self {
            origin,
            session_name,
            connection: session_connection,
            media,
        })
    }
}

/// `o=<user> <sid> <ver> <net> <addr-type> <addr>`
fn parse_origin(value: &str, line: usize) -> Result<Origin, ParseError> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 6 {
        return Err(ParseError::Malformed {
            line,
            reason: "o= expects 6 tokens".into(),
        });
    }
    let session_id = parts[1].parse::<u64>().map_err(|e| ParseError::Malformed {
        line,
        reason: format!("o= session-id: {e}"),
    })?;
    let session_version = parts[2].parse::<u64>().map_err(|e| ParseError::Malformed {
        line,
        reason: format!("o= session-version: {e}"),
    })?;
    let address = IpAddr::from_str(parts[5]).map_err(|e| ParseError::Malformed {
        line,
        reason: format!("o= address: {e}"),
    })?;
    Ok(Origin {
        username: parts[0].to_owned(),
        session_id,
        session_version,
        address,
    })
}

/// `c=<net> <addr-type> <addr>`
fn parse_connection(value: &str, line: usize) -> Result<ConnectionInfo, ParseError> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(ParseError::Malformed {
            line,
            reason: "c= expects 3 tokens".into(),
        });
    }
    let address = IpAddr::from_str(parts[2]).map_err(|e| ParseError::Malformed {
        line,
        reason: format!("c= address: {e}"),
    })?;
    Ok(ConnectionInfo { address })
}

/// `m=<kind> <port>[/N] <proto> <fmt> [<fmt> ...]`
fn parse_media(value: &str, line: usize) -> Result<MediaDescription, ParseError> {
    let mut parts = value.split_whitespace();
    let kind = parts.next().ok_or_else(|| ParseError::Malformed {
        line,
        reason: "m= missing media token".into(),
    })?;
    let port_token = parts.next().ok_or_else(|| ParseError::Malformed {
        line,
        reason: "m= missing port".into(),
    })?;
    // Ignore `/<count>` form; take only the leading port number.
    let port_str = port_token.split('/').next().unwrap_or(port_token);
    let port = port_str.parse::<u16>().map_err(|e| ParseError::Malformed {
        line,
        reason: format!("m= port: {e}"),
    })?;
    let protocol = parts
        .next()
        .ok_or_else(|| ParseError::Malformed {
            line,
            reason: "m= missing protocol".into(),
        })?
        .to_owned();

    let mut formats: Vec<u8> = Vec::new();
    for f in parts {
        formats.push(f.parse::<u8>().map_err(|e| ParseError::Malformed {
            line,
            reason: format!("m= format `{f}`: {e}"),
        })?);
    }

    Ok(MediaDescription {
        kind: MediaKind::parse(kind),
        port,
        protocol,
        formats,
        rtpmap: Vec::new(),
        crypto: Vec::new(),
        direction: Direction::default(),
        connection: None,
        fingerprint: None,
        setup: None,
        ice_ufrag: None,
        ice_pwd: None,
        ice_options: Vec::new(),
        candidates: Vec::new(),
        end_of_candidates: false,
    })
}

/// Apply an `a=...` line to the current media block (or ignore at
/// session level for attributes we don't model).
fn apply_attribute(
    value: &str,
    cur: &mut Option<MediaDescription>,
    line: usize,
) -> Result<(), ParseError> {
    if let Some(rest) = value.strip_prefix("rtpmap:") {
        let Some(m) = cur.as_mut() else {
            return Ok(()); // rtpmap outside media — unusual, ignore
        };
        let rtpmap = parse_rtpmap(rest, line)?;
        m.rtpmap.push(rtpmap);
        return Ok(());
    }
    if value.starts_with("crypto:") {
        // `SdesCrypto::parse` wants the full `a=crypto:...` form; reattach.
        let Some(m) = cur.as_mut() else {
            return Ok(()); // crypto outside media — ignore
        };
        // `a=crypto:` parse errors are soft: log and skip. A single
        // malformed line shouldn't fail the whole SDP document; the
        // negotiator will simply see fewer crypto options and may
        // return Mismatch if none are acceptable.
        match SdesCrypto::parse(&format!("a={value}")) {
            Ok(c) => m.crypto.push(c),
            Err(e) => tracing::debug!(line, ?e, "skipping malformed a=crypto line"),
        }
        return Ok(());
    }
    // ---- DTLS-SRTP + ICE attributes (RFC 5763 / RFC 8122 / RFC 8839) ----
    //
    // All soft-fail on parse — a malformed line should never fail the
    // whole SDP; the negotiator will see missing fields and decide
    // whether to reject at the transport level.
    if let Some(rest) = value.strip_prefix("fingerprint:")
        && let Some(m) = cur.as_mut()
    {
        let rest = rest.trim();
        if let Some((algo, val)) = rest.split_once(char::is_whitespace) {
            m.fingerprint = Some(Fingerprint {
                algorithm: algo.to_ascii_lowercase(),
                value: val.trim().to_owned(),
            });
        } else {
            tracing::debug!(line, "skipping malformed a=fingerprint (missing value)");
        }
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("setup:")
        && let Some(m) = cur.as_mut()
    {
        if let Some(s) = DtlsSetup::parse(rest.trim()) {
            m.setup = Some(s);
        } else {
            tracing::debug!(line, token = rest, "skipping unknown a=setup");
        }
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("ice-ufrag:")
        && let Some(m) = cur.as_mut()
    {
        m.ice_ufrag = Some(rest.trim().to_owned());
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("ice-pwd:")
        && let Some(m) = cur.as_mut()
    {
        m.ice_pwd = Some(IcePassword(rest.trim().to_owned()));
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("ice-options:")
        && let Some(m) = cur.as_mut()
    {
        m.ice_options = rest.split_whitespace().map(str::to_owned).collect();
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("candidate:")
        && let Some(m) = cur.as_mut()
    {
        match parse_candidate(rest, line) {
            Ok(c) => m.candidates.push(c),
            Err(e) => tracing::debug!(line, ?e, "skipping malformed a=candidate line"),
        }
        return Ok(());
    }
    if value == "end-of-candidates"
        && let Some(m) = cur.as_mut()
    {
        m.end_of_candidates = true;
        return Ok(());
    }
    let dir = match value {
        "sendrecv" => Some(Direction::SendRecv),
        "sendonly" => Some(Direction::SendOnly),
        "recvonly" => Some(Direction::RecvOnly),
        "inactive" => Some(Direction::Inactive),
        _ => None,
    };
    if let Some(d) = dir
        && let Some(m) = cur.as_mut()
    {
        m.direction = d;
    }
    Ok(())
}

/// `a=rtpmap:<pt> <name>/<clock>[/<channels>]`
fn parse_rtpmap(rest: &str, line: usize) -> Result<RtpMap, ParseError> {
    let (pt_tok, rest) = rest.split_once(' ').ok_or_else(|| ParseError::Malformed {
        line,
        reason: "rtpmap missing space".into(),
    })?;
    let payload_type = pt_tok.parse::<u8>().map_err(|e| ParseError::Malformed {
        line,
        reason: format!("rtpmap pt `{pt_tok}`: {e}"),
    })?;
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(ParseError::Malformed {
            line,
            reason: format!("rtpmap value `{rest}`"),
        });
    }
    let codec = parts[0].to_owned();
    let clock_rate = parts[1].parse::<u32>().map_err(|e| ParseError::Malformed {
        line,
        reason: format!("rtpmap clock: {e}"),
    })?;
    let channels = if parts.len() == 3 {
        Some(parts[2].parse::<u8>().map_err(|e| ParseError::Malformed {
            line,
            reason: format!("rtpmap channels: {e}"),
        })?)
    } else {
        None
    };
    Ok(RtpMap {
        payload_type,
        codec,
        clock_rate,
        channels,
    })
}

/// `candidate:<foundation> <component> <transport> <priority> <ip>
/// <port> typ <type> [raddr <ip>] [rport <port>] [<k> <v>]*`.
fn parse_candidate(rest: &str, line: usize) -> Result<IceCandidate, ParseError> {
    let mut parts = rest.split_whitespace();
    let mut next = |field: &'static str| {
        parts.next().ok_or_else(|| ParseError::Malformed {
            line,
            reason: format!("candidate missing {field}"),
        })
    };
    let foundation = next("foundation")?.to_owned();
    let component = next("component")?
        .parse::<u8>()
        .map_err(|e| ParseError::Malformed {
            line,
            reason: format!("candidate component: {e}"),
        })?;
    let transport = next("transport")?.to_owned();
    let priority = next("priority")?
        .parse::<u32>()
        .map_err(|e| ParseError::Malformed {
            line,
            reason: format!("candidate priority: {e}"),
        })?;
    let address_str = next("address")?;
    let address = IpAddr::from_str(address_str).map_err(|e| ParseError::Malformed {
        line,
        reason: format!("candidate address `{address_str}`: {e}"),
    })?;
    let port = next("port")?
        .parse::<u16>()
        .map_err(|e| ParseError::Malformed {
            line,
            reason: format!("candidate port: {e}"),
        })?;
    let typ_tok = next("`typ`")?;
    if !typ_tok.eq_ignore_ascii_case("typ") {
        return Err(ParseError::Malformed {
            line,
            reason: format!("candidate expected `typ`, got `{typ_tok}`"),
        });
    }
    let candidate_type = next("type")?.to_owned();

    // Everything after `typ <type>` is optional: `raddr <ip>`,
    // `rport <port>`, then arbitrary k/v pairs. We consume the
    // remaining tokens two at a time.
    let mut related_address = None;
    let mut related_port = None;
    let mut raw_params = Vec::new();
    while let Some(key) = parts.next() {
        let Some(value) = parts.next() else {
            return Err(ParseError::Malformed {
                line,
                reason: format!("candidate trailing key `{key}` without value"),
            });
        };
        match key {
            "raddr" => {
                related_address =
                    Some(IpAddr::from_str(value).map_err(|e| ParseError::Malformed {
                        line,
                        reason: format!("candidate raddr: {e}"),
                    })?);
            }
            "rport" => {
                related_port = Some(value.parse::<u16>().map_err(|e| ParseError::Malformed {
                    line,
                    reason: format!("candidate rport: {e}"),
                })?);
            }
            _ => raw_params.push((key.to_owned(), value.to_owned())),
        }
    }
    Ok(IceCandidate {
        foundation,
        component,
        transport,
        priority,
        address,
        port,
        candidate_type,
        related_address,
        related_port,
        raw_params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_OFFER: &str = concat!(
        "v=0\r\n",
        "o=alice 2890844526 2890844527 IN IP4 192.0.2.101\r\n",
        "s=-\r\n",
        "c=IN IP4 192.0.2.101\r\n",
        "t=0 0\r\n",
        "m=audio 49170 RTP/AVP 0 8 111\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "a=rtpmap:111 opus/48000/2\r\n",
        "a=sendrecv\r\n",
    );

    #[test]
    fn parses_typical_offer() {
        let sdp = SessionDescription::parse(SAMPLE_OFFER).unwrap();
        assert_eq!(sdp.origin.username, "alice");
        assert_eq!(sdp.origin.session_id, 2_890_844_526);
        assert_eq!(sdp.session_name, "-");
        assert_eq!(
            sdp.connection.map(|c| c.address.to_string()),
            Some("192.0.2.101".to_string())
        );
        assert_eq!(sdp.media.len(), 1);
        let m = &sdp.media[0];
        assert_eq!(m.kind, MediaKind::Audio);
        assert_eq!(m.port, 49170);
        assert_eq!(m.protocol, "RTP/AVP");
        assert_eq!(m.formats, vec![0, 8, 111]);
        assert_eq!(m.rtpmap.len(), 3);
        assert_eq!(m.rtpmap[2].codec, "opus");
        assert_eq!(m.rtpmap[2].clock_rate, 48_000);
        assert_eq!(m.rtpmap[2].channels, Some(2));
        assert_eq!(m.direction, Direction::SendRecv);
    }

    #[test]
    fn round_trip_via_display() {
        let sdp = SessionDescription::parse(SAMPLE_OFFER).unwrap();
        let serialized = sdp.to_string();
        let reparsed = SessionDescription::parse(&serialized).unwrap();
        assert_eq!(sdp, reparsed);
    }

    #[test]
    fn rejects_missing_version() {
        let bad = "o=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n";
        assert!(matches!(
            SessionDescription::parse(bad),
            Err(ParseError::MissingSessionLine("v"))
        ));
    }

    #[test]
    fn accepts_bare_lf_line_endings() {
        let bare = SAMPLE_OFFER.replace("\r\n", "\n");
        let sdp = SessionDescription::parse(&bare).unwrap();
        assert_eq!(sdp.media[0].port, 49170);
    }
}
