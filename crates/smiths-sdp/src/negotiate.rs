//! SDP offer/answer negotiation.
//!
//! Scope: passthrough for audio (PCMU / PCMA / Opus) and video
//! (H.264 / VP8 / VP9). The negotiator intersects the offered codecs
//! with the engine's `supported` / `supported_video` lists (matched
//! by case-insensitive codec name *and* clock rate; payload type
//! numbers follow the offer to stay passthrough-friendly).
//!
//! The answer carries one audio codec plus the offer's RFC 4733
//! `telephone-event` payload (when offered), and echoes the
//! attributes a passthrough leg must agree on: `a=fmtp` / `a=ptime`
//! / `a=maxptime` for the accepted payload types, `a=mid` +
//! `a=group:BUNDLE`, `a=extmap` (direction reversed), `a=rtcp-mux`
//! and `a=rtcp-fb`. A stream offered on port 0 is answered on port 0
//! (RFC 3264 §6).
//!
//! Audio negotiation is required: an offer with no `m=audio` block
//! mismatches. Video is additive — `m=video` in the offer is
//! answered with either a matching codec on a caller-supplied port
//! or `m=video 0 ...` to decline (RFC 3264 §6 port-zero).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rand::{Rng as _, RngExt as _};
use smiths_core::SrtpSuite;
use smiths_core::sdp::{DtlsParams, DtlsRole, NegotiationOutcome, SdpNegotiator, SrtpKeys};
use smiths_core::{Metrics, SelfSignedCert};

use crate::srtp_attr::SdesCrypto;
use crate::types::{
    ConnectionInfo, Direction, DtlsSetup, ExtMap, Fingerprint, Group, IceCandidate, IcePassword,
    MediaDescription, MediaKind, Origin, RtpMap, SessionDescription,
};

/// Generate fresh SDES key material for `suite` using the OS CSPRNG.
///
/// Returns `suite.key_material_len` bytes (16 + 14 = 30 for the
/// one suite we currently support). Every call returns fresh entropy
/// — callers must not reuse the result across dialogs.
#[must_use]
pub fn fresh_sdes_key(suite: SrtpSuite) -> Vec<u8> {
    let mut buf = vec![0u8; suite.key_material_len()];
    rand::rng().fill_bytes(&mut buf);
    buf
}

/// Generate a fresh `a=ice-ufrag` token. RFC 8839 §5.3: 4–256
/// ICE-chars. 8 chars drawn from the unreserved `a-zA-Z0-9+/`
/// alphabet covers the browser-compat baseline with room to grow.
#[must_use]
pub fn fresh_ice_ufrag() -> String {
    random_ice_token(8)
}

/// Generate a fresh `a=ice-pwd` token. RFC 8839 §5.3: 22–256
/// ICE-chars. 24 gives ~142 bits of entropy — same ballpark
/// browsers emit today.
#[must_use]
pub fn fresh_ice_pwd() -> String {
    random_ice_token(24)
}

fn random_ice_token(len: usize) -> String {
    // ICE-chars per RFC 8839 §5.3: ALPHA / DIGIT / "+" / "/".
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| char::from(CHARS[rng.random_range(0..CHARS.len())]))
        .collect()
}

/// Build a `host` ICE candidate for `(ip, port)` on component 1
/// (RTP) with the RFC 8445 §5.1.2.1 priority formula. Used by
/// the negotiator to emit a host candidate in the answer when
/// native ICE is enabled; the `smiths-ice` crate has the
/// multi-bind gatherer for callers that need more than one.
#[must_use]
pub fn make_host_candidate(ip: IpAddr, port: u16) -> IceCandidate {
    let type_pref: u32 = 126;
    let local_pref: u32 = if matches!(ip, IpAddr::V6(_)) {
        65_535
    } else {
        65_534
    };
    // component = 1 (RTP); priority = (2^24)*type + (2^8)*local + (256 - component)
    let priority = (type_pref << 24) + (local_pref << 8) + 255;
    IceCandidate {
        foundation: "host0".into(),
        component: 1,
        transport: "UDP".into(),
        priority,
        address: ip,
        port,
        candidate_type: "host".into(),
        related_address: None,
        related_port: None,
        raw_params: Vec::new(),
    }
}

/// Outcome of running offer/answer.
///
/// Same size-disparity story as
/// [`smiths_core::NegotiationOutcome`] — `Answer` dominates,
/// boxing would cascade through every destructuring caller, and
/// the negotiator fires at most once per INVITE. The `allow`
/// stays narrow.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum NegotiationResult {
    /// Offer accepted; the engine's answer is ready to send.
    Answer {
        /// Rendered answer.
        sdp: SessionDescription,
        /// SRTP keying material, when the offer was `RTP/SAVP` with a
        /// supported `a=crypto:`. `None` for plain `RTP/AVP`.
        srtp: Option<SrtpKeys>,
        /// DTLS-SRTP parameters. `Some` when the offer used
        /// `UDP/TLS/RTP/SAVP[F]` and the engine had a configured
        /// cert. The caller runs the DTLS handshake to derive SRTP
        /// keys; this field surfaces the peer fingerprint + resolved
        /// role.
        dtls: Option<DtlsParams>,
        /// ICE parameters. Populated when `ice_enabled` is true and
        /// the offer carried ICE credentials.
        ice: Option<smiths_core::sdp::IceParams>,
    },
    /// No codec in the offer intersected with the engine's supported
    /// list — caller should reply with `488 Not Acceptable Here`.
    Mismatch,
    /// Transport profile recognized but unsupported (today:
    /// `UDP/TLS/RTP/SAVP[F]` without a configured cert). Carries a
    /// short human-readable reason for the answerer to include in
    /// the SIP `Warning:` header.
    UnsupportedTransport(String),
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
    /// Video codecs the engine can pass through. Same matching
    /// semantics as [`Self::supported`]: the answer echoes the
    /// offerer's PT so downstream B2BUAs stay happy. Passthrough
    /// only — no decode, no transcoding.
    pub supported_video: Vec<RtpMap>,
    /// Optional DTLS-SRTP identity. When present, offers using the
    /// `UDP/TLS/RTP/SAVP[F]` transport profile are accepted and the
    /// answer carries the cert's fingerprint + a role complementary
    /// to the offer's `a=setup:`. `None` = DTLS-SRTP offers are
    /// rejected with [`NegotiationResult::UnsupportedTransport`].
    pub dtls_cert: Option<Arc<SelfSignedCert>>,
    /// Optional metrics handle. When present, every DTLS-SRTP
    /// negotiation path bumps a counter on the answer's outcome
    /// so operators see the answer-side signal — distinct from
    /// the handshake-side metric the media fabric records.
    pub metrics: Option<Arc<Metrics>>,
    /// Emit native ICE attributes on DTLS-SRTP answers. When
    /// `true`, the answer carries `a=ice-ufrag`, `a=ice-pwd`,
    /// `a=ice-options:trickle`, a host candidate derived from
    /// `(local_ip, local_rtp_port)`, and `a=end-of-candidates`.
    /// The offer's ice-ufrag/ice-pwd are surfaced on
    /// [`NegotiationResult::Answer::ice`] so the `smiths-ice`
    /// agent can authenticate connectivity checks. `false` keeps
    /// the plain shape: no ICE attrs, peer address taken from the
    /// offer's `c=` / `m=` line.
    pub ice_enabled: bool,
    /// Tie-breaker for ICE role determination (RFC 8445 §6.1.1).
    /// Randomly initialized; persisted across negotiations in a
    /// single negotiator instance.
    pub ice_tie_breaker: u64,
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
            dtls_cert: None,
            metrics: None,
            ice_enabled: false,
            ice_tie_breaker: rand::rng().random(),
        }
    }

    /// Enable the native ICE surface on DTLS-SRTP answers. See
    /// [`Self::ice_enabled`] for the exact wire effect.
    #[must_use]
    pub fn with_ice_enabled(mut self, enabled: bool) -> Self {
        self.ice_enabled = enabled;
        self
    }

    /// Attach a DTLS-SRTP identity. Offers using the
    /// `UDP/TLS/RTP/SAVP[F]` transport profile are accepted only
    /// when a cert is present; without one they are rejected as
    /// [`NegotiationResult::UnsupportedTransport`].
    #[must_use]
    pub fn with_dtls_cert(mut self, cert: Arc<SelfSignedCert>) -> Self {
        self.dtls_cert = Some(cert);
        self
    }

    /// Attach the engine's metrics handle so DTLS-SRTP
    /// negotiation outcomes bump `smiths_webrtc_dtls_negotiations_total`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Produce an audio-only answer to `offer`, publishing
    /// `local_port` as the media port. Ignores any `m=video` /
    /// `m=application` blocks the offer carries. Use
    /// [`Self::answer_with_video`] when you want `m=video`
    /// handling.
    ///
    /// When the offer uses `RTP/SAVP` with at least one supported
    /// `a=crypto:` suite, the engine generates a fresh key and emits
    /// a matching `a=crypto:` line in the answer; the returned
    /// [`NegotiationResult::Answer`] carries both halves of the SRTP
    /// key material so the caller can wire transforms on the bridge.
    /// Offers using `RTP/SAVP` **without** any supported crypto line
    /// are rejected as [`NegotiationResult::Mismatch`] — this mirrors
    /// RFC 4568 §5.1.2: a SAVP responder must not proceed unprotected.
    ///
    /// An audio stream offered on port 0 (RFC 3264 §6 rejection /
    /// legacy hold) is answered on port 0 with no codec, no SRTP
    /// and no DTLS material — the call stays up without media.
    #[must_use]
    // Three transport-profile branches (plain / SDES / DTLS-SRTP)
    // each contribute a handful of lines that are cheaper to read
    // inline than factored out behind a helper — extracting them
    // would force the caller to reassemble the `MediaDescription`
    // from separate pieces without clarifying anything.
    #[allow(clippy::too_many_lines)]
    pub fn answer(&self, offer: &SessionDescription, local_port: u16) -> NegotiationResult {
        let Some(audio) = offer.media.iter().find(|m| m.kind == MediaKind::Audio) else {
            return NegotiationResult::Mismatch;
        };

        if audio.port == 0 {
            // RFC 3264 §6: a stream offered with port zero MUST be
            // answered with port zero. No transport is set up, so
            // there is nothing to key or authenticate.
            let media = vec![rejected_stream(audio)];
            return NegotiationResult::Answer {
                sdp: self.session(offer, media),
                srtp: None,
                dtls: None,
                ice: None,
            };
        }

        let Some(chosen) = choose_codec(audio, &self.supported) else {
            return NegotiationResult::Mismatch;
        };
        let dtmf = telephone_event_for(audio, &chosen);

        // --- Transport-profile branching -----------------------------
        //
        // Three profiles are observable on the offer today:
        // - `RTP/AVP` — plain, no SRTP.
        // - `RTP/SAVP` — SDES-negotiated SRTP (RFC 4568).
        // - `UDP/TLS/RTP/SAVP[F]` — DTLS-SRTP (RFC 5764); keys
        //   come from the handshake, not from the SDP.
        //
        // The three return types differ: SDES emits `a=crypto:`
        // and surfaces `SrtpKeys`; DTLS-SRTP emits `a=fingerprint`
        // + `a=setup:` and surfaces `DtlsParams`; plain RTP
        // leaves both empty.
        let is_dtls = is_dtls_srtp_profile(&audio.protocol);
        let is_savp = audio.protocol.eq_ignore_ascii_case("RTP/SAVP");

        let mut answer_crypto: Vec<SdesCrypto> = Vec::new();
        let mut srtp_keys: Option<SrtpKeys> = None;
        let mut answer_fingerprint: Option<Fingerprint> = None;
        let mut answer_setup: Option<DtlsSetup> = None;
        let mut dtls_params: Option<DtlsParams> = None;
        let mut answer_ice_ufrag: Option<String> = None;
        let mut answer_ice_pwd: Option<IcePassword> = None;
        let mut ice_params: Option<smiths_core::sdp::IceParams> = None;
        let mut answer_ice_options: Vec<String> = Vec::new();
        let mut answer_candidates: Vec<IceCandidate> = Vec::new();
        let mut answer_end_of_candidates = false;

        if is_dtls {
            // Accepting a DTLS-SRTP offer requires a cert; without
            // one, surface `UnsupportedTransport` so the handler
            // can explain the limitation to the peer.
            let Some(cert) = self.dtls_cert.as_ref() else {
                return NegotiationResult::UnsupportedTransport(
                    "DTLS-SRTP transport offered but engine has no cert configured".into(),
                );
            };
            let Some(peer_fp) = audio.fingerprint.as_ref() else {
                return NegotiationResult::UnsupportedTransport(
                    "DTLS-SRTP offer missing a=fingerprint".into(),
                );
            };
            // RFC 5763 §5: answer's setup role is the
            // complement of the offer's. `actpass` / `passive` →
            // we pick `active` (we send ClientHello); `active` →
            // we're `passive`.
            let offer_setup = audio.setup.unwrap_or(DtlsSetup::ActPass);
            let local_setup = offer_setup.reverse();
            let role = match local_setup {
                DtlsSetup::Passive => DtlsRole::Server,
                // `ActPass` / `HoldConn` never appear as an
                // answer setup per `DtlsSetup::reverse`.
                _ => DtlsRole::Client,
            };
            answer_fingerprint = Some(Fingerprint {
                algorithm: "sha-256".into(),
                value: cert.sha256_fingerprint.clone(),
            });
            answer_setup = Some(local_setup);
            dtls_params = Some(DtlsParams {
                peer_fingerprint_algorithm: peer_fp.algorithm.clone(),
                peer_fingerprint_value: peer_fp.value.clone(),
                local_role: role,
            });

            // Native ICE surface. The host candidate is derived
            // from `(local_ip, local_port)` — the same tuple the
            // media fabric just allocated. `trickle` is
            // advertised for browser compat even though every
            // candidate is emitted in-band.
            if self.ice_enabled {
                let local_ufrag = fresh_ice_ufrag();
                let local_pwd = fresh_ice_pwd();
                answer_ice_ufrag = Some(local_ufrag.clone());
                answer_ice_pwd = Some(IcePassword(local_pwd.clone()));
                answer_ice_options.push("trickle".into());
                answer_candidates.push(make_host_candidate(self.local_ip, local_port));
                answer_end_of_candidates = true;

                if let (Some(remote_ufrag), Some(remote_pwd)) =
                    (audio.ice_ufrag.as_ref(), audio.ice_pwd.as_ref())
                {
                    // RFC 8445 §6.1.1: If one agent is full and the other
                    // is lite, full agent is controlling. If both are full,
                    // offerer is controlling, answerer is controlled.
                    let remote_is_lite = offer.ice_lite
                        || audio
                            .ice_options
                            .iter()
                            .any(|o| o.eq_ignore_ascii_case("ice-lite"));
                    let role = if remote_is_lite {
                        smiths_core::sdp::IceRole::Controlling
                    } else {
                        smiths_core::sdp::IceRole::Controlled
                    };
                    ice_params = Some(smiths_core::sdp::IceParams {
                        local_ufrag,
                        local_pwd,
                        remote_ufrag: remote_ufrag.clone(),
                        remote_pwd: remote_pwd.0.clone(),
                        role,
                        tie_breaker: self.ice_tie_breaker,
                    });
                }

                if let Some(m) = self.metrics.as_ref() {
                    m.ice_candidates_gathered
                        .get_or_create(&smiths_core::metrics::IceCandidateTypeLabel {
                            ty: "host".into(),
                        })
                        .inc();
                }
            }
        } else if is_savp {
            // RFC 4568 §5.1: SDES responder emits matching
            // `a=crypto:`, surfaces both halves of the key
            // material.
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
            answer_crypto.push(answer_line);
            srtp_keys = Some(SrtpKeys {
                suite,
                peer_tx_key: offer_crypto.key_material.clone(),
                local_tx_key: local_km,
            });
        }

        let mut accepted_pts = vec![chosen.payload_type];
        let mut rtpmap = vec![chosen];
        if let Some(ev) = dtmf {
            accepted_pts.push(ev.payload_type);
            rtpmap.push(ev);
        }
        let mut answer_media =
            MediaDescription::new(MediaKind::Audio, local_port, audio.protocol.clone());
        answer_media.formats = accepted_pts.iter().map(ToString::to_string).collect();
        answer_media.rtpmap = rtpmap;
        answer_media.crypto = answer_crypto;
        answer_media.direction = audio.direction.reverse();
        answer_media.fingerprint = answer_fingerprint;
        answer_media.setup = answer_setup;
        answer_media.ice_ufrag = answer_ice_ufrag;
        answer_media.ice_pwd = answer_ice_pwd;
        answer_media.ice_options = answer_ice_options;
        answer_media.candidates = answer_candidates;
        answer_media.end_of_candidates = answer_end_of_candidates;
        echo_media_attributes(audio, &mut answer_media, &accepted_pts);

        NegotiationResult::Answer {
            sdp: self.session(offer, vec![answer_media]),
            srtp: srtp_keys,
            dtls: dtls_params,
            ice: ice_params,
        }
    }

    /// Multi-stream answer. Produces the same shape as
    /// [`Self::answer`] on the audio side, then appends a matching
    /// `m=video` block to preserve m-line ordering.
    ///
    /// Video-handling rules:
    /// - Offer has no `m=video` → result identical to
    ///   [`Self::answer`] (audio-only).
    /// - Offer has `m=video` on a non-zero port **and**
    ///   `local_video_port` is `Some(port)` **and** at least one
    ///   offered codec is in [`Self::supported_video`] → answer
    ///   echoes the chosen codec on the given port, together with
    ///   its `a=fmtp` / `a=rtcp-fb` lines and the DTLS/ICE
    ///   attributes the audio block negotiated.
    /// - Otherwise → answer carries `m=video 0...` (RFC 3264 §6 —
    ///   port 0 declines a stream while keeping m-line alignment).
    ///   Audio still negotiates normally.
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
            dtls: audio_dtls,
            ice: audio_ice,
        } = audio_result
        else {
            return audio_result;
        };
        let Some(offer_video) = offer.media.iter().find(|m| m.kind == MediaKind::Video) else {
            return NegotiationResult::Answer {
                sdp,
                srtp: audio_srtp,
                dtls: audio_dtls,
                ice: audio_ice,
            };
        };
        let chosen_video = if offer_video.port == 0 {
            None
        } else {
            choose_codec(offer_video, &self.supported_video)
        };

        let video_answer = match (local_video_port, chosen_video) {
            (Some(port), Some(chosen)) => {
                let mut m =
                    MediaDescription::new(MediaKind::Video, port, offer_video.protocol.clone());
                m.formats = vec![chosen.payload_type.to_string()];
                m.direction = offer_video.direction.reverse();
                echo_media_attributes(offer_video, &mut m, &[chosen.payload_type]);
                m.rtpmap = vec![chosen];
                // The video transport reuses the identity and ICE
                // credentials the audio block negotiated; only the
                // host candidate differs (its own port).
                let audio_answer = &sdp.media[0];
                m.fingerprint.clone_from(&audio_answer.fingerprint);
                m.setup = audio_answer.setup;
                m.ice_ufrag.clone_from(&audio_answer.ice_ufrag);
                m.ice_pwd.clone_from(&audio_answer.ice_pwd);
                m.ice_options.clone_from(&audio_answer.ice_options);
                if !audio_answer.candidates.is_empty() {
                    m.candidates = vec![make_host_candidate(self.local_ip, port)];
                    m.end_of_candidates = true;
                }
                m
            }
            _ => rejected_stream(offer_video),
        };
        sdp.media.push(video_answer);
        sdp.groups = bundle_groups(offer, &sdp.media);
        NegotiationResult::Answer {
            sdp,
            srtp: audio_srtp,
            dtls: audio_dtls,
            ice: audio_ice,
        }
    }

    /// Wrap answer media blocks in the session-level boilerplate:
    /// a fresh strictly-increasing `o=` id/version, the engine's
    /// `c=` line and the echoed BUNDLE group.
    fn session(
        &self,
        offer: &SessionDescription,
        media: Vec<MediaDescription>,
    ) -> SessionDescription {
        let version = next_origin_version();
        SessionDescription {
            origin: Origin {
                username: "smiths".into(),
                session_id: version,
                session_version: version,
                address: self.local_ip,
            },
            session_name: "smiths-net".into(),
            connection: Some(ConnectionInfo {
                address: self.local_ip,
            }),
            groups: bundle_groups(offer, &media),
            ice_lite: false,
            media,
        }
    }
}

/// Pick the first offered payload type whose codec is in
/// `supported`. Codecs with an explicit `a=rtpmap` match by
/// case-insensitive name + clock rate; static payload types with no
/// rtpmap (PCMU = 0, PCMA = 8) match by number. RFC 4733
/// `telephone-event` is never a codec choice.
fn choose_codec(offer: &MediaDescription, supported: &[RtpMap]) -> Option<RtpMap> {
    offer
        .payload_types()
        .find_map(|pt| match offer.rtpmap_for(pt) {
            Some(r) => supported
                .iter()
                .find(|s| s.codec.eq_ignore_ascii_case(&r.codec) && s.clock_rate == r.clock_rate)
                .map(|_| r.clone()),
            None => supported.iter().find(|s| s.payload_type == pt).cloned(),
        })
}

/// The RFC 4733 `telephone-event` payload to answer alongside
/// `chosen`: the one whose clock rate matches the codec's (RFC 4733
/// §2.1), else the first one offered.
fn telephone_event_for(offer: &MediaDescription, chosen: &RtpMap) -> Option<RtpMap> {
    let offered =
        |r: &&RtpMap| r.is_telephone_event() && offer.has_format(&r.payload_type.to_string());
    offer
        .rtpmap
        .iter()
        .filter(offered)
        .find(|r| r.clock_rate == chosen.clock_rate)
        .or_else(|| offer.rtpmap.iter().find(offered))
        .cloned()
}

/// Copy the attributes a passthrough answer must agree on from the
/// offer: `a=mid`, `a=extmap` (direction reversed), `a=rtcp-mux`,
/// `a=ptime` / `a=maxptime`, plus `a=fmtp` and `a=rtcp-fb` for the
/// accepted payload types.
fn echo_media_attributes(
    offer: &MediaDescription,
    answer: &mut MediaDescription,
    accepted_pts: &[u8],
) {
    answer.mid.clone_from(&offer.mid);
    answer.extmap = offer
        .extmap
        .iter()
        .map(|e| ExtMap {
            direction: e.direction.map(Direction::reverse),
            ..e.clone()
        })
        .collect();
    answer.rtcp_mux = offer.rtcp_mux;
    answer.ptime = offer.ptime;
    answer.maxptime = offer.maxptime;
    answer.fmtp = accepted_pts
        .iter()
        .filter_map(|pt| offer.fmtp_for(*pt).cloned())
        .collect();
    answer.rtcp_fb = offer
        .rtcp_fb
        .iter()
        .filter(|fb| accepted_pts.iter().any(|pt| fb.applies_to(*pt)))
        .cloned()
        .collect();
}

/// RFC 3264 §6 rejected stream: port 0, same kind / protocol /
/// formats, `a=mid` kept so BUNDLE bookkeeping still lines up, every
/// other attribute dropped.
fn rejected_stream(offer: &MediaDescription) -> MediaDescription {
    let mut m = MediaDescription::new(offer.kind.clone(), 0, offer.protocol.clone());
    m.formats.clone_from(&offer.formats);
    m.mid.clone_from(&offer.mid);
    m.direction = Direction::Inactive;
    m
}

/// RFC 8843 §7.3: answer the offer's BUNDLE group with the accepted
/// m-lines that share one transport. The engine allocates a separate
/// port per stream, so the group holds the first accepted m-line
/// (audio) plus any later block that landed on the same port;
/// rejected m-lines never appear in the group.
fn bundle_groups(offer: &SessionDescription, answer_media: &[MediaDescription]) -> Vec<Group> {
    let offered = offer.bundle_mids();
    if offered.is_empty() {
        return Vec::new();
    }
    let mut anchor_port: Option<u16> = None;
    let mids: Vec<String> = answer_media
        .iter()
        .filter(|m| m.port != 0)
        .filter_map(|m| {
            let mid = m.mid.as_ref()?;
            if !offered.contains(&mid.as_str()) {
                return None;
            }
            match anchor_port {
                None => {
                    anchor_port = Some(m.port);
                    Some(mid.clone())
                }
                Some(p) if p == m.port => Some(mid.clone()),
                Some(_) => None,
            }
        })
        .collect();
    if mids.is_empty() {
        Vec::new()
    } else {
        vec![Group {
            semantics: "BUNDLE".into(),
            mids,
        }]
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
        // Per-call override: always honor the caller's `local_ip` over
        // whatever the negotiator was seeded with.
        let mut scoped = self.clone();
        scoped.local_ip = local_ip;
        match scoped.answer(&offer, local_rtp_port) {
            NegotiationResult::Answer {
                sdp,
                srtp,
                dtls,
                ice,
            } => accepted_outcome(&offer, &sdp, srtp, dtls, ice, false),
            NegotiationResult::Mismatch => NegotiationOutcome::Mismatch,
            NegotiationResult::UnsupportedTransport(reason) => {
                NegotiationOutcome::UnsupportedTransport { reason }
            }
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
        let mut scoped = self.clone();
        scoped.local_ip = local_ip;
        match scoped.answer_with_video(&offer, local_audio_port, local_video_port) {
            NegotiationResult::Answer {
                sdp,
                srtp,
                dtls,
                ice,
            } => accepted_outcome(&offer, &sdp, srtp, dtls, ice, true),
            NegotiationResult::Mismatch => NegotiationOutcome::Mismatch,
            NegotiationResult::UnsupportedTransport(reason) => {
                NegotiationOutcome::UnsupportedTransport { reason }
            }
        }
    }

    fn build_offer(&self, local_ip: IpAddr, local_rtp_port: u16) -> String {
        let version = next_origin_version();
        let mut audio = MediaDescription::new(MediaKind::Audio, local_rtp_port, "RTP/AVP");
        audio.formats = self
            .supported
            .iter()
            .map(|c| c.payload_type.to_string())
            .collect();
        audio.rtpmap.clone_from(&self.supported);
        let sdp = SessionDescription {
            origin: Origin {
                username: "smiths".into(),
                session_id: version,
                session_version: version,
                address: local_ip,
            },
            session_name: "smiths-net".into(),
            connection: Some(ConnectionInfo { address: local_ip }),
            groups: Vec::new(),
            ice_lite: false,
            media: vec![audio],
        };
        sdp.to_string()
    }

    fn parse_remote_rtp(&self, answer_body: &str) -> Option<SocketAddr> {
        let sdp = SessionDescription::parse(answer_body).ok()?;
        first_audio_endpoint(&sdp)
    }
}

/// Assemble the trait-level outcome from a rendered answer: peer
/// endpoints come from the offer, codec / clock rate / rtcp-mux
/// from the answer.
fn accepted_outcome(
    offer: &SessionDescription,
    answer: &SessionDescription,
    srtp: Option<SrtpKeys>,
    dtls: Option<DtlsParams>,
    ice: Option<smiths_core::sdp::IceParams>,
    include_video: bool,
) -> NegotiationOutcome {
    let offer_audio = offer.media.iter().find(|m| m.kind == MediaKind::Audio);
    let answer_audio = answer.media.iter().find(|m| m.kind == MediaKind::Audio);
    let remote_media = first_audio_endpoint(offer);
    let remote_rtcp_port = if remote_media.is_some() {
        offer_audio.and_then(MediaDescription::rtcp_port)
    } else {
        None
    };
    NegotiationOutcome::Accepted {
        answer_body: answer.to_string(),
        remote_media,
        remote_rtcp_port,
        rtcp_mux: answer_audio.is_some_and(|m| m.rtcp_mux),
        video_media: if include_video {
            first_video_endpoint(offer)
        } else {
            None
        },
        srtp,
        dtls,
        audio_codec: first_codec_of_kind(answer, &MediaKind::Audio),
        audio_clock_rate: answer_audio
            .filter(|m| m.port != 0)
            .and_then(|m| m.rtpmap.first())
            .map(|r| r.clock_rate),
        video_codec: if include_video {
            first_codec_of_kind(answer, &MediaKind::Video)
        } else {
            None
        },
        ice,
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

/// First codec on a given media kind's answer block.
///
/// Returns the codec that landed first in the answer's `a=rtpmap:`
/// entries for the requested `kind` (the negotiated codec precedes
/// `telephone-event`). Produces `None` when the media block is
/// absent, was declined (port 0 ⇒ no rtpmap emitted), or exists only
/// as a transport-line placeholder. The UAS reads this into the
/// `DialogRecord`'s `per_leg_codec` map — the two legs' codecs
/// compare equal iff the call can run passthrough.
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

/// Process-wide source of `o=` session ids / versions.
///
/// RFC 3264 §8 wants every re-answer to carry a version greater than
/// the previous one. Wall-clock seconds alone can't guarantee that
/// for two answers in the same second, so the counter is seeded from
/// the clock and then strictly increases: `max(previous + 1, now)`.
static ORIGIN_VERSION: AtomicU64 = AtomicU64::new(0);

fn next_origin_version() -> u64 {
    let now = unix_seconds();
    let previous = ORIGIN_VERSION
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |prev| {
            Some(prev.saturating_add(1).max(now))
        })
        .unwrap_or(now);
    previous.saturating_add(1).max(now)
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

    fn rtp(pt: u8, codec: &str, clock: u32, channels: Option<u8>) -> RtpMap {
        RtpMap {
            payload_type: pt,
            codec: codec.into(),
            clock_rate: clock,
            channels,
        }
    }

    fn pcmu() -> RtpMap {
        rtp(0, "PCMU", 8_000, None)
    }

    fn offer_with(formats: &[u8], rtpmaps: Vec<RtpMap>) -> SessionDescription {
        let mut audio = MediaDescription::new(MediaKind::Audio, 49_170, "RTP/AVP");
        audio.formats = formats.iter().map(ToString::to_string).collect();
        audio.rtpmap = rtpmaps;
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
            groups: Vec::new(),
            ice_lite: false,
            media: vec![audio],
        }
    }

    fn savp_offer_with_crypto(tag: u32) -> SessionDescription {
        let mut offer = offer_with(&[0], vec![pcmu()]);
        offer.media[0].protocol = "RTP/SAVP".into();
        offer.media[0].crypto = vec![SdesCrypto {
            tag,
            suite: SrtpSuite::AesCm128HmacSha1_80,
            key_material: (0..30u8).collect(),
        }];
        offer
    }

    fn answer_of(result: NegotiationResult) -> SessionDescription {
        match result {
            NegotiationResult::Answer { sdp, .. } => sdp,
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn picks_first_common_codec() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(
            &[111, 0, 8],
            vec![
                rtp(111, "opus", 48_000, Some(2)),
                pcmu(),
                rtp(8, "PCMA", 8_000, None),
            ],
        );
        let NegotiationResult::Answer {
            sdp: answer, srtp, ..
        } = neg.answer(&offer, 16_384)
        else {
            panic!("expected Answer");
        };
        assert_eq!(answer.media.len(), 1);
        assert_eq!(answer.media[0].formats, ["111"]);
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
        let offer = offer_with(&[0], vec![]);
        let answer = answer_of(neg.answer(&offer, 16_384));
        assert_eq!(answer.media[0].formats, ["0"]);
    }

    #[test]
    fn reverses_direction() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut offer = offer_with(&[0], vec![pcmu()]);
        offer.media[0].direction = Direction::SendOnly;
        let answer = answer_of(neg.answer(&offer, 1_234));
        assert_eq!(answer.media[0].direction, Direction::RecvOnly);
    }

    #[test]
    fn savp_offer_gets_savp_answer_with_matching_crypto_tag() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = savp_offer_with_crypto(42);
        let NegotiationResult::Answer {
            sdp: answer, srtp, ..
        } = neg.answer(&offer, 16_384)
        else {
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
        let answer = answer_of(neg.answer(&offer, 16_384));
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
        let offer = offer_with(&[18], vec![rtp(18, "G729", 8_000, None)]);
        assert_eq!(neg.answer(&offer, 1_234), NegotiationResult::Mismatch);
    }

    #[test]
    fn telephone_event_alone_is_not_an_audio_codec() {
        // RFC 4733 events ride alongside a codec; an offer with only
        // the event payload has nothing to carry voice on.
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(&[96], vec![rtp(96, "telephone-event", 8_000, None)]);
        assert_eq!(neg.answer(&offer, 1_234), NegotiationResult::Mismatch);
    }

    #[test]
    fn answer_carries_codec_plus_telephone_event_and_its_fmtp() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut offer = offer_with(
            &[0, 8, 101],
            vec![
                pcmu(),
                rtp(8, "PCMA", 8_000, None),
                rtp(101, "telephone-event", 8_000, None),
            ],
        );
        offer.media[0].fmtp = vec![crate::types::Fmtp {
            format: "101".into(),
            params: "0-16".into(),
        }];
        offer.media[0].ptime = Some(20);
        let answer = answer_of(neg.answer(&offer, 16_384));
        let m = &answer.media[0];
        assert_eq!(m.formats, ["0", "101"]);
        assert_eq!(m.rtpmap.len(), 2);
        assert_eq!(m.rtpmap[0].codec, "PCMU");
        assert!(m.rtpmap[1].is_telephone_event());
        assert_eq!(m.fmtp_for(101).map(|f| f.params.as_str()), Some("0-16"));
        assert_eq!(m.ptime, Some(20));
        let body = answer.to_string();
        assert!(body.contains("m=audio 16384 RTP/AVP 0 101\r\n"), "{body}");
        assert!(body.contains("a=fmtp:101 0-16\r\n"), "{body}");
        assert!(body.contains("a=ptime:20\r\n"), "{body}");
    }

    #[test]
    fn telephone_event_prefers_codec_clock_rate() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(
            &[111, 126, 110],
            vec![
                rtp(111, "opus", 48_000, Some(2)),
                rtp(126, "telephone-event", 8_000, None),
                rtp(110, "telephone-event", 48_000, None),
            ],
        );
        let answer = answer_of(neg.answer(&offer, 16_384));
        assert_eq!(answer.media[0].formats, ["111", "110"]);
    }

    #[test]
    fn origin_version_strictly_increases_across_answers() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(&[0], vec![pcmu()]);
        let a = answer_of(neg.answer(&offer, 16_384));
        let b = answer_of(neg.answer(&offer, 16_384));
        assert!(
            b.origin.session_version > a.origin.session_version,
            "re-answer must bump o= version even within one second: {} vs {}",
            a.origin.session_version,
            b.origin.session_version
        );
    }

    // -----------------------------------------------------------------
    // Video passthrough
    // -----------------------------------------------------------------

    fn audio_video_offer(audio_port: u16, video_port: u16) -> SessionDescription {
        let mut sdp = offer_with(&[0], vec![pcmu()]);
        sdp.media[0].port = audio_port;
        let mut video = MediaDescription::new(MediaKind::Video, video_port, "RTP/AVP");
        video.formats = vec!["96".into(), "97".into()];
        video.rtpmap = vec![rtp(96, "H264", 90_000, None), rtp(97, "VP8", 90_000, None)];
        sdp.media.push(video);
        sdp
    }

    #[test]
    fn answer_with_video_echoes_first_supported_codec() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let sdp = answer_of(neg.answer_with_video(&offer, 16_384, Some(16_386)));
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[0].kind, MediaKind::Audio);
        assert_eq!(sdp.media[0].port, 16_384);
        assert_eq!(sdp.media[1].kind, MediaKind::Video);
        assert_eq!(sdp.media[1].port, 16_386);
        // H.264 was first in the offer's format list; it wins.
        assert_eq!(sdp.media[1].formats, ["96"]);
        assert_eq!(sdp.media[1].rtpmap.len(), 1);
        assert_eq!(sdp.media[1].rtpmap[0].codec, "H264");
    }

    #[test]
    fn answer_with_video_port_none_declines_with_port_zero() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = audio_video_offer(49_170, 49_172);
        let sdp = answer_of(neg.answer_with_video(&offer, 16_384, None));
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
        offer.media[1].formats = vec!["100".into()];
        offer.media[1].rtpmap = vec![rtp(100, "AV1", 90_000, None)];
        let sdp = answer_of(neg.answer_with_video(&offer, 16_384, Some(16_386)));
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[1].port, 0);
        // Audio untouched.
        assert_eq!(sdp.media[0].port, 16_384);
    }

    #[test]
    fn answer_with_video_is_audio_only_when_offer_has_no_video() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(&[0], vec![pcmu()]);
        let sdp = answer_of(neg.answer_with_video(&offer, 16_384, Some(16_386)));
        assert_eq!(sdp.media.len(), 1);
        assert_eq!(sdp.media[0].kind, MediaKind::Audio);
    }

    #[test]
    fn accepted_video_echoes_rtcp_fb_and_fmtp_for_chosen_pt() {
        use crate::types::{Fmtp, RtcpFeedback};
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut offer = audio_video_offer(49_170, 49_172);
        offer.media[1].rtcp_fb = vec![
            RtcpFeedback {
                format: "96".into(),
                value: "nack pli".into(),
            },
            RtcpFeedback {
                format: "97".into(),
                value: "goog-remb".into(),
            },
            RtcpFeedback {
                format: "*".into(),
                value: "ccm fir".into(),
            },
        ];
        offer.media[1].fmtp = vec![
            Fmtp {
                format: "96".into(),
                params: "profile-level-id=42e01f;packetization-mode=1".into(),
            },
            Fmtp {
                format: "97".into(),
                params: "max-fs=12288".into(),
            },
        ];
        let sdp = answer_of(neg.answer_with_video(&offer, 16_384, Some(16_386)));
        let video = &sdp.media[1];
        assert_eq!(
            video.rtcp_fb,
            vec![
                RtcpFeedback {
                    format: "96".into(),
                    value: "nack pli".into()
                },
                RtcpFeedback {
                    format: "*".into(),
                    value: "ccm fir".into()
                },
            ],
            "only feedback lines for the chosen PT (and the wildcard) are echoed"
        );
        assert_eq!(video.fmtp.len(), 1);
        assert_eq!(video.fmtp[0].format, "96");
        let body = sdp.to_string();
        assert!(body.contains("a=rtcp-fb:96 nack pli\r\n"), "{body}");
        assert!(body.contains("a=rtcp-fb:* ccm fir\r\n"), "{body}");
        assert!(
            body.contains("a=fmtp:96 profile-level-id=42e01f;packetization-mode=1\r\n"),
            "{body}"
        );
        assert!(!body.contains("goog-remb"), "{body}");
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

    // ---- Codec detection on the accepted outcome ----

    #[test]
    fn negotiate_audio_exposes_pcmu_on_pcmu_offer() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = offer_with(&[0], vec![pcmu()]);
        let outcome =
            neg.negotiate_audio(&offer.to_string(), IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384);
        let smiths_core::NegotiationOutcome::Accepted {
            audio_codec,
            video_codec,
            audio_clock_rate,
            rtcp_mux,
            remote_rtcp_port,
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
        assert_eq!(audio_clock_rate, Some(8_000));
        assert!(!rtcp_mux);
        assert_eq!(remote_rtcp_port, Some(49_171), "RTP + 1 without a=rtcp");
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

    // --- DTLS-SRTP accept path --------------

    fn dtls_offer_with_setup(setup: DtlsSetup) -> SessionDescription {
        let mut offer = offer_with(&[0], vec![pcmu()]);
        offer.media[0].protocol = "UDP/TLS/RTP/SAVP".into();
        offer.media[0].fingerprint = Some(Fingerprint {
            algorithm: "sha-256".into(),
            // 32 colon-separated octets — doesn't need to match a
            // real cert, the negotiator only echoes the peer's
            // value into DtlsParams.
            value: "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:\
                 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99"
                .into(),
        });
        offer.media[0].setup = Some(setup);
        offer
    }

    #[test]
    fn dtls_offer_without_cert_returns_unsupported() {
        // Bare negotiator — no SelfSignedCert configured.
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        match neg.answer(&offer, 16_384) {
            NegotiationResult::UnsupportedTransport(reason) => {
                assert!(
                    reason.contains("cert"),
                    "unsupported reason should mention the missing cert: {reason}"
                );
            }
            other => panic!("expected UnsupportedTransport, got {other:?}"),
        }
    }

    #[test]
    fn dtls_offer_with_actpass_gets_active_answer() {
        // Offer `actpass` → engine picks `active` (client role,
        // RFC 5763 §5). Answer carries the engine's fingerprint
        // verbatim and the peer's fingerprint echoes back in
        // DtlsParams.
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert.clone()));
        let offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        let NegotiationResult::Answer {
            sdp, dtls, srtp, ..
        } = neg.answer(&offer, 16_384)
        else {
            panic!("expected Answer");
        };
        assert_eq!(
            sdp.media[0].protocol, "UDP/TLS/RTP/SAVP",
            "answer must echo the DTLS-SRTP transport profile"
        );
        let fp = sdp.media[0]
            .fingerprint
            .as_ref()
            .expect("answer must carry a=fingerprint");
        assert_eq!(fp.algorithm, "sha-256");
        assert_eq!(
            fp.value, cert.sha256_fingerprint,
            "answer fingerprint must hash the engine's cert"
        );
        assert_eq!(
            sdp.media[0].setup,
            Some(DtlsSetup::Active),
            "actpass offer should get active answer per RFC 5763 §5"
        );
        let dtls = dtls.expect("DTLS-SRTP accept must surface DtlsParams");
        assert_eq!(dtls.local_role, DtlsRole::Client);
        assert_eq!(dtls.peer_fingerprint_algorithm, "sha-256");
        assert!(
            dtls.peer_fingerprint_value.starts_with("AA:BB:"),
            "peer fingerprint should round-trip verbatim from the offer"
        );
        assert!(
            srtp.is_none(),
            "DTLS path derives keys via the handshake — no SDES keys on the outcome"
        );
    }

    #[test]
    fn dtls_offer_with_active_gets_passive_answer() {
        // Offer `active` → engine takes `passive` (server role).
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert));
        let offer = dtls_offer_with_setup(DtlsSetup::Active);
        let NegotiationResult::Answer { sdp, dtls, .. } = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(sdp.media[0].setup, Some(DtlsSetup::Passive));
        assert_eq!(dtls.unwrap().local_role, DtlsRole::Server);
    }

    #[test]
    fn dtls_offer_without_fingerprint_is_rejected() {
        // RFC 5763 §5.3 requires a fingerprint line; without it
        // we can't verify the peer's cert. Surface as
        // UnsupportedTransport so the handler gets a clear reason.
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert));
        let mut offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        offer.media[0].fingerprint = None;
        match neg.answer(&offer, 16_384) {
            NegotiationResult::UnsupportedTransport(reason) => {
                assert!(reason.contains("fingerprint"), "{reason}");
            }
            other => panic!("expected UnsupportedTransport, got {other:?}"),
        }
    }

    #[test]
    fn ice_disabled_answer_omits_ice_attrs() {
        // Without `with_ice_enabled(true)`, a DTLS-SRTP answer
        // skips ice-ufrag/ice-pwd/candidate — the shape operators
        // who don't want the native stack get.
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert));
        let offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        let sdp = answer_of(neg.answer(&offer, 16_384));
        assert!(sdp.media[0].ice_ufrag.is_none());
        assert!(sdp.media[0].ice_pwd.is_none());
        assert!(sdp.media[0].candidates.is_empty());
    }

    #[test]
    fn ice_enabled_answer_carries_ufrag_pwd_and_host_candidate() {
        // With ICE enabled, the DTLS-SRTP answer carries a fresh
        // ufrag/pwd + one host candidate derived from the engine's
        // local address + allocated port. `end-of-candidates` marks
        // the in-band list complete for a browser running with
        // `iceGatheringPolicy = "all"`.
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)))
            .with_dtls_cert(Arc::new(cert))
            .with_ice_enabled(true);
        let offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        let sdp = answer_of(neg.answer(&offer, 31_415));
        let m = &sdp.media[0];
        let ufrag = m.ice_ufrag.as_ref().expect("ice-ufrag");
        let pwd = m.ice_pwd.as_ref().expect("ice-pwd");
        assert!(ufrag.len() >= 4, "ice-ufrag must be >=4 ICE-chars");
        assert!(pwd.0.len() >= 22, "ice-pwd must be >=22 ICE-chars");
        assert_eq!(m.candidates.len(), 1, "one host candidate");
        let c = &m.candidates[0];
        assert_eq!(c.candidate_type, "host");
        assert_eq!(c.address, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)));
        assert_eq!(c.port, 31_415);
        assert!(m.end_of_candidates);
        // Trickle option advertised so browsers treat it
        // as a valid trickle-ICE session.
        assert!(m.ice_options.iter().any(|o| o == "trickle"));
    }

    #[test]
    fn ice_ufrag_and_pwd_are_fresh_across_answers() {
        // Each answer must mint new tokens — repeating them
        // across dialogs would let a cross-dialog attacker
        // guess the connectivity-check key.
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert))
            .with_ice_enabled(true);
        let offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        let a = answer_of(neg.answer(&offer, 16_384));
        let b = answer_of(neg.answer(&offer, 16_384));
        assert_ne!(a.media[0].ice_ufrag, b.media[0].ice_ufrag);
        assert_ne!(
            a.media[0].ice_pwd.as_ref().map(|p| &p.0),
            b.media[0].ice_pwd.as_ref().map(|p| &p.0)
        );
    }

    #[test]
    fn ice_role_is_controlling_against_ice_lite_offer() {
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert))
            .with_ice_enabled(true);
        let mut offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        offer.media[0].ice_ufrag = Some("peer".into());
        offer.media[0].ice_pwd = Some(IcePassword("peerpwdpeerpwdpeerpwd01".into()));
        let NegotiationResult::Answer { ice, .. } = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(
            ice.unwrap().role,
            smiths_core::sdp::IceRole::Controlled,
            "both full → answerer is controlled"
        );
        offer.ice_lite = true;
        let NegotiationResult::Answer { ice, .. } = neg.answer(&offer, 16_384) else {
            panic!("expected Answer");
        };
        assert_eq!(
            ice.unwrap().role,
            smiths_core::sdp::IceRole::Controlling,
            "full agent is controlling against an ice-lite peer"
        );
    }

    #[test]
    fn negotiate_audio_exposes_dtls_when_cert_configured() {
        // Round-trip through the trait method so the
        // `NegotiationOutcome::Accepted.dtls` binding is
        // locked in — downstream consumers (CLI WebRTC
        // handler, SIP UAS) pattern-match on this field.
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST))
            .with_dtls_cert(Arc::new(cert.clone()));
        let offer = dtls_offer_with_setup(DtlsSetup::ActPass);
        let outcome =
            neg.negotiate_audio(&offer.to_string(), IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384);
        let smiths_core::NegotiationOutcome::Accepted {
            dtls,
            srtp,
            answer_body,
            ..
        } = outcome
        else {
            panic!("expected Accepted");
        };
        let dtls = dtls.expect("Accepted should carry DtlsParams");
        assert_eq!(dtls.local_role, DtlsRole::Client);
        assert!(srtp.is_none());
        // Spot-check the rendered answer body carries the
        // DTLS-SRTP profile + our fingerprint.
        assert!(answer_body.contains("UDP/TLS/RTP/SAVP"));
        assert!(answer_body.contains(&cert.sha256_fingerprint));
        assert!(answer_body.contains("a=setup:active"));
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

    // -----------------------------------------------------------------
    // Negotiation matrix on wire-format offers
    // -----------------------------------------------------------------

    /// Chrome-style audio + video offer: BUNDLE, mids, extmap,
    /// rtcp-mux, `a=rtcp:`, multiple PTs with telephone-event at two
    /// clock rates, session-level DTLS attributes.
    const BROWSER_OFFER: &str = concat!(
        "v=0\r\n",
        "o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n",
        "s=-\r\n",
        "t=0 0\r\n",
        "a=group:BUNDLE 0 1\r\n",
        "a=ice-options:trickle\r\n",
        "a=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\n",
        "a=setup:actpass\r\n",
        "m=audio 50000 UDP/TLS/RTP/SAVPF 111 0 8 110 126\r\n",
        "c=IN IP4 198.51.100.4\r\n",
        "a=rtcp:50001 IN IP4 198.51.100.4\r\n",
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
        "m=video 50002 UDP/TLS/RTP/SAVPF 96 97\r\n",
        "c=IN IP4 198.51.100.4\r\n",
        "a=ice-ufrag:F7gI\r\n",
        "a=ice-pwd:x9cml/YzichV2+XlhiMu8g\r\n",
        "a=mid:1\r\n",
        "a=sendrecv\r\n",
        "a=rtcp-mux\r\n",
        "a=rtpmap:96 VP8/90000\r\n",
        "a=rtcp-fb:96 nack pli\r\n",
        "a=rtpmap:97 H264/90000\r\n",
        "a=fmtp:97 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
    );

    fn dtls_negotiator() -> Negotiator {
        let cert = smiths_core::SelfSignedCert::generate("test").unwrap();
        Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)))
            .with_dtls_cert(Arc::new(cert))
            .with_ice_enabled(true)
    }

    #[test]
    fn matrix_browser_offer_multi_pt_bundle_rtcp_mux_video_declined() {
        let neg = dtls_negotiator();
        let outcome = neg.negotiate(
            BROWSER_OFFER,
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)),
            16_384,
            None,
        );
        let smiths_core::NegotiationOutcome::Accepted {
            answer_body,
            remote_media,
            remote_rtcp_port,
            rtcp_mux,
            audio_codec,
            audio_clock_rate,
            video_codec,
            ice,
            ..
        } = outcome
        else {
            panic!("expected Accepted, got {outcome:?}");
        };
        assert_eq!(remote_media.unwrap().port(), 50_000);
        assert!(rtcp_mux, "offered a=rtcp-mux must be accepted");
        assert_eq!(
            remote_rtcp_port,
            Some(50_001),
            "explicit a=rtcp: port wins over the rtcp-mux default"
        );
        assert_eq!(audio_codec, Some(smiths_core::NegotiatedCodec::Opus));
        assert_eq!(audio_clock_rate, Some(48_000));
        assert_eq!(video_codec, None);
        let ice = ice.expect("ICE creds surfaced");
        assert_eq!(ice.remote_ufrag, "F7gI");
        assert_eq!(ice.remote_pwd, "x9cml/YzichV2+XlhiMu8g");

        let answer = SessionDescription::parse(&answer_body).expect("answer parses");
        assert_eq!(answer.media.len(), 2, "one answer m-line per offer m-line");
        let audio = &answer.media[0];
        assert_eq!(
            audio.formats,
            ["111", "110"],
            "opus + 48 kHz telephone-event"
        );
        assert_eq!(audio.mid.as_deref(), Some("0"));
        assert!(audio.rtcp_mux);
        assert_eq!(audio.ptime, Some(20));
        assert_eq!(
            audio.fmtp_for(111).map(|f| f.params.as_str()),
            Some("minptime=10;useinbandfec=1")
        );
        assert_eq!(audio.extmap.len(), 2);
        assert_eq!(audio.extmap[0].id, 1);
        assert_eq!(
            audio.extmap[1].direction,
            Some(Direction::RecvOnly),
            "sendonly extension on the offer is recvonly on the answer"
        );
        assert_eq!(audio.rtcp_fb.len(), 1);
        assert_eq!(audio.rtcp_fb[0].value, "transport-cc");
        assert_eq!(audio.setup, Some(DtlsSetup::Active));
        assert!(audio.fingerprint.is_some());

        let video = &answer.media[1];
        assert_eq!(video.port, 0);
        assert_eq!(
            video.mid.as_deref(),
            Some("1"),
            "rejected m-line keeps its mid"
        );
        assert_eq!(video.formats, ["96", "97"]);

        assert_eq!(
            answer.bundle_mids(),
            vec!["0"],
            "BUNDLE group echoes only the accepted m-line"
        );
        assert!(
            answer_body.contains("a=group:BUNDLE 0\r\n"),
            "{answer_body}"
        );
    }

    #[test]
    fn matrix_hold_with_port_zero_is_answered_with_port_zero() {
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let mut offer = offer_with(
            &[0, 101],
            vec![pcmu(), rtp(101, "telephone-event", 8_000, None)],
        );
        offer.media[0].port = 0;
        offer.media[0].mid = Some("audio".into());
        let outcome =
            neg.negotiate_audio(&offer.to_string(), IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384);
        let smiths_core::NegotiationOutcome::Accepted {
            answer_body,
            remote_media,
            remote_rtcp_port,
            audio_codec,
            audio_clock_rate,
            ..
        } = outcome
        else {
            panic!("expected Accepted, got {outcome:?}");
        };
        assert!(remote_media.is_none());
        assert!(remote_rtcp_port.is_none());
        assert!(audio_codec.is_none());
        assert!(audio_clock_rate.is_none());
        assert!(
            answer_body.contains("m=audio 0 RTP/AVP 0 101\r\n"),
            "port-0 offer must be answered on port 0: {answer_body}"
        );
        assert!(answer_body.contains("a=mid:audio\r\n"));
        assert!(!answer_body.contains("a=rtpmap"));
    }

    #[test]
    fn matrix_sip_phone_offer_pcmu_plus_dtmf_without_bundle() {
        let wire = concat!(
            "v=0\r\n",
            "o=phone 1 1 IN IP4 192.0.2.40\r\n",
            "s=call\r\n",
            "c=IN IP4 192.0.2.40\r\n",
            "t=0 0\r\n",
            "m=audio 4000 RTP/AVP 8 0 101\r\n",
            "a=rtpmap:8 PCMA/8000\r\n",
            "a=rtpmap:0 PCMU/8000\r\n",
            "a=rtpmap:101 telephone-event/8000\r\n",
            "a=fmtp:101 0-16\r\n",
            "a=ptime:30\r\n",
            "a=sendrecv\r\n",
        );
        let neg = Negotiator::with_default_codecs(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let outcome = neg.negotiate_audio(wire, IpAddr::V4(Ipv4Addr::LOCALHOST), 16_384);
        let smiths_core::NegotiationOutcome::Accepted {
            answer_body,
            rtcp_mux,
            remote_rtcp_port,
            audio_codec,
            ..
        } = outcome
        else {
            panic!("expected Accepted, got {outcome:?}");
        };
        assert_eq!(audio_codec, Some(smiths_core::NegotiatedCodec::Pcma));
        assert!(!rtcp_mux);
        assert_eq!(remote_rtcp_port, Some(4_001));
        assert!(
            answer_body.contains("m=audio 16384 RTP/AVP 8 101\r\n"),
            "{answer_body}"
        );
        assert!(answer_body.contains("a=fmtp:101 0-16\r\n"), "{answer_body}");
        assert!(answer_body.contains("a=ptime:30\r\n"), "{answer_body}");
        assert!(!answer_body.contains("a=group:"), "{answer_body}");
        assert!(!answer_body.contains("a=mid:"), "{answer_body}");
        assert!(!answer_body.contains("a=rtcp-mux"), "{answer_body}");
    }

    #[test]
    fn matrix_video_accepted_on_own_port_stays_out_of_bundle() {
        // The media layer allocates one transport per stream, so an
        // accepted video m-line on a second port cannot honestly be
        // part of the audio BUNDLE group.
        let neg = dtls_negotiator();
        let outcome = neg.negotiate(
            BROWSER_OFFER,
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9)),
            16_384,
            Some(16_386),
        );
        let smiths_core::NegotiationOutcome::Accepted {
            answer_body,
            video_codec,
            ..
        } = outcome
        else {
            panic!("expected Accepted, got {outcome:?}");
        };
        assert_eq!(video_codec, Some(smiths_core::NegotiatedCodec::Vp8));
        let answer = SessionDescription::parse(&answer_body).unwrap();
        assert_eq!(answer.bundle_mids(), vec!["0"]);
        let video = &answer.media[1];
        assert_eq!(video.port, 16_386);
        assert_eq!(video.mid.as_deref(), Some("1"));
        assert!(video.rtcp_mux);
        assert_eq!(video.rtcp_fb.len(), 1);
        assert_eq!(video.rtcp_fb[0].value, "nack pli");
        assert!(
            video.fingerprint.is_some(),
            "DTLS video m-line needs a fingerprint"
        );
        assert_eq!(video.setup, Some(DtlsSetup::Active));
        assert_eq!(video.candidates.len(), 1);
        assert_eq!(video.candidates[0].port, 16_386);
    }
}
