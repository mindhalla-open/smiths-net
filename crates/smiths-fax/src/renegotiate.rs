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

use smiths_sdp::{ConnectionInfo, Direction, MediaDescription, MediaKind, SessionDescription};

use crate::sdp::{T38_FORMAT, T38Params, UDPTL_PROTOCOL};

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
/// address for this call).
///
/// On success the returned SDP has:
/// - The original audio m-line rewritten with `port=0`,
///   `direction=inactive`.
/// - A fresh `m=image <port> udptl t38` block appended.
/// - The session-level `c=` line updated to `local_addr`.
/// - `origin.session_version` bumped — RFC 3264 §5 requires the
///   answerer treat the SDP as changed.
///
/// The renderer [`crate::sdp::render_offer_with_params`] will splice
/// in the `a=T38…` attribute lines when the SDP is serialized onto
/// the wire.
///
/// # Errors
/// [`FaxRenegotiateError`] — see variant docs.
pub fn fax_renegotiate(
    active: &SessionDescription,
    local_addr: ConnectionInfo,
    local_fax_port: u16,
    _params: &T38Params,
) -> Result<SessionDescription, FaxRenegotiateError> {
    if active.media.iter().any(crate::sdp::is_t38_media) {
        return Err(FaxRenegotiateError::AlreadyFax);
    }
    let audio_idx = active
        .media
        .iter()
        .position(|m| m.kind == MediaKind::Audio)
        .ok_or(FaxRenegotiateError::NoAudioMLine)?;

    let mut media: Vec<MediaDescription> = active.media.clone();
    // Decline the audio m-line per RFC 3264 §6: same kind + protocol,
    // port 0, direction=inactive, strip every attribute.
    let audio = &mut media[audio_idx];
    audio.port = 0;
    audio.direction = Direction::Inactive;
    audio.rtpmap.clear();
    audio.crypto.clear();
    audio.connection = None;
    audio.fingerprint = None;
    audio.setup = None;
    audio.ice_ufrag = None;
    audio.ice_pwd = None;
    audio.ice_options.clear();
    audio.candidates.clear();
    audio.end_of_candidates = false;

    // Append the fax block.
    media.push(MediaDescription {
        kind: MediaKind::Image,
        port: local_fax_port,
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
    });

    let mut next = active.clone();
    next.connection = Some(local_addr);
    next.origin.session_version = active.origin.session_version.saturating_add(1);
    next.media = media;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_sdp::Origin;
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

    fn audio_sdp() -> SessionDescription {
        SessionDescription {
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
        }
    }

    #[test]
    fn renegotiate_declines_audio_and_appends_image() {
        let next =
            fax_renegotiate(&audio_sdp(), conn(), 6250, &T38Params::sensible_offer()).unwrap();
        assert_eq!(next.media[0].kind, MediaKind::Audio);
        assert_eq!(next.media[0].port, 0);
        assert_eq!(next.media[0].direction, Direction::Inactive);
        assert_eq!(next.media[1].kind, MediaKind::Image);
        assert_eq!(next.media[1].port, 6250);
        assert!(next.media[1].protocol.contains("udptl"));
    }

    #[test]
    fn renegotiate_bumps_session_version() {
        let next = fax_renegotiate(&audio_sdp(), conn(), 6250, &T38Params::default()).unwrap();
        assert_eq!(next.origin.session_version, 43);
    }

    #[test]
    fn renegotiate_without_audio_is_an_error() {
        let no_audio = SessionDescription {
            origin: origin(),
            session_name: "video".into(),
            connection: Some(conn()),
            media: vec![MediaDescription {
                kind: MediaKind::Video,
                port: 5006,
                protocol: "RTP/AVP".into(),
                formats: vec![96],
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
        assert_eq!(
            fax_renegotiate(&no_audio, conn(), 6250, &T38Params::default()),
            Err(FaxRenegotiateError::NoAudioMLine)
        );
    }

    #[test]
    fn renegotiate_on_active_fax_is_an_error() {
        let mut active = audio_sdp();
        active.media.push(MediaDescription {
            kind: MediaKind::Image,
            port: 6250,
            protocol: "udptl t38".into(),
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
        assert_eq!(
            fax_renegotiate(&active, conn(), 6300, &T38Params::default()),
            Err(FaxRenegotiateError::AlreadyFax)
        );
    }
}
