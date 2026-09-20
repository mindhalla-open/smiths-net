//! SDP surface for T.38 negotiation (ITU-T T.38 Annex D, RFC 3362).
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
//! The protocol token is `udptl` and the format token `t38` (both
//! case-insensitive). Every `a=T38…` attribute is optional — a bare
//! `m=image 6250 udptl t38` with no attributes is a valid offer that
//! peers interpret with default values. The `smiths-sdp` parser
//! types the attributes as [`T38Params`] on the media block, so an
//! offer parsed off the wire carries its parameters and the answer
//! is built from them; [`SessionDescription`]'s `Display` renders
//! them back.

pub use smiths_sdp::T38Params;
use smiths_sdp::{
    ConnectionInfo, Direction, MediaDescription, MediaKind, Origin, SessionDescription,
};

/// `udptl` protocol token on the `m=` line.
pub const UDPTL_PROTOCOL: &str = "udptl";

/// Format token on a T.38 `m=image` line (`t38` lowercase).
pub const T38_FORMAT: &str = "t38";

/// Find the T.38 media block on an SDP. Returns the index into
/// `sdp.media` on match so callers can mutate it in place.
#[must_use]
pub fn find_fax_media(sdp: &SessionDescription) -> Option<usize> {
    sdp.media.iter().position(is_t38_media)
}

/// Is this m-line an `m=image... udptl t38` offer?
///
/// The protocol must be `udptl`; the format list must name `t38` or
/// be empty (some gateways emit the bare `m=image <port> udptl`
/// form and rely on the T.38 default).
#[must_use]
pub fn is_t38_media(m: &MediaDescription) -> bool {
    m.kind == MediaKind::Image
        && m.protocol
            .split_whitespace()
            .next()
            .is_some_and(|tok| tok.eq_ignore_ascii_case(UDPTL_PROTOCOL))
        && (m.formats.is_empty() || m.has_format(T38_FORMAT))
}

/// Build the T.38 media block `m=image <port> udptl t38` carrying
/// `params` as `a=T38…` attributes.
#[must_use]
pub fn t38_media(port: u16, params: &T38Params) -> MediaDescription {
    let mut m = MediaDescription::new(MediaKind::Image, port, UDPTL_PROTOCOL);
    m.formats = vec![T38_FORMAT.to_owned()];
    m.t38 = Some(params.clone());
    m
}

/// Build a bare T.38 offer from scratch — one `m=image <port> udptl
/// t38` block with `params` rendered as `a=T38…` lines, plus the
/// session-level boilerplate.
#[must_use]
pub fn offer_fax(
    origin: Origin,
    session_name: impl Into<String>,
    connection: ConnectionInfo,
    port: u16,
    params: &T38Params,
) -> SessionDescription {
    SessionDescription {
        origin,
        session_name: session_name.into(),
        connection: Some(connection),
        groups: Vec::new(),
        ice_lite: false,
        media: vec![t38_media(port, params)],
    }
}

/// Build an answer SDP for a received T.38 offer. Declines every
/// other m-line (port 0 per RFC 3264 §6) and mirrors the fax block
/// on a locally-bound port.
///
/// The fax block echoes the offer's `a=T38…` parameters: the engine
/// is a UDPTL relay, so the terminals on either side own the
/// capability negotiation, and T.38 Annex D only requires the
/// answerer's values not to exceed the offerer's.
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

    let mut media = Vec::with_capacity(offer.media.len());
    for (idx, m) in offer.media.iter().enumerate() {
        if idx == fax_idx {
            let mut fax =
                MediaDescription::new(MediaKind::Image, local_fax_port, m.protocol.clone());
            fax.formats = if m.formats.is_empty() {
                vec![T38_FORMAT.to_owned()]
            } else {
                m.formats.clone()
            };
            fax.mid.clone_from(&m.mid);
            fax.t38.clone_from(&m.t38);
            media.push(fax);
        } else {
            media.push(declined(m));
        }
    }

    Some(SessionDescription {
        origin,
        session_name: "-".into(),
        connection: Some(answer_addr),
        groups: Vec::new(),
        ice_lite: false,
        media,
    })
}

/// RFC 3264 §6 declined m-line: same kind, protocol and formats,
/// port 0, `a=mid` kept for BUNDLE bookkeeping, every other
/// attribute dropped. The answerer MUST echo the same set of
/// m-lines in the same order.
#[must_use]
pub fn declined(m: &MediaDescription) -> MediaDescription {
    let mut out = MediaDescription::new(m.kind.clone(), 0, m.protocol.clone());
    out.formats.clone_from(&m.formats);
    out.mid.clone_from(&m.mid);
    out.direction = Direction::Inactive;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    /// Re-INVITE body as Asterisk emits it when switching to T.38.
    const ASTERISK_T38_REINVITE: &str = concat!(
        "v=0\r\n",
        "o=- 1731359015 1731359016 IN IP4 203.0.113.7\r\n",
        "s=Asterisk\r\n",
        "c=IN IP4 203.0.113.7\r\n",
        "t=0 0\r\n",
        "m=audio 0 RTP/AVP 0 101\r\n",
        "m=image 6250 udptl t38\r\n",
        "a=T38FaxVersion:0\r\n",
        "a=T38MaxBitRate:14400\r\n",
        "a=T38FaxRateManagement:transferredTCF\r\n",
        "a=T38FaxMaxBuffer:72\r\n",
        "a=T38FaxMaxDatagram:316\r\n",
        "a=T38FaxUdpEC:t38UDPRedundancy\r\n",
        "a=sendrecv\r\n",
    );

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

    fn audio_media(port: u16) -> MediaDescription {
        let mut m = MediaDescription::new(MediaKind::Audio, port, "RTP/AVP");
        m.formats = vec!["0".into()];
        m
    }

    #[test]
    fn t38_offer_has_image_udptl_m_line_and_renders_params() {
        let sdp = offer_fax(origin(), "fax", conn(), 6250, &T38Params::sensible_offer());
        let wire = sdp.to_string();
        assert!(wire.contains("m=image 6250 udptl t38\r\n"), "{wire}");
        assert!(wire.contains("a=T38FaxVersion:0\r\n"), "{wire}");
        assert!(wire.contains("a=T38MaxBitRate:14400\r\n"), "{wire}");
        assert!(
            wire.contains("a=T38FaxUdpEC:t38UDPRedundancy\r\n"),
            "{wire}"
        );
        assert!(find_fax_media(&sdp).is_some());
        // The rendered offer parses back to the same structure.
        let reparsed = SessionDescription::parse(&wire).unwrap();
        assert_eq!(reparsed, sdp);
        assert_eq!(
            reparsed.media[0].t38.as_ref(),
            Some(&T38Params::sensible_offer())
        );
    }

    #[test]
    fn params_round_trip_through_wire_form() {
        let original = T38Params::sensible_offer();
        let rendered = original.render_attrs();
        let parsed = T38Params::parse_attrs(&rendered);
        assert_eq!(parsed, original);
    }

    #[test]
    fn wire_offer_from_asterisk_parses_and_is_detected() {
        let offer =
            SessionDescription::parse(ASTERISK_T38_REINVITE).expect("T.38 re-INVITE parses");
        assert_eq!(find_fax_media(&offer), Some(1));
        let fax = &offer.media[1];
        let t38 = fax.t38.as_ref().expect("a=T38 attributes parsed");
        assert_eq!(t38.version, Some(0));
        assert_eq!(t38.max_bit_rate, Some(14_400));
        assert_eq!(t38.rate_management.as_deref(), Some("transferredTCF"));
        assert_eq!(t38.max_buffer, Some(72));
        assert_eq!(t38.max_datagram, Some(316));
        assert_eq!(t38.udp_ec.as_deref(), Some("t38UDPRedundancy"));
    }

    #[test]
    fn answer_declines_audio_and_echoes_fax_with_local_port_and_params() {
        let offer = SessionDescription::parse(ASTERISK_T38_REINVITE).unwrap();
        let answer = answer_fax_offer(&offer, origin(), conn(), 7000).unwrap();
        assert_eq!(answer.media.len(), 2, "same m-line count as the offer");
        assert_eq!(answer.media[0].kind, MediaKind::Audio);
        assert_eq!(answer.media[0].port, 0, "audio must be declined");
        assert_eq!(answer.media[0].formats, ["0", "101"]);
        assert_eq!(answer.media[1].kind, MediaKind::Image);
        assert_eq!(answer.media[1].port, 7000);
        assert_eq!(answer.media[1].protocol, "udptl");
        assert_eq!(answer.media[1].formats, ["t38"]);
        assert_eq!(
            answer.media[1].t38, offer.media[1].t38,
            "T.38 params echoed"
        );

        let wire = answer.to_string();
        assert!(wire.contains("m=audio 0 RTP/AVP 0 101\r\n"), "{wire}");
        assert!(wire.contains("m=image 7000 udptl t38\r\n"), "{wire}");
        assert!(
            wire.contains("a=T38FaxRateManagement:transferredTCF\r\n"),
            "{wire}"
        );
        assert!(wire.contains("a=T38FaxMaxDatagram:316\r\n"), "{wire}");
        let reparsed = SessionDescription::parse(&wire).unwrap();
        assert_eq!(reparsed, answer);
    }

    #[test]
    fn answer_to_struct_built_offer_declines_audio() {
        let mut sdp = offer_fax(origin(), "fax", conn(), 6250, &T38Params::default());
        sdp.media.insert(0, audio_media(5004));
        let answer = answer_fax_offer(&sdp, origin(), conn(), 7000).unwrap();
        assert_eq!(answer.media[0].kind, MediaKind::Audio);
        assert_eq!(answer.media[0].port, 0);
        assert_eq!(answer.media[1].kind, MediaKind::Image);
        assert_eq!(answer.media[1].port, 7000);
        assert!(answer.media[1].protocol.eq_ignore_ascii_case("udptl"));
    }

    #[test]
    fn is_t38_media_is_case_insensitive_and_accepts_bare_udptl() {
        let mut m = MediaDescription::new(MediaKind::Image, 1, "UDPTL");
        m.formats = vec!["T38".into()];
        assert!(is_t38_media(&m));
        m.formats.clear();
        assert!(
            is_t38_media(&m),
            "bare `m=image <port> udptl` is T.38 by default"
        );
        m.formats = vec!["t30".into()];
        assert!(!is_t38_media(&m));
        m.formats = vec!["t38".into()];
        m.protocol = "RTP/AVP".into();
        assert!(!is_t38_media(&m));
    }

    #[test]
    fn answer_returns_none_on_non_t38_offer() {
        let sdp = SessionDescription {
            origin: origin(),
            session_name: "audio".into(),
            connection: Some(conn()),
            groups: Vec::new(),
            ice_lite: false,
            media: vec![audio_media(5004)],
        };
        assert!(answer_fax_offer(&sdp, origin(), conn(), 7000).is_none());
    }
}
