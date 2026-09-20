//! Audio → T.38 renegotiation helper.
//!
//! When a terminal detects a fax CED tone during an audio call (or
//! a called-party answering machine forwards to a fax modem), the
//! common flow is:
//!
//! 1. UA-A decides to switch to T.38. Emits a re-INVITE whose SDP
//!    declines the audio m-line (port 0) and offers `m=image
//!    <port> udptl t38`.
//! 2. UA-B (or the engine-as-B2BUA) answers the re-INVITE: audio
//!    stays at port 0, image mirrors with a locally-bound port.
//! 3. Both sides tear down RTP and bring up UDPTL on the new ports.
//!
//! This module handles step 1. The answerer side is
//! [`crate::sdp::answer_fax_offer`].

use thiserror::Error;

use smiths_sdp::{ConnectionInfo, MediaKind, SessionDescription};

use crate::sdp::{T38Params, declined, t38_media};

/// Errors from [`fax_renegotiate`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FaxRenegotiateError {
    /// The active SDP has no audio m-line — can't switch *away from*
    /// audio if the call isn't audio to begin with.
    #[error("active SDP has no audio m-line to decline")]
    NoAudioMLine,
    /// The active SDP already carries a T.38 block. Double-switch is
    /// a caller bug; return the existing SDP instead.
    #[error("active SDP already carries a T.38 media block")]
    AlreadyFax,
}

/// Build a re-INVITE SDP that switches an active audio call to T.38.
///
/// `active` is the currently-negotiated SDP (what both sides agreed
/// on when the dialog came up). `local_fax_port` is the UDPTL port
/// the caller will bind for the relay; `local_addr` is the
/// connection address to advertise (typically the engine's public
/// address for this call); `params` are the `a=T38…` attributes to
/// offer.
///
/// On success the returned SDP has:
/// - The original audio m-line rewritten with `port=0`,
///   `direction=inactive` and every attribute but `a=mid` dropped.
/// - A fresh `m=image <port> udptl t38` block carrying `params`
///   appended.
/// - The session-level `c=` line updated to `local_addr`.
/// - `origin.session_version` bumped — RFC 3264 §5 requires the
///   answerer treat the SDP as changed.
///
/// # Errors
/// [`FaxRenegotiateError`] — see variant docs.
pub fn fax_renegotiate(
    active: &SessionDescription,
    local_addr: ConnectionInfo,
    local_fax_port: u16,
    params: &T38Params,
) -> Result<SessionDescription, FaxRenegotiateError> {
    if active.media.iter().any(crate::sdp::is_t38_media) {
        return Err(FaxRenegotiateError::AlreadyFax);
    }
    let audio_idx = active
        .media
        .iter()
        .position(|m| m.kind == MediaKind::Audio)
        .ok_or(FaxRenegotiateError::NoAudioMLine)?;

    let mut media = active.media.clone();
    media[audio_idx] = declined(&active.media[audio_idx]);
    media.push(t38_media(local_fax_port, params));

    let mut next = active.clone();
    next.connection = Some(local_addr);
    next.origin.session_version = active.origin.session_version.saturating_add(1);
    next.media = media;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_sdp::{Direction, MediaDescription, Origin, RtpMap};
    use std::net::IpAddr;

    fn origin() -> Origin {
        Origin {
            username: "-".into(),
            session_id: 1,
            session_version: 42,
            address: IpAddr::V4([127, 0, 0, 1].into()),
        }
    }

    fn conn() -> ConnectionInfo {
        ConnectionInfo {
            address: IpAddr::V4([127, 0, 0, 1].into()),
        }
    }

    fn session_with(media: Vec<MediaDescription>) -> SessionDescription {
        SessionDescription {
            origin: origin(),
            session_name: "audio".into(),
            connection: Some(conn()),
            groups: Vec::new(),
            ice_lite: false,
            media,
        }
    }

    fn audio_sdp() -> SessionDescription {
        let mut audio = MediaDescription::new(MediaKind::Audio, 5004, "RTP/AVP");
        audio.formats = vec!["0".into()];
        audio.rtpmap = vec![RtpMap {
            payload_type: 0,
            codec: "PCMU".into(),
            clock_rate: 8_000,
            channels: None,
        }];
        audio.ptime = Some(20);
        audio.mid = Some("0".into());
        session_with(vec![audio])
    }

    #[test]
    fn renegotiate_declines_audio_and_appends_image() {
        let next =
            fax_renegotiate(&audio_sdp(), conn(), 6250, &T38Params::sensible_offer()).unwrap();
        assert_eq!(next.media[0].kind, MediaKind::Audio);
        assert_eq!(next.media[0].port, 0);
        assert_eq!(next.media[0].direction, Direction::Inactive);
        assert!(
            next.media[0].rtpmap.is_empty(),
            "declined m-line drops rtpmap"
        );
        assert_eq!(next.media[0].ptime, None, "declined m-line drops ptime");
        assert_eq!(next.media[0].mid.as_deref(), Some("0"), "mid survives");
        assert_eq!(next.media[1].kind, MediaKind::Image);
        assert_eq!(next.media[1].port, 6250);
        assert_eq!(next.media[1].protocol, "udptl");
        assert_eq!(next.media[1].formats, ["t38"]);
    }

    #[test]
    fn renegotiate_honors_params_on_the_wire() {
        let params = T38Params {
            max_bit_rate: Some(9_600),
            ..T38Params::sensible_offer()
        };
        let next = fax_renegotiate(&audio_sdp(), conn(), 6250, &params).unwrap();
        assert_eq!(next.media[1].t38.as_ref(), Some(&params));
        let wire = next.to_string();
        assert!(wire.contains("m=audio 0 RTP/AVP 0\r\n"), "{wire}");
        assert!(wire.contains("m=image 6250 udptl t38\r\n"), "{wire}");
        assert!(wire.contains("a=T38MaxBitRate:9600\r\n"), "{wire}");
        assert!(
            wire.contains("a=T38FaxUdpEC:t38UDPRedundancy\r\n"),
            "{wire}"
        );
        let reparsed = SessionDescription::parse(&wire).unwrap();
        assert_eq!(reparsed.media[1].t38.as_ref(), Some(&params));
    }

    #[test]
    fn renegotiate_bumps_session_version() {
        let next = fax_renegotiate(&audio_sdp(), conn(), 6250, &T38Params::default()).unwrap();
        assert_eq!(next.origin.session_version, 43);
    }

    #[test]
    fn renegotiate_without_audio_is_an_error() {
        let mut video = MediaDescription::new(MediaKind::Video, 5006, "RTP/AVP");
        video.formats = vec!["96".into()];
        let no_audio = session_with(vec![video]);
        assert_eq!(
            fax_renegotiate(&no_audio, conn(), 6250, &T38Params::default()),
            Err(FaxRenegotiateError::NoAudioMLine)
        );
    }

    #[test]
    fn renegotiate_on_active_fax_is_an_error() {
        let mut active = audio_sdp();
        active.media.push(t38_media(6250, &T38Params::default()));
        assert_eq!(
            fax_renegotiate(&active, conn(), 6300, &T38Params::default()),
            Err(FaxRenegotiateError::AlreadyFax)
        );
    }
}
