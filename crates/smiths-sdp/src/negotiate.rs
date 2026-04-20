//! SDP offer/answer negotiation.
//!
//! Scope: audio passthrough with PCMU / PCMA / Opus. The negotiator
//! intersects the offered codecs with the engine's `supported` list
//! (matched by case-insensitive codec name *and* clock rate; payload
//! type numbers follow the offer to stay passthrough-friendly).

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
    /// Codecs the engine can pass through, in preference order.
    /// Payload type numbers are placeholders; the answer uses the
    /// offerer's PT for compatibility with passthrough B2BUAs.
    pub supported: Vec<RtpMap>,
}

impl Negotiator {
    /// Build a negotiator with the canonical passthrough codec set:
    /// `PCMU` (0) @ 8 kHz, `PCMA` (8) @ 8 kHz, `opus` (111) @ 48 kHz stereo.
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
        }
    }

    /// Produce an answer to `offer`, publishing `local_port` as the
    /// media port.
    ///
    /// Currently only the first `m=audio` block is negotiated; other
    /// media lines (video, application) are not acknowledged — extend
    /// here once the Call FSM carries m-line lists.
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
        let remote_media = first_audio_endpoint(&offer);
        // Per-call override: always honor the caller's `local_ip` over
        // whatever the negotiator was seeded with.
        let mut scoped = self.clone();
        scoped.local_ip = local_ip;
        match scoped.answer(&offer, local_rtp_port) {
            NegotiationResult::Answer { sdp, srtp } => NegotiationOutcome::Accepted {
                answer_body: sdp.to_string(),
                remote_media,
                srtp,
            },
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
}
