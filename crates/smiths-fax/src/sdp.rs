//! SDP surface for T.38 negotiation (ITU-T T.38 §D.2.3.5, RFC 3362).
//!
//! A T.38 offer looks like:
//!
//! ```text
//! m=image 6250 udptl t38
//! c=IN IP4 203.0.113.7
//! a=T38FaxVersion:0
//! a=T38MaxBitRate:14400
//! a=T38FaxRateManagement:transferredTCF
//! a=T38FaxMaxBuffer:72
//! a=T38FaxMaxDatagram:316
//! a=T38FaxUdpEC:t38UDPRedundancy
//! ```
//!
//! The protocol token is `udptl t38` (case-insensitive). Every `a=T38…`
//! attribute is optional — a bare `m=image 6250 udptl t38` with no
//! attributes is a valid offer that peers interpret with default
//! values. We preserve whatever the offerer emits, echo it back when
//! we're the answerer, and expose a typed view through [`T38Params`]
//! for callers that want it.

use std::fmt::Write as _;

use smiths_sdp::{
    ConnectionInfo, Direction, MediaDescription, MediaKind, Origin, SessionDescription,
};

/// `udptl` protocol token on the `m=` line.
pub const UDPTL_PROTOCOL: &str = "udptl";

/// Format token on a T.38 `m=image` line (`t38` lowercase).
pub const T38_FORMAT: &str = "t38";

/// T.38 session parameters extracted from `a=T38…` lines.
///
/// Every field is `Option<_>` — T.38 specifies defaults for each
/// attribute, but the engine has no business inventing values the
/// peer didn't offer. A downstream gateway that *needs* a specific
/// knob (bit rate, rate management) is the one that should surface
/// an error when it's missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct T38Params {
    /// `a=T38FaxVersion:<u8>` — protocol version. Typical values
    /// are 0, 1, 2, 3.
    pub version: Option<u8>,
    /// `a=T38MaxBitRate:<bps>` — cap on modem rate the terminal will
    /// attempt. Often 14400.
    pub max_bit_rate: Option<u32>,
    /// `a=T38FaxRateManagement:<token>` — usually
    /// `transferredTCF` (preferred) or `localTCF`.
    pub rate_management: Option<String>,
    /// `a=T38FaxMaxBuffer:<bytes>` — terminal buffer advertisement.
    pub max_buffer: Option<u32>,
    /// `a=T38FaxMaxDatagram:<bytes>` — MTU advertisement.
    pub max_datagram: Option<u32>,
    /// `a=T38FaxUdpEC:<token>` — error-correction profile. Typical
    /// values are `t38UDPRedundancy` (slice 5.4 parses redundancy in
    /// [`crate::udptl`]) or `t38UDPFEC`.
    pub udp_ec: Option<String>,
}

impl T38Params {
    /// Parse `a=T38…` attribute lines out of a media block. Unknown
    /// attribute names are ignored; unknown values pass through the
    /// `Option<String>` fields verbatim so a future attribute value
    /// doesn't force a crate upgrade.
    ///
    /// The `attrs` slice accepts lines *already stripped* of the
    /// leading `a=` — the current SDP parser doesn't preserve
    /// arbitrary `a=` lines on `MediaDescription`, so callers pass
    /// them in out-of-band. Operators building offers from scratch
    /// use [`Self::render_attrs`] for the reverse direction.
    #[must_use]
    pub fn parse_attrs(attrs: &[&str]) -> Self {
        let mut out = Self::default();
        for line in attrs {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            match name {
                "T38FaxVersion" => out.version = value.trim().parse().ok(),
                "T38MaxBitRate" => out.max_bit_rate = value.trim().parse().ok(),
                "T38FaxRateManagement" => out.rate_management = Some(value.trim().to_owned()),
                "T38FaxMaxBuffer" => out.max_buffer = value.trim().parse().ok(),
                "T38FaxMaxDatagram" => out.max_datagram = value.trim().parse().ok(),
                "T38FaxUdpEC" => out.udp_ec = Some(value.trim().to_owned()),
                _ => {}
            }
        }
        out
    }

    /// Render the non-empty fields back to wire-format `a=T38…`
    /// attribute lines (no leading `a=` — the caller splices them
    /// into its SDP emitter as needed).
    #[must_use]
    pub fn render_attrs(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(v) = self.version {
            out.push(format!("T38FaxVersion:{v}"));
        }
        if let Some(r) = self.max_bit_rate {
            out.push(format!("T38MaxBitRate:{r}"));
        }
        if let Some(r) = &self.rate_management {
            out.push(format!("T38FaxRateManagement:{r}"));
        }
        if let Some(b) = self.max_buffer {
            out.push(format!("T38FaxMaxBuffer:{b}"));
        }
        if let Some(d) = self.max_datagram {
            out.push(format!("T38FaxMaxDatagram:{d}"));
        }
        if let Some(e) = &self.udp_ec {
            out.push(format!("T38FaxUdpEC:{e}"));
        }
        out
    }

    /// Sensible defaults for a locally-initiated T.38 offer. Matches
    /// what most soft-PBX implementations (Asterisk, `FreeSWITCH`,
    /// typical ATAs) emit.
    #[must_use]
    pub fn sensible_offer() -> Self {
        Self {
            version: Some(0),
            max_bit_rate: Some(14_400),
            rate_management: Some("transferredTCF".into()),
            max_buffer: Some(72),
            max_datagram: Some(316),
            udp_ec: Some("t38UDPRedundancy".into()),
        }
    }
}

/// Find the T.38 media block on an SDP. Returns the index into
/// `sdp.media` on match so callers can mutate it in place.
#[must_use]
pub fn find_fax_media(sdp: &SessionDescription) -> Option<usize> {
    sdp.media.iter().position(is_t38_media)
}

/// Is this m-line an `m=image ... udptl t38 ...` offer?
#[must_use]
pub fn is_t38_media(m: &MediaDescription) -> bool {
    m.kind == MediaKind::Image
        && m.protocol
            .to_ascii_lowercase()
            .split_whitespace()
            .any(|tok| tok == UDPTL_PROTOCOL)
}

/// Build a bare T.38 offer from scratch — one `m=image <port> udptl
/// t38` block plus the session-level boilerplate. The attribute set
/// is rendered from `params` (no leading `a=`, `MediaDescription`
/// doesn't carry arbitrary attributes today). Callers that want to
/// include `a=T38…` lines on the wire can splice `params.render_attrs()`
/// into the `Display` output before sending.
#[must_use]
pub fn offer_fax(
    origin: Origin,
    session_name: impl Into<String>,
    connection: ConnectionInfo,
    port: u16,
    _params: &T38Params,
) -> SessionDescription {
    SessionDescription {
        origin,
        session_name: session_name.into(),
        connection: Some(connection),
        media: vec![MediaDescription {
            kind: MediaKind::Image,
            port,
            protocol: format!("{UDPTL_PROTOCOL} {T38_FORMAT}"),
            formats: vec![],
            rtpmap: vec![],
            crypto: vec![],
            direction: Direction::SendRecv,
            connection: None,
            fingerprint: None,
            setup: None,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: vec![],
            candidates: vec![],
            end_of_candidates: false,
        }],
    }
}

/// Build an answer SDP for a received T.38 offer. Delegates the
/// audio m-line (if one exists) with port 0 per RFC 3264 §6, and
/// mirrors the fax block with a locally-bound port.
///
/// Returns [`None`] when `offer` has no T.38 block — callers should
/// fall back to the regular audio negotiator in that case.
#[must_use]
pub fn answer_fax_offer(
    offer: &SessionDescription,
    origin: Origin,
    answer_addr: ConnectionInfo,
    local_fax_port: u16,
) -> Option<SessionDescription> {
    let fax_idx = find_fax_media(offer)?;
    let fax_m = &offer.media[fax_idx];

    let mut media = Vec::with_capacity(offer.media.len());
    for (idx, m) in offer.media.iter().enumerate() {
        if idx == fax_idx {
            media.push(MediaDescription {
                kind: MediaKind::Image,
                port: local_fax_port,
                protocol: fax_m.protocol.clone(),
                formats: vec![],
                rtpmap: vec![],
                crypto: vec![],
                direction: Direction::SendRecv,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: vec![],
                candidates: vec![],
                end_of_candidates: false,
            });
        } else {
            // RFC 3264 §6: declined m-line keeps `kind` and
            // `protocol`, carries port 0, drops every attribute. The
            // answerer MUST echo the same set of m-lines, same order.
            media.push(MediaDescription {
                kind: m.kind.clone(),
                port: 0,
                protocol: m.protocol.clone(),
                formats: m.formats.clone(),
                rtpmap: vec![],
                crypto: vec![],
                direction: Direction::Inactive,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: vec![],
                candidates: vec![],
                end_of_candidates: false,
            });
        }
    }

    Some(SessionDescription {
        origin,
        session_name: "-".into(),
        connection: Some(answer_addr),
        media,
    })
}

/// Helper to render a complete T.38 offer including the `a=T38…`
/// attribute lines that [`SessionDescription::Display`] doesn't know
/// to emit on its own. Suitable for `Content-Type:
/// application/sdp` bodies on a re-INVITE.
#[must_use]
pub fn render_offer_with_params(sdp: &SessionDescription, params: &T38Params) -> String {
    let mut out = sdp.to_string();
    for line in params.render_attrs() {
        // Attribute lines go after the m-line they apply to. Our
        // sdp types don't carry per-m-line free attributes, so we
        // append at the very end of the SDP — which is still
        // media-level for the current (single-m-line) offer shape.
        // `writeln!` appends '\n' after the explicit '\r', yielding
        // the RFC 8866-mandated '\r\n'.
        let _ = writeln!(out, "a={line}\r");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn origin() -> Origin {
        Origin {
            username: "-".into(),
            session_id: 1,
            session_version: 1,
            address: IpAddr::V4([127, 0, 0, 1].into()),
        }
    }

    fn conn() -> ConnectionInfo {
        ConnectionInfo {
            address: IpAddr::V4([127, 0, 0, 1].into()),
        }
    }

    #[test]
    fn t38_offer_has_image_udptl_m_line() {
        let sdp = offer_fax(origin(), "fax", conn(), 6250, &T38Params::sensible_offer());
        let wire = sdp.to_string();
        assert!(wire.contains("m=image 6250 udptl t38"));
        assert!(find_fax_media(&sdp).is_some());
    }

    #[test]
    fn params_round_trip_through_wire_form() {
        let original = T38Params::sensible_offer();
        let rendered = original.render_attrs();
        let lines: Vec<&str> = rendered.iter().map(String::as_str).collect();
        let parsed = T38Params::parse_attrs(&lines);
        assert_eq!(parsed, original);
    }

    #[test]
    fn answer_declines_audio_and_echoes_fax_with_local_port() {
        // Build an offer that has [audio, image] m-lines.
        let mut sdp = offer_fax(origin(), "fax", conn(), 6250, &T38Params::default());
        sdp.media.insert(
            0,
            MediaDescription {
                kind: MediaKind::Audio,
                port: 5004,
                protocol: "RTP/AVP".into(),
                formats: vec![0],
                rtpmap: vec![],
                crypto: vec![],
                direction: Direction::SendRecv,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: vec![],
                candidates: vec![],
                end_of_candidates: false,
            },
        );
        let answer = answer_fax_offer(&sdp, origin(), conn(), 7000).unwrap();
        assert_eq!(answer.media[0].kind, MediaKind::Audio);
        assert_eq!(answer.media[0].port, 0, "audio must be declined");
        assert_eq!(answer.media[1].kind, MediaKind::Image);
        assert_eq!(answer.media[1].port, 7000);
        assert!(answer.media[1].protocol.contains("udptl"));
    }

    #[test]
    fn is_t38_media_is_case_insensitive() {
        let mut m = MediaDescription {
            kind: MediaKind::Image,
            port: 1,
            protocol: "UDPTL t38".into(),
            formats: vec![],
            rtpmap: vec![],
            crypto: vec![],
            direction: Direction::SendRecv,
            connection: None,
            fingerprint: None,
            setup: None,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: vec![],
            candidates: vec![],
            end_of_candidates: false,
        };
        assert!(is_t38_media(&m));
        m.protocol = "RTP/AVP".into();
        assert!(!is_t38_media(&m));
    }

    #[test]
    fn answer_returns_none_on_non_t38_offer() {
        let sdp = SessionDescription {
            origin: origin(),
            session_name: "audio".into(),
            connection: Some(conn()),
            media: vec![MediaDescription {
                kind: MediaKind::Audio,
                port: 5004,
                protocol: "RTP/AVP".into(),
                formats: vec![0],
                rtpmap: vec![],
                crypto: vec![],
                direction: Direction::SendRecv,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: vec![],
                candidates: vec![],
                end_of_candidates: false,
            }],
        };
        assert!(answer_fax_offer(&sdp, origin(), conn(), 7000).is_none());
    }

    #[test]
    fn render_offer_with_params_emits_attribute_lines() {
        let sdp = offer_fax(origin(), "fax", conn(), 6250, &T38Params::default());
        let wire = render_offer_with_params(&sdp, &T38Params::sensible_offer());
        assert!(wire.contains("a=T38FaxVersion:0"));
        assert!(wire.contains("a=T38FaxUdpEC:t38UDPRedundancy"));
    }
}
