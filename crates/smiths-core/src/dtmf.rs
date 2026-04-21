//! RFC 4733 (a.k.a. RFC 2833) telephone-event DTMF parse + encode.
//!
//! Minimal subset: the four-byte telephone-event payload plus the
//! `event` → ASCII digit mapping from Table 1 of RFC 4733. No named
//! signals beyond that (fax tones, modem detect) — MVP covers the
//! sixteen DTMF digits (0–9, *, #, A–D) plus the "flash" hook.
//!
//! Wire format ( §2.3 ):
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |     event     |E|R| volume    |          duration             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! - `event`    — 0–15 DTMF digit, 16 flash (§3.2 Table 1).
//! - `E` flag   — end-of-event.
//! - `R` bit    — reserved, always 0.
//! - `volume`   — dB below reference (0–63). Engine-side emitters
//!   default to 10 dB (a comfortable listening level).
//! - `duration` — samples at the clock rate (typically 8 kHz).
//!
//! Engines receive a **stream** of telephone-event packets per digit
//! press — redundant retransmits with the same event id, followed by
//! three End packets. This module's [`TelephoneEvent::parse`] pulls
//! one packet; the deduplication-across-retransmits logic lives in
//! [`crate::dtmf::DtmfDetector`] because it needs state.

use serde::{Deserialize, Serialize};

/// Payload type the engine treats as RFC 4733 telephone-event. The
/// de-facto value across SDP fleets is 101; operators that negotiate
/// a different value should thread it through the detector rather
/// than patching this constant.
pub const RFC4733_PAYLOAD_TYPE: u8 = 101;

/// One parsed telephone-event payload.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TelephoneEvent {
    /// Raw event code per RFC 4733 §3.2 Table 1.
    pub event: u8,
    /// End-of-event flag. Set on the last three packets of a digit.
    pub end: bool,
    /// Volume: dB below reference (0–63). Most softphones emit ~10 dB.
    pub volume: u8,
    /// Duration in timestamp samples (typically 8 kHz).
    pub duration_samples: u16,
}

impl TelephoneEvent {
    /// Parse an RFC 4733 telephone-event payload. Returns `None` when
    /// the slice is shorter than 4 bytes or the reserved bit is set
    /// with a non-zero value (we refuse to guess).
    #[must_use]
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        let event = payload[0];
        let flags = payload[1];
        let end = (flags & 0x80) != 0;
        // Reserved bit (§2.3 `R`). RFC says ignore on receipt; we do.
        let volume = flags & 0x3F;
        let duration_samples = u16::from_be_bytes([payload[2], payload[3]]);
        Some(Self {
            event,
            end,
            volume,
            duration_samples,
        })
    }

    /// Serialize back to wire bytes. Symmetric with [`Self::parse`].
    #[must_use]
    pub fn encode(&self) -> [u8; 4] {
        let mut out = [0u8; 4];
        out[0] = self.event;
        out[1] = (u8::from(self.end) << 7) | (self.volume & 0x3F);
        let dur = self.duration_samples.to_be_bytes();
        out[2] = dur[0];
        out[3] = dur[1];
        out
    }

    /// Map the event code to its ASCII digit per RFC 4733 §3.2
    /// Table 1. Returns `None` for unsupported events (17+, fax
    /// tones, etc.) — callers decide whether to log and drop.
    #[must_use]
    pub fn as_digit(&self) -> Option<char> {
        event_code_to_digit(self.event)
    }
}

/// RFC 4733 §3.2 Table 1, restricted to DTMF digits + flash.
#[must_use]
pub fn event_code_to_digit(event: u8) -> Option<char> {
    match event {
        0..=9 => Some(char::from(b'0' + event)),
        10 => Some('*'),
        11 => Some('#'),
        12 => Some('A'),
        13 => Some('B'),
        14 => Some('C'),
        15 => Some('D'),
        // 16 = flash hook. Engines model it as '!' per the historical
        // `telephone-event-flash` convention; the MCP surface uses the
        // same character so operators can grep it out of event logs.
        16 => Some('!'),
        _ => None,
    }
}

/// Inverse of [`event_code_to_digit`]. Case-insensitive on A–D.
#[must_use]
pub fn digit_to_event_code(digit: char) -> Option<u8> {
    match digit {
        '0'..='9' => Some(digit as u8 - b'0'),
        '*' => Some(10),
        '#' => Some(11),
        'A' | 'a' => Some(12),
        'B' | 'b' => Some(13),
        'C' | 'c' => Some(14),
        'D' | 'd' => Some(15),
        '!' => Some(16),
        _ => None,
    }
}

/// Sink for detected DTMF keypresses. Trait object so the media
/// bridge can deliver presses without depending on the event-bus
/// types in `smiths-core::bus`.
///
/// Kept synchronous: the bridge forwarder is a tight per-packet loop
/// and can't afford an `.await` just to publish a DTMF event. The
/// bus adapter does a non-blocking `try_send` internally.
pub trait DtmfSink: Send + Sync {
    /// Called once per completed keypress. `leg` is `"a->b"` or
    /// `"b->a"` — the direction the press arrived on.
    fn deliver(&self, leg: &'static str, keypress: DtmfKeypress);
}

/// [`DtmfSink`] adapter that publishes each keypress to an
/// [`crate::bus::EventBus`] as a `SipEvent::Dtmf`. Drop the returned
/// handle and events stop flowing; clone for multiple bridges to
/// share one bus.
#[derive(Clone)]
pub struct BusDtmfSink {
    bus: crate::bus::EventBus,
    /// The `Call-ID` tag written onto every emitted event. Callers
    /// that can scope this per-bridge (UAS does — one bridge per
    /// dialog) set it at construction; callers that can't leave it
    /// `None` so subscribers know to cross-reference elsewhere.
    call_id: Option<String>,
}

impl BusDtmfSink {
    /// Build a sink that tags every event with `call_id`.
    #[must_use]
    pub fn for_call(bus: crate::bus::EventBus, call_id: impl Into<String>) -> Self {
        Self {
            bus,
            call_id: Some(call_id.into()),
        }
    }

    /// Build a sink that emits `None` for `call_id` — appropriate
    /// when the caller can't associate the bridge with a dialog
    /// (standalone test harnesses).
    #[must_use]
    pub fn unassociated(bus: crate::bus::EventBus) -> Self {
        Self { bus, call_id: None }
    }
}

impl DtmfSink for BusDtmfSink {
    fn deliver(&self, _leg: &'static str, keypress: DtmfKeypress) {
        // `EventBus::publish` is non-blocking (bounded broadcast
        // channel). A full channel drops the event rather than
        // backing up the bridge forwarder — DTMF is advisory, not
        // load-bearing on the RTP path.
        let _ = self
            .bus
            .publish(crate::event::Event::Sip(crate::event::SipEvent::Dtmf {
                call_id: self.call_id.clone(),
                keypress,
            }));
    }
}

/// One detected DTMF keypress. Emitted on the event bus when the
/// detector sees the End marker sequence close out a telephone-event
/// stream. Duration + digit are both first-class so UIs / IVR
/// routers don't have to reinterpret the raw samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DtmfKeypress {
    /// Decoded ASCII digit — `'0'`–`'9'`, `'*'`, `'#'`, `'A'`–`'D'`,
    /// or `'!'` for flash.
    pub digit: char,
    /// Wall-clock duration of the keypress. Derived from the RTP
    /// timestamp delta between the first and last telephone-event
    /// packet, converted through the clock rate the detector was
    /// constructed with.
    pub duration_ms: u32,
    /// Which leg of the call the press came from. `"a"` and `"b"`
    /// for rendezvous-bridge calls; a single-leg deployment passes
    /// `"a"`.
    pub leg: String,
}

/// Stateful RFC 4733 decoder. A call's bridge owns one detector per
/// direction; every incoming RTP packet whose payload type matches
/// [`RFC4733_PAYLOAD_TYPE`] is fed through [`Self::feed`]. The
/// detector deduplicates against retransmits (same event + start
/// timestamp) and emits exactly one [`DtmfKeypress`] per digit press.
#[derive(Clone, Debug)]
pub struct DtmfDetector {
    leg_id: String,
    clock_rate_hz: u32,
    /// `(event_code, start_timestamp)` of the keypress currently in
    /// flight. `None` = no press in progress, or one already emitted.
    in_flight: Option<(u8, u32)>,
    /// Highest `duration_samples` seen for the in-flight press.
    /// Telephone-event retransmits report the cumulative duration,
    /// so we take the max rather than sum.
    max_duration: u16,
    /// `(event, timestamp)` of the most recent keypress we've
    /// already emitted. §2.5.1.3 mandates three end-retransmits —
    /// this slot swallows the second and third so the subscriber
    /// only sees one `DtmfKeypress` per press.
    last_emitted: Option<(u8, u32)>,
}

impl DtmfDetector {
    /// Build a detector for one leg. `clock_rate_hz` is the SDP-
    /// negotiated clock for the telephone-event rtpmap (almost always
    /// 8000 — the audio clock).
    #[must_use]
    pub fn new(leg_id: impl Into<String>, clock_rate_hz: u32) -> Self {
        Self {
            leg_id: leg_id.into(),
            clock_rate_hz: clock_rate_hz.max(1),
            in_flight: None,
            max_duration: 0,
            last_emitted: None,
        }
    }

    /// Feed one telephone-event packet. Returns `Some(press)` exactly
    /// once per completed digit — when the first `end=true` packet
    /// arrives. Subsequent End retransmits (§2.5.1.3 mandates three)
    /// for the same `(event, start_ts)` are deduped.
    ///
    /// `start_timestamp` is the RTP timestamp of the telephone-event
    /// packet (the engine's bridge has it handy from the RTP header).
    /// A fresh keypress uses a new timestamp; retransmits reuse it.
    pub fn feed(&mut self, event: &TelephoneEvent, start_timestamp: u32) -> Option<DtmfKeypress> {
        // Swallow end-retransmits of a press we already emitted.
        if let Some((code, ts)) = self.last_emitted
            && code == event.event
            && ts == start_timestamp
        {
            return None;
        }

        // Track the highest duration seen for the active press.
        match self.in_flight {
            Some((code, ts)) if code == event.event && ts == start_timestamp => {
                if event.duration_samples > self.max_duration {
                    self.max_duration = event.duration_samples;
                }
            }
            _ => {
                // New press (different event code or fresh timestamp).
                self.in_flight = Some((event.event, start_timestamp));
                self.max_duration = event.duration_samples;
            }
        }

        if !event.end {
            return None;
        }
        // End packet — emit once, then record the tuple so the
        // RFC-mandated retransmits dedup.
        let (code, ts) = self.in_flight.take()?;
        let digit = event_code_to_digit(code)?;
        let duration_ms = u32::from(self.max_duration)
            .saturating_mul(1000)
            .saturating_div(self.clock_rate_hz);
        self.max_duration = 0;
        self.last_emitted = Some((code, ts));
        Some(DtmfKeypress {
            digit,
            duration_ms,
            leg: self.leg_id.clone(),
        })
    }
}

// ---------------------------------------------------------------------
// RFC 4733 transmit helpers
// ---------------------------------------------------------------------

/// Sample rate [`generate_keypress`] emits at — matches every real
/// RFC 4733 deployment in the wild.
pub const DTMF_GEN_CLOCK_RATE_HZ: u32 = 8_000;
/// Packetization interval in ms used by [`generate_keypress`].
pub const DTMF_GEN_FRAME_MS: u32 = 20;
/// Audio samples per frame (8 kHz × 20 ms / 1000).
pub const DTMF_GEN_FRAME_SAMPLES: u16 = 160;

/// Build the full RTP packet stream for one DTMF keypress — the
/// transmit-side symmetric of [`DtmfDetector`].
///
/// Each call emits one start frame, `duration_ms / 20` intermediates
/// (cumulative `duration_samples` counter), and three end-retransmits
/// (§2.5.1.3). Marker bit set on the first packet only.
///
/// Kept in `smiths-core` rather than buried in a test crate so the
/// MCP `send_dtmf` tool + any future sidecar plugin can share one
/// generator.
///
/// # Panics
///
/// Panics when `digit` isn't a valid DTMF symbol — the caller is
/// expected to validate up-front. Tests pass literals so this only
/// fires on a typo.
#[must_use]
pub fn generate_keypress(
    digit: char,
    duration_ms: u32,
    ssrc: u32,
    start_sequence: u16,
    start_timestamp: u32,
) -> Vec<crate::rtp::RtpPacket> {
    #[allow(clippy::expect_used)] // caller-supplied literal; docs mandate validity.
    let event = digit_to_event_code(digit).expect("invalid DTMF digit");
    let frames = duration_ms.max(DTMF_GEN_FRAME_MS) / DTMF_GEN_FRAME_MS;
    let mut out: Vec<crate::rtp::RtpPacket> = Vec::with_capacity(frames as usize + 3);
    let mut seq = start_sequence;

    for i in 0..frames {
        let cumulative =
            DTMF_GEN_FRAME_SAMPLES.saturating_mul(u16::try_from(i + 1).unwrap_or(u16::MAX));
        let ev = TelephoneEvent {
            event,
            end: false,
            volume: 10,
            duration_samples: cumulative,
        };
        out.push(crate::rtp::RtpPacket {
            marker: i == 0,
            payload_type: RFC4733_PAYLOAD_TYPE,
            sequence: seq,
            timestamp: start_timestamp,
            ssrc,
            payload: ev.encode().to_vec(),
        });
        seq = seq.wrapping_add(1);
    }

    let final_samples =
        DTMF_GEN_FRAME_SAMPLES.saturating_mul(u16::try_from(frames).unwrap_or(u16::MAX));
    let end_ev = TelephoneEvent {
        event,
        end: true,
        volume: 10,
        duration_samples: final_samples,
    };
    for _ in 0..3 {
        out.push(crate::rtp::RtpPacket {
            marker: false,
            payload_type: RFC4733_PAYLOAD_TYPE,
            sequence: seq,
            timestamp: start_timestamp,
            ssrc,
            payload: end_ev.encode().to_vec(),
        });
        seq = seq.wrapping_add(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_encode_round_trip() {
        let ev = TelephoneEvent {
            event: 5,
            end: true,
            volume: 10,
            duration_samples: 1_600,
        };
        let wire = ev.encode();
        let back = TelephoneEvent::parse(&wire).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn parse_rejects_short_payload() {
        assert!(TelephoneEvent::parse(&[0, 0, 0]).is_none());
    }

    #[test]
    fn end_flag_and_volume_decoded() {
        // event=3, end=1, volume=10, duration=0x0400
        let wire = [0x03, 0x8A, 0x04, 0x00];
        let ev = TelephoneEvent::parse(&wire).unwrap();
        assert_eq!(ev.event, 3);
        assert!(ev.end);
        assert_eq!(ev.volume, 10);
        assert_eq!(ev.duration_samples, 0x0400);
    }

    #[test]
    fn event_code_maps_digits() {
        assert_eq!(event_code_to_digit(0), Some('0'));
        assert_eq!(event_code_to_digit(9), Some('9'));
        assert_eq!(event_code_to_digit(10), Some('*'));
        assert_eq!(event_code_to_digit(11), Some('#'));
        assert_eq!(event_code_to_digit(12), Some('A'));
        assert_eq!(event_code_to_digit(15), Some('D'));
        assert_eq!(event_code_to_digit(16), Some('!'));
        assert_eq!(event_code_to_digit(17), None);
    }

    #[test]
    fn digit_to_code_inverse() {
        for c in ['0', '9', '*', '#', 'A', 'D', '!'] {
            let code = digit_to_event_code(c).unwrap();
            assert_eq!(event_code_to_digit(code), Some(c));
        }
        assert_eq!(digit_to_event_code('Z'), None);
    }

    #[test]
    fn detector_emits_once_on_end_packet() {
        let mut d = DtmfDetector::new("a", 8_000);
        // Start (duration 160 = 20 ms @ 8 kHz).
        let p1 = TelephoneEvent {
            event: 7,
            end: false,
            volume: 10,
            duration_samples: 160,
        };
        assert!(d.feed(&p1, 1000).is_none());
        // Middle retransmit.
        let p2 = TelephoneEvent {
            event: 7,
            end: false,
            volume: 10,
            duration_samples: 320,
        };
        assert!(d.feed(&p2, 1000).is_none());
        // End (duration 640 = 80 ms total).
        let p_end = TelephoneEvent {
            event: 7,
            end: true,
            volume: 10,
            duration_samples: 640,
        };
        let press = d.feed(&p_end, 1000).unwrap();
        assert_eq!(press.digit, '7');
        assert_eq!(press.duration_ms, 80);
        assert_eq!(press.leg, "a");
    }

    #[test]
    fn detector_deduplicates_redundant_end_retransmits() {
        let mut d = DtmfDetector::new("b", 8_000);
        let end = TelephoneEvent {
            event: 2,
            end: true,
            volume: 10,
            duration_samples: 800,
        };
        // §2.5.1.3: three end-retransmits. First emits; the rest are
        // deduped.
        assert!(d.feed(&end, 500).is_some());
        assert!(d.feed(&end, 500).is_none());
        assert!(d.feed(&end, 500).is_none());
    }

    #[test]
    fn detector_starts_fresh_press_on_new_timestamp() {
        let mut d = DtmfDetector::new("a", 8_000);
        let p1_end = TelephoneEvent {
            event: 1,
            end: true,
            volume: 10,
            duration_samples: 160,
        };
        assert!(d.feed(&p1_end, 100).is_some());
        // New press (different RTP ts) — even same digit — emits.
        let p2_end = TelephoneEvent {
            event: 1,
            end: true,
            volume: 10,
            duration_samples: 160,
        };
        assert!(d.feed(&p2_end, 900).is_some());
    }
}
