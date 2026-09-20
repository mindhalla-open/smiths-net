//! Audio injection into a live call leg: `speak`, `send_dtmf`,
//! `record_prompt`, and the RTP framing they share.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use smiths_core::{EndpointId, RtpPacket};

use super::ai::resolve_and_validate;
use super::{live_media_leg, require_str};
use crate::tool::{Tool, ToolContext, ToolError};

/// RFC 3551 PCMU payload type.
pub(super) const PT_PCMU: u8 = 0;
/// PCMU frame cadence (ITU-T G.711, 8 kHz → 20 ms = 160 samples).
pub(super) const FRAME_SAMPLES: usize = 160;
/// [`FRAME_SAMPLES`] as an RTP clock increment.
pub(super) const FRAME_TICKS: u32 = 160;
/// Wall-clock spacing between paced frames.
pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(20);

/// Sequence / timestamp / SSRC state for one outbound RTP stream.
/// The first packet after construction (or after [`Self::mark_gap`])
/// carries the marker bit, as RFC 3551 §4.1 asks for the start of a
/// talkspurt.
pub(super) struct RtpStream {
    ssrc: u32,
    seq: u16,
    ts: u32,
    marker_pending: bool,
}

impl RtpStream {
    /// Fresh stream with a session-unique SSRC and a randomised
    /// starting sequence number.
    pub(super) fn new() -> Self {
        Self {
            ssrc: fresh_ssrc(),
            seq: fresh_seq(),
            ts: 0,
            marker_pending: true,
        }
    }

    pub(super) fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Set the marker bit on the next packet (start of a talkspurt
    /// after silence).
    pub(super) fn mark_gap(&mut self) {
        self.marker_pending = true;
    }

    /// Frame `payload` as the next packet, advancing the sequence by
    /// one and the timestamp by `ticks`.
    pub(super) fn next_packet(
        &mut self,
        payload_type: u8,
        payload: Vec<u8>,
        ticks: u32,
    ) -> RtpPacket {
        let pkt = RtpPacket {
            marker: std::mem::take(&mut self.marker_pending),
            payload_type,
            sequence: self.seq,
            timestamp: self.ts,
            ssrc: self.ssrc,
            payload,
        };
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(ticks);
        pkt
    }
}

/// Stream PCM16 (at `sample_rate`) into a live call leg as paced PCMU
/// RTP frames. Returns the number of 20 ms frames sent plus the
/// stream's SSRC.
pub(super) async fn inject_pcm16_into_call(
    ctx: &ToolContext,
    endpoint: EndpointId,
    remote: SocketAddr,
    samples: &[i16],
    sample_rate: u32,
) -> Result<(usize, u32), ToolError> {
    let samples_8k = downsample_to_8k(samples, sample_rate);
    let mulaw = smiths_core::pcm16_to_pcmu(&samples_8k);
    let mut stream = RtpStream::new();
    let total = mulaw.chunks(FRAME_SAMPLES).len();
    let mut frames_sent = 0usize;
    for (i, chunk) in mulaw.chunks(FRAME_SAMPLES).enumerate() {
        let pkt = stream.next_packet(PT_PCMU, chunk.to_vec(), FRAME_TICKS);
        ctx.media
            .send_packet(endpoint, remote, &pkt.encode())
            .await
            .map_err(|e| ToolError::Internal(format!("send_packet: {e}")))?;
        frames_sent += 1;
        // Pace frames in wall-clock — skip the sleep after the last
        // chunk so the tool returns promptly.
        if i + 1 < total {
            tokio::time::sleep(FRAME_INTERVAL).await;
        }
    }
    Ok((frames_sent, stream.ssrc()))
}

/// `speak` — synthesize text via an `ai.tts` plugin and stream it as
/// RTP (PCMU / 8 kHz / 20 ms frames) into a live call's media leg.
/// The tool returns once the last packet has been queued; pacing
/// happens in-process with a 20 ms sleep between frames so a real UA
/// hears the audio at real-time rate.
pub struct SpeakTool;

#[async_trait]
impl Tool for SpeakTool {
    fn name(&self) -> &'static str {
        "speak"
    }

    fn description(&self) -> &'static str {
        "Synthesize text via an `ai.tts` plugin and stream it as RTP \
         (PCMU / 8 kHz) into a live call's media leg. Returns after \
         the final packet is sent."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": { "type": "string", "description": "SIP Call-ID of a live dialog." },
                "plugin":  { "type": "string", "description": "ai.tts plugin name." },
                "text":    { "type": "string", "description": "Text to synthesize." },
                "voice":   { "type": "string", "description": "Voice id (optional)." },
                "controls":{ "type": "object", "description": "Provider-specific controls." }
            },
            "required": ["call_id", "plugin", "text"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = require_str(&args, "call_id")?;
        let plugin_name = require_str(&args, "plugin")?;
        let text = require_str(&args, "text")?;
        let (_, endpoint, remote) = live_media_leg(ctx, call_id)?;

        // Ask the TTS plugin for PCM16 LE @ 16 kHz (the common case).
        let provider = resolve_and_validate(ctx, plugin_name, "ai.tts", &args)?;
        let synth = provider
            .invoke(
                "synthesize",
                json!({
                    "text": text,
                    "voice": args.get("voice"),
                    "controls": args.get("controls"),
                    "output": {"codec": "pcm_s16le", "sample_rate": 16000},
                }),
            )
            .await
            .map_err(|e| ToolError::Internal(format!("synthesize: {e}")))?;

        let started = Instant::now();
        let (samples, sample_rate) = decode_pcm16(&synth)?;
        let (frames_sent, ssrc) =
            inject_pcm16_into_call(ctx, endpoint, remote, &samples, sample_rate).await?;
        Ok(json!({
            "call_id":     call_id,
            "plugin":      plugin_name,
            "frames_sent": frames_sent,
            "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "ssrc":        ssrc,
        }))
    }
}

/// Pull PCM16 bytes out of a `synthesize` result. Returns the decoded
/// `i16` samples plus the declared sample rate. Accepts both the flat
/// `{codec, sample_rate, audio_base64}` shape used by the in-tree
/// mock TTS and the nested `format` shape some third-party plugins
/// might emit.
pub(super) fn decode_pcm16(synth: &Value) -> Result<(Vec<i16>, u32), ToolError> {
    let b64 = synth
        .get("audio_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Internal("synthesize: missing audio_base64".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ToolError::Internal(format!("audio_base64 decode: {e}")))?;

    let pick = |key: &str| -> Option<&Value> {
        synth
            .get(key)
            .or_else(|| synth.get("format").and_then(|f| f.get(key)))
    };
    let codec = pick("codec").and_then(Value::as_str).unwrap_or("pcm_s16le");
    if codec != "pcm_s16le" {
        return Err(ToolError::Internal(format!(
            "speak requires PCM16 LE; plugin returned codec `{codec}`"
        )));
    }
    let sample_rate = u32::try_from(
        pick("sample_rate")
            .and_then(Value::as_u64)
            .unwrap_or(16_000),
    )
    .map_err(|_| ToolError::Internal("sample_rate out of range".into()))?;
    Ok((pcm16_from_le_bytes(&bytes), sample_rate))
}

/// Decode little-endian PCM16 bytes; a trailing odd byte is ignored.
pub(super) fn pcm16_from_le_bytes(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Naive rate-adapt to 8 kHz by decimation or fall-through. PCMU needs
/// exactly 8 kHz; anything else (16 kHz, 22.05 kHz, 44.1 kHz, 48 kHz)
/// is resampled with a crude pick-every-Nth. Good enough for a
/// walking-skeleton `speak`; a production build would plug a proper
/// resampler in at the same seam.
pub(super) fn downsample_to_8k(samples: &[i16], from_hz: u32) -> Vec<i16> {
    if from_hz <= 8_000 || samples.is_empty() {
        // Upsampling would need interpolation; not a path a modern TTS
        // takes. Return the samples unchanged and let the output be
        // slower than intended — preferable to a silent failure.
        return samples.to_vec();
    }
    // Integer-only decimation: index i of the output maps to sample
    // floor(i * from_hz / 8000) of the input.
    let from = u64::from(from_hz);
    let out_len = usize::try_from((samples.len() as u64) * 8_000 / from).unwrap_or(0);
    (0..out_len)
        .map(|i| {
            let src = usize::try_from((i as u64) * from / 8_000).unwrap_or(0);
            samples[src.min(samples.len() - 1)]
        })
        .collect()
}

/// Non-cryptographic SSRC for one injection stream — collision-free
/// across streams in a session thanks to the monotonic counter.
fn fresh_ssrc() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    nanos
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(c.wrapping_mul(0x0100_0001B))
}

fn fresh_seq() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static COUNTER: AtomicU16 = AtomicU16::new(0);
    COUNTER.fetch_add(101, Ordering::Relaxed).wrapping_add(1000)
}

/// `send_dtmf(call_id, digits)` — emit an RFC 4733 telephone-event
/// stream over an active call's media leg.
///
/// Each digit becomes a complete cadence: one `start` frame + one
/// intermediate per 20 ms held + three end-retransmits. An inter-
/// digit gap of 40 ms follows every digit so the receiver's detector
/// sees a distinct press.
pub struct SendDtmfTool;

/// Inter-digit silence so the receiver detector registers distinct
/// keypresses. 40 ms is conservative (some softphones need 30 ms).
const DTMF_INTERDIGIT_MS: u64 = 40;

#[async_trait]
impl Tool for SendDtmfTool {
    fn name(&self) -> &'static str {
        "send_dtmf"
    }

    fn description(&self) -> &'static str {
        "Send one or more DTMF digits as RFC 4733 telephone-event \
         packets into a live call's media leg."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id":  { "type": "string", "description": "SIP Call-ID of a live dialog." },
                "digits":   { "type": "string",
                              "description": "Digits to send. Allowed: 0–9, *, #, A–D, ! (flash)." },
                "duration_ms": { "type": "integer", "minimum": 20, "maximum": 2000,
                                 "description": "Per-digit duration. Default 160 ms (8 × 20 ms frames)." }
            },
            "required": ["call_id", "digits"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = require_str(&args, "call_id")?;
        let digits = require_str(&args, "digits")?;
        if digits.is_empty() {
            return Err(ToolError::InvalidArguments(
                "digits must be non-empty".into(),
            ));
        }
        let duration_ms = u32::try_from(
            args.get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(160),
        )
        .map_err(|_| ToolError::InvalidArguments("duration_ms out of range".into()))?;

        // Validate every digit before we touch the wire — partial
        // sends would leave the call in an ambiguous state.
        for d in digits.chars() {
            if smiths_core::digit_to_event_code(d).is_none() {
                return Err(ToolError::InvalidArguments(format!(
                    "unsupported DTMF digit `{d}`; allowed: 0-9 * # A-D !"
                )));
            }
        }
        let (_, endpoint, remote) = live_media_leg(ctx, call_id)?;

        let ssrc = fresh_ssrc();
        // One contiguous sequence + timestamp stream, bumped
        // per-packet. RFC 4733 keeps timestamp constant *within* a
        // keypress; the timestamp advances by `FRAME_SAMPLES * frames
        // + inter-digit-gap-samples` between successive digits.
        let mut seq: u16 = fresh_seq();
        let mut ts: u32 = 0;
        let mut total_packets = 0usize;

        let digits_vec: Vec<char> = digits.chars().collect();
        for (i, digit) in digits_vec.iter().enumerate() {
            let packets = smiths_core::dtmf::generate_keypress(*digit, duration_ms, ssrc, seq, ts);
            let frames_in_press = u16::try_from(packets.len()).unwrap_or(u16::MAX);
            seq = seq.wrapping_add(frames_in_press);
            // Bump ts by the keypress duration + the inter-digit gap.
            let press_samples = u32::from(
                smiths_core::dtmf::DTMF_GEN_FRAME_SAMPLES.saturating_mul(
                    u16::try_from(duration_ms / smiths_core::dtmf::DTMF_GEN_FRAME_MS)
                        .unwrap_or(u16::MAX),
                ),
            );
            let gap_samples = (smiths_core::dtmf::DTMF_GEN_CLOCK_RATE_HZ / 1_000)
                .saturating_mul(u32::try_from(DTMF_INTERDIGIT_MS).unwrap_or(0));
            ts = ts.wrapping_add(press_samples).wrapping_add(gap_samples);

            for pkt in &packets {
                ctx.media
                    .send_packet(endpoint, remote, &pkt.encode())
                    .await
                    .map_err(|e| ToolError::Internal(format!("send_packet: {e}")))?;
                total_packets += 1;
                tokio::time::sleep(Duration::from_millis(u64::from(
                    smiths_core::dtmf::DTMF_GEN_FRAME_MS,
                )))
                .await;
            }
            if i + 1 < digits_vec.len() {
                tokio::time::sleep(Duration::from_millis(DTMF_INTERDIGIT_MS)).await;
            }
        }

        Ok(json!({
            "call_id":       call_id,
            "digits":        digits,
            "duration_ms":   duration_ms,
            "packets_sent":  total_packets,
            "ssrc":          ssrc,
        }))
    }
}

/// `record_prompt(call_id, audio_base64, path)` — write a provided
/// PCM16 LE audio blob as a mono WAV under the operator-configured
/// prompt root. The IVR plugin (`ivr-kit`) then references the path
/// as `prompts/<basename>.wav` on its next playback action.
///
/// The engine doesn't tap a live call's media stream on demand, so
/// the tool takes `audio_base64` inline — the typical flow is a
/// `synthesize`-then-`record_prompt` pair, or an upload from the
/// operator's side.
pub struct RecordPromptTool;

#[async_trait]
impl Tool for RecordPromptTool {
    fn name(&self) -> &'static str {
        "record_prompt"
    }

    fn description(&self) -> &'static str {
        "Write a PCM16 LE audio blob as a WAV prompt under the \
         engine's configured prompt root. Returns the resolved path \
         + duration so the caller can reference it from an IVR \
         script's `prompt:` field."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id":      { "type": "string", "description": "Call-ID for provenance / audit logs." },
                "audio_base64": { "type": "string", "description": "PCM16 LE bytes, base64-encoded." },
                "sample_rate":  { "type": "integer", "minimum": 8000, "maximum": 48000,
                                   "description": "Sample rate of the audio (default 8000)." },
                "path":         { "type": "string",
                                   "description": "Relative path under the prompt root, e.g. `prompts/welcome.wav`." }
            },
            "required": ["call_id", "audio_base64", "path"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = require_str(&args, "call_id")?;
        let audio_b64 = require_str(&args, "audio_base64")?;
        let path = require_str(&args, "path")?;
        let sample_rate = u32::try_from(
            args.get("sample_rate")
                .and_then(Value::as_u64)
                .unwrap_or(8000),
        )
        .map_err(|_| ToolError::InvalidArguments("sample_rate out of range".into()))?;

        let Some(library) = &ctx.prompts else {
            return Err(ToolError::NotFound(
                "no prompt library is wired; configure `[media.prompts] root` \
                 to enable record_prompt"
                    .into(),
            ));
        };
        if path.contains("..") || std::path::Path::new(path).is_absolute() {
            return Err(ToolError::InvalidArguments(
                "path must be relative and contain no `..` segments".into(),
            ));
        }

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(audio_b64)
            .map_err(|e| ToolError::InvalidArguments(format!("audio_base64: {e}")))?;
        if bytes.len() % 2 != 0 {
            return Err(ToolError::InvalidArguments(
                "audio_base64 must decode to an even number of bytes (PCM16 LE)".into(),
            ));
        }
        let samples = pcm16_from_le_bytes(&bytes);

        let abs = if library.root().as_os_str().is_empty() {
            std::path::PathBuf::from(path)
        } else {
            library.root().join(path)
        };
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ToolError::Internal(format!("create prompt dir `{}`: {e}", parent.display()))
            })?;
        }
        let wav = smiths_media::encode_wav(sample_rate, &samples);
        std::fs::write(&abs, &wav)
            .map_err(|e| ToolError::Internal(format!("write `{}`: {e}", abs.display())))?;
        library.insert_raw(path, sample_rate, samples.clone());

        let duration_ms =
            u64::try_from(samples.len()).unwrap_or(u64::MAX) * 1000 / u64::from(sample_rate.max(1));
        Ok(json!({
            "call_id":     call_id,
            "path":        path,
            "absolute":    abs.display().to_string(),
            "sample_rate": sample_rate,
            "bytes":       wav.len(),
            "duration_ms": duration_ms,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::{FakeProvider, StaticRegistry, TestEngine};
    use std::sync::Arc;

    fn leg() -> (EndpointId, SocketAddr) {
        (EndpointId(7), "127.0.0.1:4000".parse().unwrap())
    }

    /// 16 kHz PCM16 silence of `ms` milliseconds, base64-encoded.
    fn pcm16_silence_b64(ms: usize) -> String {
        let bytes = vec![0u8; 16_000 * 2 * ms / 1000];
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn rtp_stream_marks_first_packet_and_advances_counters() {
        let mut s = RtpStream::new();
        let p0 = s.next_packet(PT_PCMU, vec![1; 160], FRAME_TICKS);
        let p1 = s.next_packet(PT_PCMU, vec![2; 160], FRAME_TICKS);
        assert!(p0.marker);
        assert!(!p1.marker);
        assert_eq!(p1.sequence, p0.sequence.wrapping_add(1));
        assert_eq!(p1.timestamp, p0.timestamp + FRAME_TICKS);
        assert_eq!(p0.ssrc, p1.ssrc);
        assert_eq!(p0.ssrc, s.ssrc());
        s.mark_gap();
        assert!(s.next_packet(PT_PCMU, vec![], FRAME_TICKS).marker);
        let a = RtpStream::new().ssrc();
        let b = RtpStream::new().ssrc();
        assert_ne!(a, b, "ssrcs must not collide across streams");
    }

    #[test]
    fn downsample_decimates_and_passes_8k_through() {
        let input: Vec<i16> = (0..320).collect();
        assert_eq!(downsample_to_8k(&input, 8_000), input);
        let half = downsample_to_8k(&input, 16_000);
        assert_eq!(half.len(), 160);
        assert_eq!(half[1], 2);
        assert_eq!(half[159], 318);
        let sixth = downsample_to_8k(&input, 48_000);
        assert_eq!(sixth.len(), 53);
        assert!(downsample_to_8k(&[], 16_000).is_empty());
    }

    #[test]
    fn decode_pcm16_accepts_flat_and_nested_format() {
        let b64 = base64::engine::general_purpose::STANDARD.encode([0x01, 0x00, 0xFF, 0xFF]);
        let (s, rate) = decode_pcm16(&json!({"audio_base64": b64, "sample_rate": 8000})).unwrap();
        assert_eq!(s, vec![1, -1]);
        assert_eq!(rate, 8000);
        let (_, rate) = decode_pcm16(&json!({
            "audio_base64": b64,
            "format": {"codec": "pcm_s16le", "sample_rate": 22050}
        }))
        .unwrap();
        assert_eq!(rate, 22050);
        let err = decode_pcm16(&json!({"audio_base64": b64, "codec": "opus"})).unwrap_err();
        assert!(matches!(err, ToolError::Internal(m) if m.contains("opus")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn speak_without_call_is_not_found() {
        let engine = TestEngine::new();
        let err = SpeakTool
            .call(
                json!({"call_id": "nope", "plugin": "tts", "text": "hi"}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn speak_streams_paced_pcmu_frames_to_the_call_leg() {
        // 60 ms of 16 kHz audio → 3 × 20 ms PCMU frames.
        let tts = FakeProvider::new(
            "tts",
            "ai.tts",
            &json!({"voices": [{"id": "alice"}]}),
            json!({"codec": "pcm_s16le", "sample_rate": 16000, "audio_base64": pcm16_silence_b64(60)}),
        );
        let engine = TestEngine::with_registry(Arc::new(StaticRegistry(vec![tts.clone()])));
        let (endpoint, remote) = leg();
        engine.dialog_created("c1", Some((endpoint, remote))).await;

        let out = SpeakTool
            .call(
                json!({"call_id": "c1", "plugin": "tts", "text": "hello", "voice": "alice"}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["frames_sent"], 3);
        let ssrc = out["ssrc"].as_u64().unwrap();

        let (method, params) = tts.last_call().unwrap();
        assert_eq!(method, "synthesize");
        assert_eq!(params["text"], "hello");
        assert_eq!(params["voice"], "alice");
        assert_eq!(params["output"]["codec"], "pcm_s16le");

        let sent = engine.fabric.sent();
        assert_eq!(sent.len(), 3);
        let packets: Vec<RtpPacket> = sent
            .iter()
            .map(|p| {
                assert_eq!(p.src, endpoint);
                assert_eq!(p.dest, remote);
                RtpPacket::decode(&p.bytes).expect("valid RTP")
            })
            .collect();
        assert!(packets[0].marker && !packets[1].marker);
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(p.payload_type, PT_PCMU);
            assert_eq!(p.payload.len(), FRAME_SAMPLES);
            assert_eq!(u64::from(p.ssrc), ssrc);
            let n16 = u16::try_from(i).unwrap();
            let n32 = u32::try_from(i).unwrap();
            assert_eq!(p.sequence, packets[0].sequence.wrapping_add(n16));
            assert_eq!(p.timestamp, packets[0].timestamp + FRAME_TICKS * n32);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn speak_rejects_unknown_voice_before_synthesis() {
        let tts = FakeProvider::new(
            "tts",
            "ai.tts",
            &json!({"voices": [{"id": "alice"}]}),
            json!({}),
        );
        let engine = TestEngine::with_registry(Arc::new(StaticRegistry(vec![tts.clone()])));
        engine.dialog_created("c1", Some(leg())).await;
        let err = SpeakTool
            .call(
                json!({"call_id": "c1", "plugin": "tts", "text": "hi", "voice": "bob"}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(m) if m.contains("bob")));
        assert!(tts.last_call().is_none());
        assert!(engine.fabric.sent().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_dtmf_emits_rfc4733_packets_per_digit() {
        let engine = TestEngine::new();
        let (endpoint, remote) = leg();
        engine.dialog_created("c1", Some((endpoint, remote))).await;
        let out = SendDtmfTool
            .call(
                json!({"call_id": "c1", "digits": "1#", "duration_ms": 40}),
                &engine.ctx,
            )
            .await
            .unwrap();
        let packets_sent = out["packets_sent"].as_u64().unwrap();
        assert!(packets_sent >= 2, "{out}");
        let sent = engine.fabric.sent();
        assert_eq!(u64::try_from(sent.len()).unwrap(), packets_sent);
        let first = RtpPacket::decode(&sent[0].bytes).unwrap();
        assert!(
            first.marker,
            "first packet of a keypress carries the marker"
        );
        assert_eq!(u64::from(first.ssrc), out["ssrc"].as_u64().unwrap());
        assert_eq!(sent[0].dest, remote);
        // Every packet is a 4-byte telephone-event payload.
        for p in &sent {
            let pkt = RtpPacket::decode(&p.bytes).unwrap();
            assert_eq!(pkt.payload.len(), 4, "{pkt:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn send_dtmf_validates_digits_before_touching_the_wire() {
        let engine = TestEngine::new();
        engine.dialog_created("c1", Some(leg())).await;
        let err = SendDtmfTool
            .call(json!({"call_id": "c1", "digits": "1Z"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(engine.fabric.sent().is_empty());
        let err = SendDtmfTool
            .call(json!({"call_id": "c1", "digits": ""}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_prompt_without_library_is_not_found() {
        let engine = TestEngine::new();
        let audio = base64::engine::general_purpose::STANDARD.encode([0u8, 0u8, 0u8, 0u8]);
        let err = RecordPromptTool
            .call(
                json!({"call_id": "x", "audio_base64": audio, "path": "prompts/x.wav"}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_prompt_rejects_absolute_and_dotdot_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = TestEngine::new();
        let ctx = engine
            .ctx
            .clone()
            .with_prompts(smiths_media::PromptLibrary::with_root(tmp.path()));
        let audio = base64::engine::general_purpose::STANDARD.encode([0u8, 0u8]);
        for path in ["/etc/passwd", "../outside.wav"] {
            let err = RecordPromptTool
                .call(
                    json!({"call_id": "x", "audio_base64": audio.clone(), "path": path}),
                    &ctx,
                )
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidArguments(_)), "{path}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_prompt_writes_wav_and_caches_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = smiths_media::PromptLibrary::with_root(tmp.path());
        let engine = TestEngine::new();
        let ctx = engine.ctx.clone().with_prompts(lib.clone());
        // Two samples of PCM16 LE = 4 bytes.
        let audio = base64::engine::general_purpose::STANDARD.encode([0u8, 0u8, 0u8, 0u8]);
        let out = RecordPromptTool
            .call(
                json!({
                    "call_id": "c1",
                    "audio_base64": audio,
                    "sample_rate": 8000,
                    "path": "prompts/hello.wav",
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["sample_rate"], 8000);
        assert!(
            tmp.path().join("prompts/hello.wav").exists(),
            "wav not written"
        );
        let prompt = lib.get("prompts/hello.wav").unwrap();
        assert_eq!(prompt.sample_rate, 8000);
        assert_eq!(prompt.samples.len(), 2);
    }
}
