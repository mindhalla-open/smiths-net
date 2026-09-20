//! Line-oriented SDP parser.
//!
//! Accepts `\r\n` and bare `\n` line endings. Ignores blank lines and
//! `a=` attributes we don't model explicitly.
//!
//! Session-level `a=fingerprint`, `a=setup`, `a=ice-ufrag`,
//! `a=ice-pwd`, `a=ice-options` and direction attributes are
//! inherited by every media block that doesn't override them
//! (RFC 8839 §5.4, RFC 8122 §5) — Firefox, in particular, puts the
//! DTLS fingerprint at session level.

use std::net::IpAddr;
use std::str::FromStr;

use crate::error::ParseError;
use crate::srtp_attr::SdesCrypto;
use crate::types::{
    ConnectionInfo, Direction, DtlsSetup, ExtMap, Fingerprint, Fmtp, Group, IceCandidate,
    IcePassword, MediaDescription, MediaKind, Origin, RtcpAttr, RtcpFeedback, RtpMap,
    SessionDescription, T38Params,
};

/// Session-level attributes that media blocks inherit.
#[derive(Default)]
struct SessionLevel {
    fingerprint: Option<Fingerprint>,
    setup: Option<DtlsSetup>,
    ice_ufrag: Option<String>,
    ice_pwd: Option<IcePassword>,
    ice_options: Vec<String>,
    direction: Option<Direction>,
    groups: Vec<Group>,
    ice_lite: bool,
}

impl SessionDescription {
    /// Parse an SDP document.
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let mut origin: Option<Origin> = None;
        let mut session_name: Option<String> = None;
        let mut session_connection: Option<ConnectionInfo> = None;
        let mut session = SessionLevel::default();
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
                    let mut m = parse_media(value, line_no)?;
                    session.seed(&mut m);
                    cur = Some(m);
                }
                "a" => match cur.as_mut() {
                    Some(m) => apply_media_attribute(value, m, line_no)?,
                    None => apply_session_attribute(value, &mut session, line_no),
                },
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
            groups: session.groups,
            ice_lite: session.ice_lite,
            media,
        })
    }
}

impl SessionLevel {
    /// Copy the inheritable session-level attributes onto a freshly
    /// parsed media block. Media-level lines parsed afterwards
    /// override them.
    fn seed(&self, m: &mut MediaDescription) {
        m.fingerprint.clone_from(&self.fingerprint);
        m.setup = self.setup;
        m.ice_ufrag.clone_from(&self.ice_ufrag);
        m.ice_pwd.clone_from(&self.ice_pwd);
        m.ice_options.clone_from(&self.ice_options);
        if let Some(d) = self.direction {
            m.direction = d;
        }
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
    let protocol = parts.next().ok_or_else(|| ParseError::Malformed {
        line,
        reason: "m= missing protocol".into(),
    })?;

    let mut m = MediaDescription::new(MediaKind::parse(kind), port, protocol);
    // Format tokens are opaque here: RTP profiles carry payload-type
    // numbers, `udptl` carries `t38`. Numeric validation happens on
    // access via `MediaDescription::payload_types`.
    m.formats = parts.map(str::to_owned).collect();
    Ok(m)
}

/// Apply an `a=...` line that appeared before the first `m=`.
fn apply_session_attribute(value: &str, session: &mut SessionLevel, line: usize) {
    if let Some(rest) = value.strip_prefix("group:") {
        let mut toks = rest.split_whitespace();
        if let Some(semantics) = toks.next() {
            session.groups.push(Group {
                semantics: semantics.to_owned(),
                mids: toks.map(str::to_owned).collect(),
            });
        }
        return;
    }
    if value == "ice-lite" {
        session.ice_lite = true;
        return;
    }
    if let Some(rest) = value.strip_prefix("fingerprint:") {
        session.fingerprint = parse_fingerprint(rest, line);
        return;
    }
    if let Some(rest) = value.strip_prefix("setup:") {
        session.setup = DtlsSetup::parse(rest.trim());
        return;
    }
    if let Some(rest) = value.strip_prefix("ice-ufrag:") {
        session.ice_ufrag = Some(rest.trim().to_owned());
        return;
    }
    if let Some(rest) = value.strip_prefix("ice-pwd:") {
        session.ice_pwd = Some(IcePassword(rest.trim().to_owned()));
        return;
    }
    if let Some(rest) = value.strip_prefix("ice-options:") {
        session.ice_options = rest.split_whitespace().map(str::to_owned).collect();
        return;
    }
    if let Some(d) = Direction::parse(value) {
        session.direction = Some(d);
    }
}

/// Apply an `a=...` line to the current media block.
///
/// Only `a=rtpmap` is strict — a malformed one is a real error
/// because the negotiator can't pick a codec without it. Everything
/// else soft-fails: the line is logged and skipped, and the
/// negotiator decides what missing information means.
fn apply_media_attribute(
    value: &str,
    m: &mut MediaDescription,
    line: usize,
) -> Result<(), ParseError> {
    if let Some(rest) = value.strip_prefix("rtpmap:") {
        m.rtpmap.push(parse_rtpmap(rest, line)?);
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("fmtp:") {
        if let Some((format, params)) = rest.trim().split_once(char::is_whitespace) {
            m.fmtp.push(Fmtp {
                format: format.to_owned(),
                params: params.trim().to_owned(),
            });
        } else {
            tracing::debug!(line, "skipping malformed a=fmtp (missing parameters)");
        }
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("ptime:") {
        m.ptime = rest.trim().parse().ok();
        return Ok(());
    }
    if let Some(rest) = value.strip_prefix("maxptime:") {
        m.maxptime = rest.trim().parse().ok();
        return Ok(());
    }
    if value.starts_with("crypto:") {
        // `SdesCrypto::parse` wants the full `a=crypto:...` form; reattach.
        // A single malformed line shouldn't fail the whole SDP; the
        // negotiator will simply see fewer crypto options.
        match SdesCrypto::parse(&format!("a={value}")) {
            Ok(c) => m.crypto.push(c),
            Err(e) => tracing::debug!(line, ?e, "skipping malformed a=crypto line"),
        }
        return Ok(());
    }
    if apply_security_attribute(value, m, line) || apply_multiplex_attribute(value, m, line) {
        return Ok(());
    }
    // ---- T.38 (ITU-T T.38 Annex D) ----
    if value.starts_with(T38Params::ATTR_PREFIX) {
        m.t38
            .get_or_insert_with(T38Params::default)
            .apply_attribute(value);
        return Ok(());
    }
    if let Some(d) = Direction::parse(value) {
        m.direction = d;
    }
    Ok(())
}

/// DTLS-SRTP and ICE attributes (RFC 5763 / RFC 8122 / RFC 8839).
/// Returns `true` when `value` was one of them.
fn apply_security_attribute(value: &str, m: &mut MediaDescription, line: usize) -> bool {
    if let Some(rest) = value.strip_prefix("fingerprint:") {
        if let Some(fp) = parse_fingerprint(rest, line) {
            m.fingerprint = Some(fp);
        }
        return true;
    }
    if let Some(rest) = value.strip_prefix("setup:") {
        if let Some(s) = DtlsSetup::parse(rest.trim()) {
            m.setup = Some(s);
        } else {
            tracing::debug!(line, token = rest, "skipping unknown a=setup");
        }
        return true;
    }
    if let Some(rest) = value.strip_prefix("ice-ufrag:") {
        m.ice_ufrag = Some(rest.trim().to_owned());
        return true;
    }
    if let Some(rest) = value.strip_prefix("ice-pwd:") {
        m.ice_pwd = Some(IcePassword(rest.trim().to_owned()));
        return true;
    }
    if let Some(rest) = value.strip_prefix("ice-options:") {
        m.ice_options = rest.split_whitespace().map(str::to_owned).collect();
        return true;
    }
    if let Some(rest) = value.strip_prefix("candidate:") {
        match parse_candidate(rest, line) {
            Ok(c) => m.candidates.push(c),
            Err(e) => tracing::debug!(line, ?e, "skipping malformed a=candidate line"),
        }
        return true;
    }
    if value == "end-of-candidates" {
        m.end_of_candidates = true;
        return true;
    }
    false
}

/// BUNDLE grouping, header extensions and RTCP attributes
/// (RFC 5888, 8285, 5761, 3605, 4585). Returns `true` when `value`
/// was one of them.
fn apply_multiplex_attribute(value: &str, m: &mut MediaDescription, line: usize) -> bool {
    if let Some(rest) = value.strip_prefix("mid:") {
        m.mid = Some(rest.trim().to_owned());
        return true;
    }
    if let Some(rest) = value.strip_prefix("extmap:") {
        if let Some(e) = parse_extmap(rest) {
            m.extmap.push(e);
        } else {
            tracing::debug!(line, "skipping malformed a=extmap line");
        }
        return true;
    }
    if value == "rtcp-mux" {
        m.rtcp_mux = true;
        return true;
    }
    if let Some(rest) = value.strip_prefix("rtcp:") {
        if let Some(r) = parse_rtcp(rest) {
            m.rtcp = Some(r);
        } else {
            tracing::debug!(line, "skipping malformed a=rtcp line");
        }
        return true;
    }
    if let Some(rest) = value.strip_prefix("rtcp-fb:") {
        if let Some((format, fb)) = rest.trim().split_once(char::is_whitespace) {
            m.rtcp_fb.push(RtcpFeedback {
                format: format.to_owned(),
                value: fb.trim().to_owned(),
            });
        } else {
            tracing::debug!(line, "skipping malformed a=rtcp-fb line");
        }
        return true;
    }
    false
}

/// `a=fingerprint:<algorithm> <value>`
fn parse_fingerprint(rest: &str, line: usize) -> Option<Fingerprint> {
    let rest = rest.trim();
    if let Some((algo, val)) = rest.split_once(char::is_whitespace) {
        Some(Fingerprint {
            algorithm: algo.to_ascii_lowercase(),
            value: val.trim().to_owned(),
        })
    } else {
        tracing::debug!(line, "skipping malformed a=fingerprint (missing value)");
        None
    }
}

/// `a=extmap:<id>[/<direction>] <uri> [<attributes>]`
fn parse_extmap(rest: &str) -> Option<ExtMap> {
    let mut toks = rest.trim().splitn(3, char::is_whitespace);
    let id_tok = toks.next()?;
    let uri = toks.next()?.to_owned();
    let attributes = toks
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let (id_str, direction) = match id_tok.split_once('/') {
        Some((id, dir)) => (id, Some(Direction::parse(dir)?)),
        None => (id_tok, None),
    };
    let id = id_str.parse::<u16>().ok()?;
    Some(ExtMap {
        id,
        direction,
        uri,
        attributes,
    })
}

/// `a=rtcp:<port> [<nettype> <addrtype> <address>]`
fn parse_rtcp(rest: &str) -> Option<RtcpAttr> {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let port = parts.first()?.parse::<u16>().ok()?;
    let address = match parts.len() {
        1 => None,
        4 => Some(IpAddr::from_str(parts[3]).ok()?),
        _ => return None,
    };
    Some(RtcpAttr { port, address })
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

/// Public wrapper around the internal candidate-line parser. Accepts
/// the wire form a trickle-ICE signaling frame carries: the part
/// **after** the `candidate:` keyword. Strips an optional leading
/// `candidate:` / `a=candidate:` for caller ergonomics.
///
/// # Errors
/// Returns [`ParseError`] when any required field is
/// missing or unparseable.
pub fn parse_candidate_line(raw: &str) -> Result<IceCandidate, ParseError> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("a=candidate:")
        .or_else(|| trimmed.strip_prefix("candidate:"))
        .unwrap_or(trimmed);
    parse_candidate(inner, 0)
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

    /// Shape of a Chrome audio+video offer: BUNDLE, mids, extmap,
    /// rtcp-mux, fmtp, rtcp-fb, telephone-event, session-level
    /// ICE/DTLS attributes.
    const BROWSER_OFFER: &str = concat!(
        "v=0\r\n",
        "o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "t=0 0\r\n",
        "a=group:BUNDLE 0 1\r\n",
        "a=ice-options:trickle\r\n",
        "a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n",
        "a=setup:actpass\r\n",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111 0 8 110 126\r\n",
        "c=IN IP4 0.0.0.0\r\n",
        "a=rtcp:9 IN IP4 0.0.0.0\r\n",
        "a=ice-ufrag:F7gI\r\n",
        "a=ice-pwd:x9cml/YzichV2+XlhiMu8g\r\n",
        "a=mid:0\r\n",
        "a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\n",
        "a=extmap:2/sendonly http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\n",
        "a=sendrecv\r\n",
        "a=rtcp-mux\r\n",
        "a=rtpmap:111 opus/48000/2\r\n",
        "a=rtcp-fb:111 transport-cc\r\n",
        "a=fmtp:111 minptime=10;useinbandfec=1\r\n",
        "a=rtpmap:0 PCMU/8000\r\n",
        "a=rtpmap:8 PCMA/8000\r\n",
        "a=rtpmap:110 telephone-event/48000\r\n",
        "a=rtpmap:126 telephone-event/8000\r\n",
        "a=fmtp:126 0-15\r\n",
        "a=ptime:20\r\n",
        "a=maxptime:120\r\n",
        "m=video 9 UDP/TLS/RTP/SAVPF 96 97\r\n",
        "c=IN IP4 0.0.0.0\r\n",
        "a=ice-ufrag:F7gI\r\n",
        "a=ice-pwd:x9cml/YzichV2+XlhiMu8g\r\n",
        "a=mid:1\r\n",
        "a=sendrecv\r\n",
        "a=rtcp-mux\r\n",
        "a=rtpmap:96 VP8/90000\r\n",
        "a=rtcp-fb:96 nack\r\n",
        "a=rtcp-fb:96 nack pli\r\n",
        "a=rtcp-fb:* ccm fir\r\n",
        "a=rtpmap:97 H264/90000\r\n",
        "a=fmtp:97 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
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
        assert_eq!(m.formats, ["0", "8", "111"]);
        assert_eq!(m.payload_types().collect::<Vec<u8>>(), vec![0, 8, 111]);
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

    #[test]
    fn parses_t38_image_line_with_non_numeric_format() {
        let wire = concat!(
            "v=0\r\n",
            "o=- 1 2 IN IP4 203.0.113.7\r\n",
            "s=-\r\n",
            "c=IN IP4 203.0.113.7\r\n",
            "t=0 0\r\n",
            "m=audio 0 RTP/AVP 0\r\n",
            "m=image 6250 udptl t38\r\n",
            "a=T38FaxVersion:0\r\n",
            "a=T38MaxBitRate:14400\r\n",
            "a=T38FaxFillBitRemoval\r\n",
            "a=T38FaxRateManagement:transferredTCF\r\n",
            "a=T38FaxMaxBuffer:72\r\n",
            "a=T38FaxMaxDatagram:316\r\n",
            "a=T38FaxUdpEC:t38UDPRedundancy\r\n",
        );
        let sdp = SessionDescription::parse(wire).expect("T.38 SDP must parse");
        assert_eq!(sdp.media.len(), 2);
        let fax = &sdp.media[1];
        assert_eq!(fax.kind, MediaKind::Image);
        assert_eq!(fax.protocol, "udptl");
        assert_eq!(fax.formats, ["t38"]);
        assert_eq!(fax.payload_types().count(), 0);
        let t38 = fax.t38.as_ref().expect("T.38 attributes parsed");
        assert_eq!(t38.version, Some(0));
        assert_eq!(t38.max_bit_rate, Some(14_400));
        assert!(t38.fill_bit_removal);
        assert_eq!(t38.rate_management.as_deref(), Some("transferredTCF"));
        assert_eq!(t38.max_buffer, Some(72));
        assert_eq!(t38.max_datagram, Some(316));
        assert_eq!(t38.udp_ec.as_deref(), Some("t38UDPRedundancy"));

        // Wire → struct → wire → struct is lossless.
        let reparsed = SessionDescription::parse(&sdp.to_string()).unwrap();
        assert_eq!(reparsed, sdp);
        assert!(sdp.to_string().contains("m=image 6250 udptl t38\r\n"));
        assert!(sdp.to_string().contains("a=T38FaxFillBitRemoval\r\n"));
    }

    #[test]
    fn parses_browser_offer_surface() {
        let sdp = SessionDescription::parse(BROWSER_OFFER).unwrap();
        assert_eq!(sdp.bundle_mids(), vec!["0", "1"]);
        assert!(!sdp.ice_lite);
        let audio = &sdp.media[0];
        assert_eq!(audio.mid.as_deref(), Some("0"));
        assert!(audio.rtcp_mux);
        assert_eq!(
            audio.rtcp,
            Some(RtcpAttr {
                port: 9,
                address: Some("0.0.0.0".parse().unwrap())
            })
        );
        assert_eq!(audio.ptime, Some(20));
        assert_eq!(audio.maxptime, Some(120));
        assert_eq!(audio.extmap.len(), 2);
        assert_eq!(audio.extmap[0].id, 1);
        assert_eq!(audio.extmap[0].direction, None);
        assert_eq!(audio.extmap[1].id, 2);
        assert_eq!(audio.extmap[1].direction, Some(Direction::SendOnly));
        assert_eq!(
            audio.fmtp_for(111).map(|f| f.params.as_str()),
            Some("minptime=10;useinbandfec=1")
        );
        assert_eq!(audio.fmtp_for(126).map(|f| f.params.as_str()), Some("0-15"));
        assert_eq!(audio.rtcp_fb.len(), 1);
        // Session-level DTLS + ice-options inherited by both m-lines.
        for m in &sdp.media {
            assert_eq!(m.fingerprint.as_ref().unwrap().algorithm, "sha-256");
            assert_eq!(m.setup, Some(DtlsSetup::ActPass));
            assert_eq!(m.ice_options, vec!["trickle"]);
            assert_eq!(m.ice_ufrag.as_deref(), Some("F7gI"));
        }
        let video = &sdp.media[1];
        assert_eq!(video.rtcp_fb.len(), 3);
        assert!(
            video.rtcp_fb[2].applies_to(97),
            "`*` wildcard applies to every PT"
        );
        assert!(!video.rtcp_fb[0].applies_to(97));

        // Round trip is lossless for everything we model.
        let reparsed = SessionDescription::parse(&sdp.to_string()).unwrap();
        assert_eq!(reparsed.bundle_mids(), vec!["0", "1"]);
        assert_eq!(reparsed.media[0].extmap, audio.extmap);
        assert_eq!(reparsed.media[0].fmtp, audio.fmtp);
        assert_eq!(reparsed.media[1].rtcp_fb, video.rtcp_fb);
        assert_eq!(reparsed.media[0].rtcp, audio.rtcp);
    }

    #[test]
    fn media_level_attributes_override_session_level() {
        let wire = concat!(
            "v=0\r\n",
            "o=- 1 2 IN IP4 127.0.0.1\r\n",
            "s=-\r\n",
            "t=0 0\r\n",
            "a=sendonly\r\n",
            "a=setup:actpass\r\n",
            "m=audio 4000 RTP/AVP 0\r\n",
            "a=setup:active\r\n",
            "m=video 4002 RTP/AVP 96\r\n",
            "a=recvonly\r\n",
        );
        let sdp = SessionDescription::parse(wire).unwrap();
        assert_eq!(sdp.media[0].direction, Direction::SendOnly);
        assert_eq!(sdp.media[0].setup, Some(DtlsSetup::Active));
        assert_eq!(sdp.media[1].direction, Direction::RecvOnly);
        assert_eq!(sdp.media[1].setup, Some(DtlsSetup::ActPass));
    }
}
