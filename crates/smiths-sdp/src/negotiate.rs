//! SDP offer/answer negotiation.
//!
//! Scope: passthrough for audio (PCMU / PCMA / Opus) and video
//! (H.264 / VP8 / VP9 — slice 5.1 / P11). The negotiator intersects
//! the offered codecs with the engine's `supported` / `supported_video`
//! lists (matched by case-insensitive codec name *and* clock rate;
//! payload type numbers follow the offer to stay passthrough-friendly).
//!
//! Audio negotiation is required: an offer with no `m=audio` block
//! mismatches. Video is additive — `m=video` in the offer is
//! answered with either a matching codec on a caller-supplied port
//! or `m=video 0 ...` to decline (RFC 3264 §6 port-zero).

use std::net::{IpAddr, SocketAddr};

use rand::Rng as _;
use smiths_core::SrtpSuite;
use smiths_core::sdp::{NegotiationOutcome, SdpNegotiator, SrtpKeys};

use crate::srtp_attr::SdesCrypto;
use crate::types::{
    ConnectionInfo, MediaDescription, MediaKind, Origin, RtpMap, SessionDescription,
};

/// Generate fresh SDES key material for `suite` using the OS CSPRNG.
///
/// Returns `suite.key_material_len()` bytes (16 + 14 = 30 for the
/// one suite we currently support). Every call returns fresh entropy
/// — callers must not reuse the result across dialogs.
#[must_use]
pub fn fresh_sdes_key(suite: SrtpSuite) -> Vec<u8> {
    let mut buf = vec![0u8; suite.key_material_len()];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// Outcome of running offer/answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NegotiationResult {
    /// Offer accepted; the engine's answer is ready to send.
    Answer {
        /// Rendered answer.
        sdp: SessionDescription,
        /// SRTP keying material, when the offer was `RTP/SAVP` with a
        /// supported `a=crypto:`. `None` for plain `RTP/AVP`.
        srtp: Option<SrtpKeys>,
    },
    /// No codec in the offer intersected with the engine's supported
    /// list — caller should reply with `488 Not Acceptable Here`. MVP
    /// guardrail: a future `smiths-transcode` crate can branch on this
    /// instead of failing the call.
    Mismatch,
}

/// Engine-side negotiator.
#[derive(Clone, Debug)]
pub struct Negotiator {
    /// IP address the engine publishes in `o=` / `c=`.
    pub local_ip: IpAddr,
    /// Audio codecs the engine can pass through, in preference
    /// order. Payload type numbers are placeholders; the answer
    /// uses the offerer's PT for compatibility with passthrough
    /// B2BUAs.
    pub supported: Vec<RtpMap>,
    /// Video codecs the engine can pass through (slice 5.1 / P11).
    /// Same matching semantics as [`Self::supported`]: the answer
    /// echoes the offerer's PT so downstream B2BUAs stay happy.
    /// Passthrough only — no decode, no transcoding.
    pub supported_video: Vec<RtpMap>,
}

impl Negotiator {
    /// Build a negotiator with the canonical passthrough codec set:
    /// `PCMU` (0) @ 8 kHz, `PCMA` (8) @ 8 kHz, `opus` (111) @ 48 kHz stereo
    /// on the audio side, plus `H264` (96), `VP8` (97), `VP9` (98) at the
    /// RTP clock rate of 90 000 Hz on the video side (RFC 6184 / RFC 7741
    /// / draft-ietf-payload-vp9).
    #[must_use]
    pub fn with_default_codecs(local_ip: IpAddr) -> Self {
        Self {
            local_ip,
            supported: vec![
                RtpMap {
                    payload_type: 0,
                    codec: "PCMU".into(),
                    clock_rate: 8_000,
                    channels: None,
                },
                RtpMap {
                    payload_type: 8,
                    codec: "PCMA".into(),
                    clock_rate: 8_000,
                    channels: None,
                },
                RtpMap {
                    payload_type: 111,
                    codec: "opus".into(),
                    clock_rate: 48_000,
                    channels: Some(2),
                },
            ],
            supported_video: vec![
                RtpMap {
                    payload_type: 96,
                    codec: "H264".into(),
                    clock_rate: 90_000,
                    channels: None,
                },
                RtpMap {
                    payload_type: 97,
                    codec: "VP8".into(),
                    clock_rate: 90_000,
                    channels: None,
                },
                RtpMap {
                    payload_type: 98,
                    codec: "VP9".into(),
                    clock_rate: 90_000,
                    channels: None,
                },
            ],
        }
    }

    /// Produce an audio-only answer to `offer`, publishing
    /// `local_port` as the media port. Ignores any `m=video` /
    /// `m=application` blocks the offer carries. Use
    /// [`Self::answer_with_video`] when you want `m=video`
    /// handling (slice 5.1 / P11).
    ///
    /// When the offer uses `RTP/SAVP` with at least one supported
    /// `a=crypto:` suite, the engine generates a fresh key and emits
    /// a matching `a=crypto:` line in the answer; the returned
    /// [`NegotiationResult::Answer`] carries both halves of the SRTP
    /// key material so the caller can wire transforms on the bridge.
    /// Offers using `RTP/SAVP` **without** any supported crypto line
    /// are rejected as [`NegotiationResult::Mismatch`] — this mirrors
    /// RFC 4568 §5.1.2: a SAVP responder must not proceed unprotected.
    #[must_use]
    pub fn answer(&self, offer: &SessionDescription, local_port: u16) -> NegotiationResult {
        let Some(audio) = offer.media.iter().find(|m| m.kind == MediaKind::Audio) else {
            return NegotiationResult::Mismatch;
        };

        // Pick the first offered PT whose rtpmap matches one of our
        // supported codecs. Audio codecs with no explicit rtpmap (e.g.
        // static PCMU=0, PCMA=8) are matched by PT fallback.
        let chosen = audio.formats.iter().find_map(|pt| {
            let rtpmap = audio.rtpmap.iter().find(|r| r.payload_type == *pt);
            match rtpmap {
                Some(r) => self
                    .supported
                    .iter()
                    .find(|s| {
                        s.codec.eq_ignore_ascii_case(&r.codec) && s.clock_rate == r.clock_rate
                    })
                    .map(|_| r.clone()),
                None => self
                    .supported
                    .iter()
                    .find(|s| s.payload_type == *pt)
                    .cloned(),
            }
        });

        let Some(chosen) = chosen else {
            return NegotiationResult::Mismatch;
        };

        // --- SDES handling -------------------------------------------
        //
        // RFC 4568 §5.1: when the offer's transport is `RTP/SAVP`
        // (or an `RTP/SAVP`-equivalent profile), the answer **must**
        // be `RTP/SAVP` and **must** include an `a=crypto:` matching
        // one of the offered tags. Transports without SAVP ignore any
        // stray `a=crypto:` lines.
        let is_savp = audio.protocol.eq_ignore_ascii_case("RTP/SAVP");
        let (answer_crypto, srtp_keys) = if is_savp {
            // First offer line whose suite we support wins. `SdesCrypto::parse`
            // already rejects suites we don't know, so every parsed
            // entry is already a candidate.
            let Some(offer_crypto) = audio.crypto.first() else {
                // SAVP without any acceptable crypto → 488 per §5.1.2.
                return NegotiationResult::Mismatch;
            };
            let suite = offer_crypto.suite;
            let local_km = fresh_sdes_key(suite);
            let answer_line = SdesCrypto {
                tag: offer_crypto.tag,
                suite,
                key_material: local_km.clone(),
            };
            let keys = SrtpKeys {
                suite,
                peer_tx_key: offer_crypto.key_material.clone(),
                local_tx_key: local_km,
            };
            (vec![answer_line], Some(keys))
        } else {
            (Vec::new(), None)
        };

        let answer_media = MediaDescription {
            kind: MediaKind::Audio,
            port: local_port,
            protocol: audio.protocol.clone(),
            formats: vec![chosen.payload_type],
            rtpmap: vec![chosen],
            crypto: answer_crypto,
            direction: audio.direction.reverse(),
            connection: None,
            // Engine doesn't emit DTLS-SRTP / ICE attrs yet — slice 1.3
            // wires fingerprint + setup; 1.4 wires ICE.
            fingerprint: None,
            setup: None,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: Vec::new(),
            candidates: Vec::new(),
            end_of_candidates: false,
        };

        NegotiationResult::Answer {
            sdp: SessionDescription {
                origin: Origin {
                    username: "smiths".into(),
                    // Session-id / version: use wall-clock seconds; the
                    // answerer is free to pick.
                    session_id: unix_seconds(),
                    session_version: unix_seconds(),
                    address: self.local_ip,
                },
                session_name: "smiths-net".into(),
                connection: Some(ConnectionInfo {
                    address: self.local_ip,
                }),
                media: vec![answer_media],
            },
            srtp: srtp_keys,
        }
    }

    /// Multi-stream answer (slice 5.1 / P11). Produces the same
    /// shape as [`Self::answer`] on the audio side, then appends a
    /// matching `m=video` block to preserve m-line ordering.
    ///
    /// Video-handling rules:
    /// - Offer has no `m=video` → result identical to
    ///   [`Self::answer`] (audio-only).
    /// - Offer has `m=video` **and** `local_video_port` is
    ///   `Some(port)` **and** at least one offered codec is in
    ///   [`Self::supported_video`] → answer echoes the chosen codec
    ///   on the given port.
    /// - Offer has `m=video` but either port is `None` or no codec
    ///   matches → answer carries `m=video 0 ...` (RFC 3264 §8.2 /
    ///   §6 — port 0 declines a stream while keeping m-line
    ///   alignment). Audio still negotiates normally.
    ///
    /// Never fails the whole negotiation on a video-only issue —
    /// declining video is a normal SDP move, not a reason to 488
    /// the call.
    #[must_use]
    pub fn answer_with_video(
        &self,
        offer: &SessionDescription,
        local_audio_port: u16,
        local_video_port: Option<u16>,
    ) -> NegotiationResult {
        let audio_result = self.answer(offer, local_audio_port);
        let NegotiationResult::Answer {
            mut sdp,
            srtp: audio_srtp,
        } = audio_result
        else {
            return audio_result;
        };
        let Some(offer_video) = offer.media.iter().find(|m| m.kind == MediaKind::Video) else {
            return NegotiationResult::Answer {
                sdp,
                srtp: audio_srtp,
            };
        };
        // Pick a passthrough codec; same match semantics as audio.
        let chosen_video = offer_video.formats.iter().find_map(|pt| {
            let rtpmap = offer_video.rtpmap.iter().find(|r| r.payload_type == *pt);
            match rtpmap {
                Some(r) => self
                    .supported_video
                    .iter()
                    .find(|s| {
                        s.codec.eq_ignore_ascii_case(&r.codec) && s.clock_rate == r.clock_rate
                    })
                    .map(|_| r.clone()),
                None => self
                    .supported_video
                    .iter()
                    .find(|s| s.payload_type == *pt)
                    .cloned(),
            }
        });

        let (video_port, video_formats, video_rtpmap) = match (local_video_port, chosen_video) {
            (Some(port), Some(chosen)) => (port, vec![chosen.payload_type], vec![chosen]),
            // Decline path: port 0 + keep the offer's formats so
            // parsers that demand non-empty format lists stay
            // happy. Empty rtpmap list means "we're not describing
            // anything" which is fine for a declined stream.
            _ => (0, offer_video.formats.clone(), Vec::new()),
        };

        let video_answer = MediaDescription {
            kind: MediaKind::Video,
            port: video_port,
            protocol: offer_video.protocol.clone(),
            formats: video_formats,
            rtpmap: video_rtpmap,
            crypto: Vec::new(),
            direction: offer_video.direction.reverse(),
            connection: None,
            fingerprint: None,
            setup: None,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: Vec::new(),
            candidates: Vec::new(),
            end_of_candidates: false,
        };
        sdp.media.push(video_answer);
        NegotiationResult::Answer {
            sdp,
            srtp: audio_srtp,
        }
    }
}

impl SdpNegotiator for Negotiator {
    fn negotiate_audio(
        &self,
        offer_body: &str,
        local_ip: IpAddr,
        local_rtp_port: u16,
    ) -> NegotiationOutcome {
        let offer = match SessionDescription::parse(offer_body) {
            Ok(o) => o,
            Err(e) => return NegotiationOutcome::Malformed(e.to_string()),
        };
        // DTLS-SRTP gate: the WebRTC transport profile (RFC 5764 §8 —
        // `UDP/TLS/RTP/SAVP[F]`) parses fine and we recognize the
        // fingerprint/setup/ICE surface, but the handshake itself
        // lands in slice 1.3. Until then, reject with a descriptive
        // reason so peers don't see a silent 488 / 408.
        if offer
            .media
            .iter()
            .any(|m| is_dtls_srtp_profile(&m.protocol))
        {
            return NegotiationOutcome::UnsupportedTransport {
                reason: "DTLS-SRTP not yet supported".into(),
            };
        }
        let remote_media = first_audio_endpoint(&offer);
        // Per-call override: always honor the caller's `local_ip` over
        // whatever the negotiator was seeded with.
        let mut scoped = self.clone();
        scoped.local_ip = local_ip;
        match scoped.answer(&offer, local_rtp_port) {
            NegotiationResult::Answer { sdp, srtp } => {
                let audio_codec = first_codec_of_kind(&sdp, &MediaKind::Audio);
                NegotiationOutcome::Accepted {
                    answer_body: sdp.to_string(),
                    remote_media,
                    // `negotiate_audio` is the audio-only path; video
                    // handling lives on the `negotiate` override below.
                    video_media: None,
                    srtp,
                    audio_codec,
                    video_codec: None,
                }
            }
            NegotiationResult::Mismatch => NegotiationOutcome::Mismatch,
        }
    }

    fn negotiate(
        &self,
        offer_body: &str,
        local_ip: IpAddr,
        local_audio_port: u16,
        local_video_port: Option<u16>,
    ) -> NegotiationOutcome {
        let offer = match SessionDescription::parse(offer_body) {
            Ok(o) => o,
            Err(e) => return NegotiationOutcome::Malformed(e.to_string()),
        };
        if offer
            .media
            .iter()
            .any(|m| is_dtls_srtp_profile(&m.protocol))
        {
            return NegotiationOutcome::UnsupportedTransport {
                reason: "DTLS-SRTP not yet supported".into(),
            };
        }
        let remote_media = first_audio_endpoint(&offer);
        let video_media = first_video_endpoint(&offer);
        let mut scoped = self.clone();
        scoped.local_ip = local_ip;
        match scoped.answer_with_video(&offer, local_audio_port, local_video_port) {
            NegotiationResult::Answer { sdp, srtp } => {
                let audio_codec = first_codec_of_kind(&sdp, &MediaKind::Audio);
                let video_codec = first_codec_of_kind(&sdp, &MediaKind::Video);
                NegotiationOutcome::Accepted {
                    answer_body: sdp.to_string(),
                    remote_media,
                    video_media,
                    srtp,
                    audio_codec,
                    video_codec,
                }
            }
            NegotiationResult::Mismatch => NegotiationOutcome::Mismatch,
        }
    }

    fn build_offer(&self, local_ip: IpAddr, local_rtp_port: u16) -> String {
        let formats: Vec<u8> = self.supported.iter().map(|c| c.payload_type).collect();
        let rtpmaps: Vec<RtpMap> = self.supported.clone();
        let sdp = SessionDescription {
            origin: Origin {
                username: "smiths".into(),
                session_id: unix_seconds(),
                session_version: unix_seconds(),
                address: local_ip,
            },
            session_name: "smiths-net".into(),
            connection: Some(ConnectionInfo { address: local_ip }),
            media: vec![MediaDescription {
                kind: MediaKind::Audio,
                port: local_rtp_port,
                protocol: "RTP/AVP".into(),
                formats,
                rtpmap: rtpmaps,
                crypto: Vec::new(),
                direction: crate::Direction::SendRecv,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: Vec::new(),
                candidates: Vec::new(),
                end_of_candidates: false,
            }],
        };
        sdp.to_string()
    }

    fn parse_remote_rtp(&self, answer_body: &str) -> Option<SocketAddr> {
        let sdp = SessionDescription::parse(answer_body).ok()?;
        first_audio_endpoint(&sdp)
    }
}

/// Extract the first audio RTP endpoint from a parsed offer.
///
/// `None` when there is no `m=audio`, the port is 0 (hold), or there
/// is no connection line at either media or session level.
fn first_audio_endpoint(sdp: &SessionDescription) -> Option<SocketAddr> {
    let audio = sdp.media.iter().find(|m| m.kind == MediaKind::Audio)?;
    if audio.port == 0 {
        return None;
    }
    let conn = audio.connection.as_ref().or(sdp.connection.as_ref())?;
    Some(SocketAddr::new(conn.address, audio.port))
}

/// Extract the first video RTP endpoint from a parsed offer.
/// Same rules as [`first_audio_endpoint`] — `None` on missing
/// `m=video`, port 0 (declined / held), or no connection line.
fn first_video_endpoint(sdp: &SessionDescription) -> Option<SocketAddr> {
    let video = sdp.media.iter().find(|m| m.kind == MediaKind::Video)?;
    if video.port == 0 {
        return None;
    }
    let conn = video.connection.as_ref().or(sdp.connection.as_ref())?;
    Some(SocketAddr::new(conn.address, video.port))
}

/// First codec on a given media kind's answer block (slice 5.6).
///
/// Returns the codec that landed in the answer's `a=rtpmap:` entry
/// for the requested `kind`. Produces `None` when the media block
/// is absent, was declined (port 0 ⇒ no rtpmap emitted), or
/// exists only as a transport-line placeholder. The UAS reads this
/// into the `DialogRecord`'s `per_leg_codec` map — the two legs'
/// codecs compare equal iff the call can run passthrough.
fn first_codec_of_kind(
    sdp: &SessionDescription,
    kind: &MediaKind,
) -> Option<smiths_core::NegotiatedCodec> {
    let m = sdp.media.iter().find(|m| &m.kind == kind)?;
    if m.port == 0 {
        return None;
    }
    let token = m.rtpmap.first().map(|r| r.codec.as_str())?;
    Some(smiths_core::NegotiatedCodec::parse(token))
}

/// `true` if the media profile names DTLS-SRTP (RFC 5764 §8).
///
/// Matches `UDP/TLS/RTP/SAVP` and `UDP/TLS/RTP/SAVPF`
/// case-insensitively. Everything else — `RTP/AVP`, `RTP/SAVP` (SDES)
/// — returns false and goes through normal offer/answer.
fn is_dtls_srtp_profile(protocol: &str) -> bool {
    let upper = protocol.to_ascii_uppercase();
    upper == "UDP/TLS/RTP/SAVP" || upper == "UDP/TLS/RTP/SAVPF"
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use crate::Direction;

    fn offer_with(formats: Vec<u8>, rtpmaps: Vec<RtpMap>) -> SessionDescription {
        SessionDescription {
            origin: Origin {
                username: "alice".into(),
                session_id: 1,
                session_version: 1,
                address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 101)),
            },
            session_name: "-".into(),
            connection: Some(ConnectionInfo {
                address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 101)),
            }),
            media: vec![MediaDescription {
                kind: MediaKind::Audio,
                port: 49_170,
                protocol: "RTP/AVP".into(),
                formats,
                rtpmap: rtpmaps,
                crypto: Vec::new(),
                direction: Direction::SendRecv,
                connection: None,
                fingerprint: None,
                setup: None,
                ice_ufrag: None,
                ice_pwd: None,
                ice_options: Vec::new(),
                candidates: Vec::new(),
                end_of_candidates: false,
            }],
        }
    }

    fn savp_offer_with_crypto(tag: u32) -> SessionDescription {
        let mut offer = offer_with(
            vec![0],
            vec![RtpMap {
                payload_type: 0,
                codec: "PCMU".into(),
                clock_rate: 8_000,
                channels: None,
            }],
        );
        offer.media[0].protocol = "RTP/SAVP".into();
        offer.media[0].crypto = vec![SdesCrypto {
            tag,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key_material: (0..30u8).collect(),
        }];
        offer
    }

    #[test]
    fn picks_first_common_codec() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(
            vec![111, 0, 8],
            vec![
                RtpMap {
                    payload_type: 111,
                    codec: "opus".into(),
                    clock_rate: 48_000,
                    channels: Some(2),
                },
                RtpMap {
                    payload_type: 0,
                    codec: "PCMU".into(),
                    clock_rate: 8_000,
                    channels: None,
                },
                RtpMap {
                    payload_type: 8,
                    codec: "PCMA".into(),
                    clock_rate: 8_000,
                    channels: None,
                },
            ],
        );
        let NegotiationResult::Answer { sdp: answer, srtp } = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(answer.media.len(), 1);
        assert_eq!(answer.media[0].formats, vec![111]);
        assert_eq!(answer.media[0].port, 16_384);
        // Offer was sendrecv → answer is sendrecv.
        assert_eq!(answer.media[0].direction, Direction::SendRecv);
        // Plain RTP/AVP: no SRTP.
        assert!(srtp.is_none());
    }

    #[test]
    fn static_pt_without_rtpmap_is_recognized() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        // Old-style offer: just `m=audio ... 0` with no rtpmap.
        let offer = offer_with(vec![0], vec![]);
        let NegotiationResult::Answer { sdp: answer, .. } = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(answer.media[0].formats, vec![0]);
    }

    #[test]
    fn reverses_direction() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut offer = offer_with(
            vec![0],
            vec![RtpMap {
                payload_type: 0,
                codec: "PCMU".into(),
                clock_rate: 8_000,
                channels: None,
            }],
        );
        offer.media[0].direction = Direction::SendOnly;
        let NegotiationResult::Answer { sdp: answer, .. } = neg.answer(&offer, 1_234) else {
            panic!();
        };
        assert_eq!(answer.media[0].direction, Direction::RecvOnly);
    }

    #[test]
    fn savp_offer_gets_savp_answer_with_matching_crypto_tag() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = savp_offer_with_crypto(42);
        let NegotiationResult::Answer { sdp: answer, srtp } = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(answer.media[0].protocol, "RTP/SAVP");
        assert_eq!(answer.media[0].crypto.len(), 1);
        assert_eq!(answer.media[0].crypto[0].tag, 42);
        assert_eq!(
            answer.media[0].crypto[0].suite,
            SrtpSuite::AesCm128HmacSha1_80
        );
        assert_eq!(
            answer.media[0].crypto[0].key_material.len(),
            SrtpSuite::AesCm128HmacSha1_80.key_material_len()
        );

        let keys = srtp.expect("SAVP offer must yield srtp keys");
        assert_eq!(keys.suite, SrtpSuite::AesCm128HmacSha1_80);
        // Peer key in the offer was 0..30; engine's local key is fresh random.
        assert_eq!(keys.peer_tx_key, (0..30u8).collect::<Vec<u8>>());
        assert_eq!(
            keys.local_tx_key.len(),
            SrtpSuite::AesCm128HmacSha1_80.key_material_len()
        );
        assert_ne!(
            keys.local_tx_key, keys.peer_tx_key,
            "engine must not echo the peer's key"
        );
    }

    #[test]
    fn savp_offer_without_crypto_is_mismatch() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut offer = savp_offer_with_crypto(1);
        offer.media[0].crypto.clear();
        assert_eq!(neg.answer(&offer, 16_384), NegotiationResult::Mismatch);
    }

    #[test]
    fn answer_body_serializes_crypto_line() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = savp_offer_with_crypto(7);
        let NegotiationResult::Answer { sdp: answer, .. } = neg.answer(&offer, 16_384) else {
            panic!();
        };
        let body = answer.to_string();
        assert!(body.contains("RTP/SAVP"), "answer protocol = SAVP");
        assert!(
            body.contains("a=crypto:7 AES_CM_128_HMAC_SHA1_80 inline:"),
            "answer must carry engine's crypto line, got:\n{body}"
        );
    }

    #[test]
    fn fresh_sdes_key_has_suite_length_and_varies() {
        let k1 = fresh_sdes_key(SrtpSuite::AesCm128HmacSha1_80);
        let k2 = fresh_sdes_key(SrtpSuite::AesCm128HmacSha1_80);
        assert_eq!(k1.len(), SrtpSuite::AesCm128HmacSha1_80.key_material_len());
        assert_eq!(k2.len(), k1.len());
        // Collision is astronomically unlikely for 30 random bytes.
        assert_ne!(k1, k2);
    }

    #[test]
    fn unknown_codec_yields_mismatch() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(
            vec![96],
            vec![RtpMap {
                payload_type: 96,
                codec: "telephone-event".into(),
                clock_rate: 8_000,
                channels: None,
            }],
        );
        assert_eq!(neg.answer(&offer, 1_234), NegotiationResult::Mismatch);
    }

    // -----------------------------------------------------------------
    // Slice 5.1 / P11 — video passthrough
    // -----------------------------------------------------------------

    fn audio_video_offer(audio_port: u16, video_port: u16) -> SessionDescription {
        let mut sdp = offer_with(
            vec![0],
            vec![RtpMap {
                payload_type: 0,
                codec: "PCMU".into(),
                clock_rate: 8_000,
                channels: None,
            }],
        );
        sdp.media[0].port = audio_port;
        sdp.media.push(MediaDescription {
            kind: MediaKind::Video,
            port: video_port,
            protocol: "RTP/AVP".into(),
            formats: vec![96, 97],
            rtpmap: vec![
                RtpMap {
                    payload_type: 96,
                    codec: "H264".into(),
                    clock_rate: 90_000,
                    channels: None,
                },
                RtpMap {
                    payload_type: 97,
                    codec: "VP8".into(),
                    clock_rate: 90_000,
                    channels: None,
                },
            ],
            crypto: Vec::new(),
            direction: Direction::SendRecv,
            connection: None,
            fingerprint: None,
            setup: None,
            ice_ufrag: None,
            ice_pwd: None,
            ice_options: Vec::new(),
            candidates: Vec::new(),
            end_of_candidates: false,
        });
        sdp
    }

    #[test]
    fn answer_with_video_echoes_first_supported_codec() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let NegotiationResult::Answer { sdp, .. } =
            neg.answer_with_video(&offer, 16_384, Some(16_386))
        else {
            panic!("expected Answer");
        };
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[0].kind, MediaKind::Audio);
        assert_eq!(sdp.media[0].port, 16_384);
        assert_eq!(sdp.media[1].kind, MediaKind::Video);
        assert_eq!(sdp.media[1].port, 16_386);
        // H.264 was first in the offer's format list; it wins.
        assert_eq!(sdp.media[1].formats, vec![96]);
        assert_eq!(sdp.media[1].rtpmap.len(), 1);
        assert_eq!(sdp.media[1].rtpmap[0].codec, "H264");
    }

    #[test]
    fn answer_with_video_port_none_declines_with_port_zero() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let NegotiationResult::Answer { sdp, .. } = neg.answer_with_video(&offer, 16_384, None)
        else {
            panic!("expected Answer");
        };
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[1].kind, MediaKind::Video);
        assert_eq!(sdp.media[1].port, 0, "declined video must use port 0");
        // Audio still negotiates normally.
        assert_eq!(sdp.media[0].port, 16_384);
    }

    #[test]
    fn answer_with_video_unknown_codec_declines_but_keeps_audio() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        // AV1 isn't in the passthrough set.
        let mut offer = audio_video_offer(49_170, 49_172);
        offer.media[1].formats = vec![100];
        offer.media[1].rtpmap = vec![RtpMap {
            payload_type: 100,
            codec: "AV1".into(),
            clock_rate: 90_000,
            channels: None,
        }];
        let NegotiationResult::Answer { sdp, .. } =
            neg.answer_with_video(&offer, 16_384, Some(16_386))
        else {
            panic!("expected Answer");
        };
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[1].port, 0);
        // Audio untouched.
        assert_eq!(sdp.media[0].port, 16_384);
    }

    #[test]
    fn answer_with_video_is_audio_only_when_offer_has_no_video() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(
            vec![0],
            vec![RtpMap {
                payload_type: 0,
                codec: "PCMU".into(),
                clock_rate: 8_000,
                channels: None,
            }],
        );
        let NegotiationResult::Answer { sdp, .. } =
            neg.answer_with_video(&offer, 16_384, Some(16_386))
        else {
            panic!("expected Answer");
        };
        assert_eq!(sdp.media.len(), 1);
        assert_eq!(sdp.media[0].kind, MediaKind::Audio);
    }

    #[test]
    fn negotiate_outcome_populates_video_media_when_peer_offers_video() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let body = offer.to_string();
        let outcome = neg.negotiate(&body, IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384, Some(16_386));
        let smiths_core::NegotiationOutcome::Accepted {
            remote_media,
            video_media,
            answer_body,
            ..
        } = outcome
        else {
            panic!("expected Accepted");
        };
        let audio_peer = remote_media.expect("audio peer");
        let video_peer = video_media.expect("video peer");
        assert_eq!(audio_peer.port(), 49_170);
        assert_eq!(video_peer.port(), 49_172);
        assert_eq!(audio_peer.ip(), video_peer.ip());
        assert!(answer_body.contains("m=audio 16384"));
        assert!(answer_body.contains("m=video 16386"));
        assert!(answer_body.contains("H264"));
    }

    #[test]
    fn negotiate_declines_video_cleanly_when_no_video_port() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let body = offer.to_string();
        let outcome = neg.negotiate(&body, IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384, None);
        let smiths_core::NegotiationOutcome::Accepted {
            video_media,
            answer_body,
            ..
        } = outcome
        else {
            panic!("expected Accepted");
        };
        // Peer still offered video, so we expose their endpoint even
        // though we're declining — consumers can opt in later without
        // re-negotiating.
        assert_eq!(video_media.unwrap().port(), 49_172);
        assert!(answer_body.contains("m=video 0"));
    }

    #[test]
    fn negotiate_audio_backcompat_path_leaves_video_media_none() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let body = offer.to_string();
        // Via the older audio-only trait method.
        let outcome = neg.negotiate_audio(&body, IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384);
        let smiths_core::NegotiationOutcome::Accepted { video_media, .. } = outcome else {
            panic!("expected Accepted");
        };
        assert!(
            video_media.is_none(),
            "negotiate_audio must not expose video endpoint"
        );
    }

    // ---- Slice 5.6: codec detection on the accepted outcome ----

    #[test]
    fn negotiate_audio_exposes_pcmu_on_pcmu_offer() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(
            vec![0],
            vec![RtpMap {
                payload_type: 0,
                codec: "PCMU".into(),
                clock_rate: 8_000,
                channels: None,
            }],
        );
        let outcome =
            neg.negotiate_audio(&offer.to_string(), IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384);
        let smiths_core::NegotiationOutcome::Accepted {
            audio_codec,
            video_codec,
            ..
        } = outcome
        else {
            panic!("expected Accepted");
        };
        assert_eq!(audio_codec, Some(smiths_core::NegotiatedCodec::Pcmu));
        assert_eq!(
            video_codec, None,
            "audio-only path must not claim a video codec"
        );
    }

    #[test]
    fn negotiate_exposes_both_codecs_on_audio_plus_video_offer() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let outcome = neg.negotiate(
            &offer.to_string(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            16_384,
            Some(16_386),
        );
        let smiths_core::NegotiationOutcome::Accepted {
            audio_codec,
            video_codec,
            ..
        } = outcome
        else {
            panic!("expected Accepted");
        };
        assert_eq!(audio_codec, Some(smiths_core::NegotiatedCodec::Pcmu));
        assert_eq!(video_codec, Some(smiths_core::NegotiatedCodec::H264));
    }

    #[test]
    fn negotiate_declined_video_leaves_video_codec_none() {
        // Video offered, negotiator declines via `video_port = None`
        // → answer carries `m=video 0 ...`, so video_codec must be
        // None (we didn't accept any codec).
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let outcome = neg.negotiate(
            &offer.to_string(),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            16_384,
            None,
        );
        let smiths_core::NegotiationOutcome::Accepted {
            audio_codec,
            video_codec,
            ..
        } = outcome
        else {
            panic!("expected Accepted");
        };
        assert_eq!(audio_codec, Some(smiths_core::NegotiatedCodec::Pcmu));
        assert_eq!(video_codec, None);
    }
}
