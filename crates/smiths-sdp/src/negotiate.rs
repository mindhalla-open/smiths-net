//! SDP offer/answer negotiation.
//!
//! Scope: audio passthrough with PCMU / PCMA / Opus. The negotiator
//! intersects the offered codecs with the engine's `supported` list
//! (matched by case-insensitive codec name *and* clock rate; payload
//! type numbers follow the offer to stay passthrough-friendly).

use std::net::IpAddr;

use crate::types::{
    ConnectionInfo, MediaDescription, MediaKind, Origin, RtpMap, SessionDescription,
};

/// Outcome of running offer/answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NegotiationResult {
    /// Offer accepted; the engine's answer is ready to send.
    Answer(SessionDescription),
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

        let answer_media = MediaDescription {
            kind: MediaKind::Audio,
            port: local_port,
            protocol: audio.protocol.clone(),
            formats: vec![chosen.payload_type],
            rtpmap: vec![chosen],
            direction: audio.direction.reverse(),
            connection: None,
        };

        NegotiationResult::Answer(SessionDescription {
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
        })
    }
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
                direction: Direction::SendRecv,
                connection: None,
            }],
        }
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
        let NegotiationResult::Answer(answer) = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(answer.media.len(), 1);
        assert_eq!(answer.media[0].formats, vec![111]);
        assert_eq!(answer.media[0].port, 16_384);
        // Offer was sendrecv → answer is sendrecv.
        assert_eq!(answer.media[0].direction, Direction::SendRecv);
    }

    #[test]
    fn static_pt_without_rtpmap_is_recognized() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        // Old-style offer: just `m=audio ... 0` with no rtpmap.
        let offer = offer_with(vec![0], vec![]);
        let NegotiationResult::Answer(answer) = neg.answer(&offer, 16_384) else {
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
        let NegotiationResult::Answer(answer) = neg.answer(&offer, 1_234) else {
            panic!();
        };
        assert_eq!(answer.media[0].direction, Direction::RecvOnly);
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
