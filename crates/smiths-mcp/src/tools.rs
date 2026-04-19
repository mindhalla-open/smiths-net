//! Concrete tool implementations shipped in-box.
//!
//! Keep this file small. Any tool here is served identically by the
//! MCP and A2A adapters. New tools land as their own module when they
//! grow beyond a few lines.

#![allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::sync::Arc;

use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use smiths_core::ai::{AiProvider, CapabilityDescriptor, validate_controls};

use crate::control::CallPhase;
use crate::tool::{Tool, ToolContext, ToolError};

/// Build a fully-populated [`crate::ToolRegistry`] with the built-in
/// tool set.
#[must_use]
pub fn builtin_registry() -> crate::ToolRegistry {
    let mut reg = crate::ToolRegistry::new();
    reg.register(ListCallsTool);
    reg.register(GetCallStatusTool);
    reg.register(HealthTool);
    reg.register(ListAiProvidersTool);
    reg.register(DescribeProviderTool);
    reg.register(SynthesizeTool);
    reg.register(TranscribeTool);
    reg.register(LlmChatTool);
    reg.register(EmbedTool);
    reg.register(SpeakTool);
    reg.register(MakeCallTool);
    reg.register(EndCallTool);
    reg.register(ReloadPluginTool);
    reg
}

/// `list_calls` — return every dialog the engine currently knows
/// about, live or recently terminated.
pub struct ListCallsTool;

#[async_trait]
impl Tool for ListCallsTool {
    fn name(&self) -> &'static str {
        "list_calls"
    }

    fn description(&self) -> &'static str {
        "List active and recently-terminated calls known to the engine."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "phase": {
                    "type": "string",
                    "enum": ["live", "terminated", "all"],
                    "description": "Filter by lifecycle phase. Default: all."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let phase = args
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("all")
            .to_owned();
        let calls = ctx.state.list_calls();
        let filtered: Vec<_> = calls
            .into_iter()
            .filter(|c| match phase.as_str() {
                "live" => matches!(c.phase, crate::control::CallPhase::Live),
                "terminated" => matches!(c.phase, crate::control::CallPhase::Terminated),
                _ => true,
            })
            .collect();
        Ok(json!({ "calls": filtered, "count": filtered.len() }))
    }
}

/// `get_call_status` — details for one call by Call-ID.
pub struct GetCallStatusTool;

#[async_trait]
impl Tool for GetCallStatusTool {
    fn name(&self) -> &'static str {
        "get_call_status"
    }

    fn description(&self) -> &'static str {
        "Fetch the snapshot of a single call by Call-ID."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": {
                    "type": "string",
                    "description": "SIP Call-ID header value."
                }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = args
            .get("call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
        let snap = ctx
            .state
            .get_call(call_id)
            .ok_or_else(|| ToolError::NotFound(format!("call {call_id}")))?;
        Ok(serde_json::to_value(&snap).unwrap_or(Value::Null))
    }
}

/// `health` — uptime plus live-call count. Cheap liveness probe
/// exposed on every adapter.
pub struct HealthTool;

#[async_trait]
impl Tool for HealthTool {
    fn name(&self) -> &'static str {
        "health"
    }

    fn description(&self) -> &'static str {
        "Return engine uptime and the current live-call count."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": false })
    }

    async fn call(&self, _args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let calls = ctx.state.list_calls();
        let live = calls
            .iter()
            .filter(|c| matches!(c.phase, crate::control::CallPhase::Live))
            .count();
        Ok(json!({
            "status": "ok",
            "uptime_secs": ctx.state.uptime_secs(),
            "live_calls": live,
            "known_calls": calls.len()
        }))
    }
}

/// `list_ai_providers` — every registered AI plugin with a short
/// summary. Full descriptors via `describe_provider`.
pub struct ListAiProvidersTool;

#[async_trait]
impl Tool for ListAiProvidersTool {
    fn name(&self) -> &'static str {
        "list_ai_providers"
    }

    fn description(&self) -> &'static str {
        "List every loaded AI plugin (ai.tts / ai.asr / ai.llm / ai.embed) \
         with its declared capabilities."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "capability": {
                    "type": "string",
                    "description": "Filter: only return providers for this capability (e.g. 'ai.tts')."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let filter = args
            .get("capability")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let entries = ctx.plugins.snapshot();
        let providers: Vec<_> = entries
            .into_iter()
            .filter_map(|e| {
                let capabilities: Vec<_> = e
                    .capabilities()
                    .iter()
                    .filter(|d| filter.as_ref().is_none_or(|f| &d.capability == f))
                    .collect();
                if capabilities.is_empty() {
                    return None;
                }
                Some(json!({
                    "plugin":       e.name(),
                    "version":      e.version(),
                    "description":  e.description(),
                    "abi":          e.abi(),
                    "capabilities": capabilities.iter().map(|d| json!({
                        "capability": d.capability,
                        "model_id":   d.model_id,
                    })).collect::<Vec<_>>(),
                }))
            })
            .collect();
        Ok(json!({ "providers": providers, "count": providers.len() }))
    }
}

/// `describe_provider` — full capability descriptor for one plugin.
pub struct DescribeProviderTool;

#[async_trait]
impl Tool for DescribeProviderTool {
    fn name(&self) -> &'static str {
        "describe_provider"
    }

    fn description(&self) -> &'static str {
        "Return the complete capability descriptor(s) a plugin advertised at load time."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin": {
                    "type": "string",
                    "description": "Plugin name as declared in its manifest."
                }
            },
            "required": ["plugin"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        let entry = ctx
            .plugins
            .get(name)
            .ok_or_else(|| ToolError::NotFound(format!("plugin {name}")))?;
        Ok(json!({
            "plugin":       entry.name(),
            "version":      entry.version(),
            "description":  entry.description(),
            "abi":          entry.abi(),
            "capabilities": entry.capabilities(),
        }))
    }
}

/// `synthesize` — invoke a plugin's `ai.tts` synthesis, with strict
/// control validation before dispatch. Returns base64 PCM16 LE audio
/// the caller can stream into a call leg over RTP.
///
/// This is the first invocation tool; `transcribe` / `llm_chat` /
/// `embed` follow the same shape (look up plugin → validate against
/// descriptor → dispatch → return structured result).
pub struct SynthesizeTool;

#[async_trait]
impl Tool for SynthesizeTool {
    fn name(&self) -> &'static str {
        "synthesize"
    }

    fn description(&self) -> &'static str {
        "Invoke an `ai.tts` plugin to render text as audio. \
         Returns base64-encoded PCM16 LE bytes plus format metadata."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin":   { "type": "string", "description": "Plugin name." },
                "text":     { "type": "string", "description": "Text to synthesize." },
                "voice":    { "type": "string", "description": "Voice id (optional; uses default_voice)." },
                "controls": { "type": "object", "description": "Provider-specific controls." },
                "output":   { "type": "object", "description": "Requested output format (codec / sample_rate)." }
            },
            "required": ["plugin", "text"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let plugin_name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("text required".into()))?;

        let entry = ctx
            .plugins
            .get(plugin_name)
            .ok_or_else(|| ToolError::NotFound(format!("plugin {plugin_name}")))?;

        let descriptor = entry
            .capabilities()
            .iter()
            .find(|d| d.capability == "ai.tts")
            .ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "plugin `{plugin_name}` does not provide `ai.tts`"
                ))
            })?;

        // Voice validation — present voice must be in declared list.
        if let Some(voice) = args.get("voice").and_then(Value::as_str)
            && !voice_is_known(descriptor, voice)
        {
            return Err(ToolError::InvalidArguments(format!(
                "voice `{voice}` unknown for plugin `{plugin_name}`"
            )));
        }

        // Strict control validation against the descriptor's schema.
        if let Some(declared) = descriptor.extra.get("controls").and_then(Value::as_object) {
            let declared: std::collections::BTreeMap<String, Value> = declared
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let submitted = args.get("controls").cloned().unwrap_or(Value::Null);
            if let Err(e) = validate_controls(&declared, &submitted) {
                return Err(ToolError::InvalidArguments(format!(
                    "{}: {}",
                    e.field, e.reason
                )));
            }
        }

        // Dispatch to the plugin.
        let params = json!({
            "text": text,
            "voice": args.get("voice"),
            "controls": args.get("controls"),
            "output": args.get("output"),
        });
        entry
            .invoke("synthesize", params)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))
    }
}

fn voice_is_known(descriptor: &CapabilityDescriptor, voice: &str) -> bool {
    let Some(voices) = descriptor.extra.get("voices").and_then(Value::as_array) else {
        return true; // Plugin didn't declare any voice list — permissive.
    };
    voices
        .iter()
        .any(|v| v.get("id").and_then(Value::as_str) == Some(voice))
}

/// Look up a plugin and validate controls for a named capability.
/// Returns the provider handle on success. Common prologue for every
/// AI-tool invocation.
fn resolve_and_validate(
    ctx: &ToolContext,
    plugin_name: &str,
    capability: &str,
    args: &Value,
) -> Result<Arc<dyn AiProvider>, ToolError> {
    let entry = ctx
        .plugins
        .get(plugin_name)
        .ok_or_else(|| ToolError::NotFound(format!("plugin {plugin_name}")))?;
    let descriptor = entry
        .capabilities()
        .iter()
        .find(|d| d.capability == capability)
        .ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "plugin `{plugin_name}` does not provide `{capability}`"
            ))
        })?;
    if let Some(declared) = descriptor.extra.get("controls").and_then(Value::as_object) {
        let declared: std::collections::BTreeMap<String, Value> = declared
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let submitted = args.get("controls").cloned().unwrap_or(Value::Null);
        if let Err(e) = validate_controls(&declared, &submitted) {
            return Err(ToolError::InvalidArguments(format!(
                "{}: {}",
                e.field, e.reason
            )));
        }
    }
    Ok(entry)
}

/// `transcribe` — invoke an `ai.asr` plugin on a base64-encoded audio
/// buffer. Returns the transcript plus metadata the plugin supplies.
pub struct TranscribeTool;

#[async_trait]
impl Tool for TranscribeTool {
    fn name(&self) -> &'static str {
        "transcribe"
    }

    fn description(&self) -> &'static str {
        "Invoke an `ai.asr` plugin on a base64-encoded PCM16 audio \
         buffer. Returns the transcript and confidence."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin":       { "type": "string", "description": "ASR plugin name." },
                "audio_base64": { "type": "string", "description": "Base64 of PCM16 LE bytes." },
                "sample_rate":  { "type": "integer", "description": "Audio sample rate in Hz (default 8000)." },
                "language":     { "type": "string",  "description": "BCP-47 tag or `auto`." },
                "controls":     { "type": "object",  "description": "Provider-specific controls." }
            },
            "required": ["plugin", "audio_base64"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let plugin_name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        let audio = args
            .get("audio_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("audio_base64 required".into()))?;
        let entry = resolve_and_validate(ctx, plugin_name, "ai.asr", &args)?;

        let params = json!({
            "audio_base64": audio,
            "sample_rate": args.get("sample_rate"),
            "language": args.get("language"),
            "controls": args.get("controls"),
        });
        entry
            .invoke("transcribe", params)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))
    }
}

/// `llm_chat` — invoke an `ai.llm.chat` plugin with a messages array.
/// Returns the assistant message plus any token-usage metadata.
pub struct LlmChatTool;

#[async_trait]
impl Tool for LlmChatTool {
    fn name(&self) -> &'static str {
        "llm_chat"
    }

    fn description(&self) -> &'static str {
        "Invoke an `ai.llm.chat` plugin with a messages array \
         (`[{role, content}, ...]`). Non-streaming one-shot."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin":   { "type": "string", "description": "LLM plugin name." },
                "messages": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "role":    { "type": "string", "enum": ["system", "user", "assistant", "tool"] },
                            "content": { "type": "string" }
                        },
                        "required": ["role", "content"]
                    }
                },
                "controls": { "type": "object", "description": "Provider-specific controls." }
            },
            "required": ["plugin", "messages"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let plugin_name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        let messages = args
            .get("messages")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidArguments("messages required".into()))?;
        if messages.is_empty() {
            return Err(ToolError::InvalidArguments(
                "messages must not be empty".into(),
            ));
        }
        let entry = resolve_and_validate(ctx, plugin_name, "ai.llm.chat", &args)?;

        let params = json!({
            "messages": messages,
            "controls": args.get("controls"),
        });
        entry
            .invoke("chat", params)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))
    }
}

/// `embed` — invoke an `ai.embed` plugin with an array of text
/// inputs. Returns one vector per input plus the shared dimension.
pub struct EmbedTool;

#[async_trait]
impl Tool for EmbedTool {
    fn name(&self) -> &'static str {
        "embed"
    }

    fn description(&self) -> &'static str {
        "Invoke an `ai.embed` plugin on an array of text inputs. \
         Returns parallel-indexed embedding vectors."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin":   { "type": "string", "description": "Embedding plugin name." },
                "inputs":   {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "description": "Text strings to embed."
                },
                "controls": { "type": "object", "description": "Provider-specific controls." }
            },
            "required": ["plugin", "inputs"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let plugin_name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        let inputs = args
            .get("inputs")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidArguments("inputs required".into()))?;
        if inputs.is_empty() {
            return Err(ToolError::InvalidArguments(
                "inputs must not be empty".into(),
            ));
        }
        if inputs.iter().any(|i| !i.is_string()) {
            return Err(ToolError::InvalidArguments(
                "every input must be a string".into(),
            ));
        }
        let entry = resolve_and_validate(ctx, plugin_name, "ai.embed", &args)?;

        let params = json!({
            "inputs": inputs,
            "controls": args.get("controls"),
        });
        entry
            .invoke("embed", params)
            .await
            .map_err(|e| ToolError::Internal(e.to_string()))
    }
}

/// `speak` — synthesize text via an `ai.tts` plugin and stream it as
/// RTP (PCMU / 8 kHz / 20 ms frames) into a live call's media leg.
/// The tool returns once the last packet has been queued; pacing
/// happens in-process with a 20 ms sleep between frames so a real UA
/// hears the audio at real-time rate.
pub struct SpeakTool;

/// RFC 3551 PCMU payload type.
const PT_PCMU: u8 = 0;
/// PCMU frame cadence (ITU-T G.711, 8 kHz → 20 ms = 160 samples).
const FRAME_SAMPLES: usize = 160;
const FRAME_INTERVAL: Duration = Duration::from_millis(20);

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
        let call_id = args
            .get("call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
        let plugin_name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("text required".into()))?;

        // Locate the call's media leg.
        let snap = ctx
            .state
            .get_call(call_id)
            .ok_or_else(|| ToolError::NotFound(format!("call {call_id}")))?;
        if !matches!(snap.phase, CallPhase::Live) {
            return Err(ToolError::InvalidArguments(format!(
                "call {call_id} is not live (phase = {:?})",
                snap.phase
            )));
        }
        let endpoint = snap.media_endpoint.ok_or_else(|| {
            ToolError::InvalidArguments(format!("call {call_id} has no media endpoint"))
        })?;
        let remote = snap.remote_rtp.ok_or_else(|| {
            ToolError::InvalidArguments(format!("call {call_id} has no remote RTP address"))
        })?;

        // Ask the TTS plugin for PCM16 LE @ 16 kHz (the common case).
        let provider = ctx
            .plugins
            .get(plugin_name)
            .ok_or_else(|| ToolError::NotFound(format!("plugin {plugin_name}")))?;
        let descriptor = provider
            .capabilities()
            .iter()
            .find(|d| d.capability == "ai.tts")
            .ok_or_else(|| {
                ToolError::InvalidArguments(format!(
                    "plugin `{plugin_name}` does not provide `ai.tts`"
                ))
            })?
            .clone();

        // Strict-validate controls against the descriptor, matching
        // the `synthesize` tool.
        if let Some(declared) = descriptor.extra.get("controls").and_then(Value::as_object) {
            let declared: std::collections::BTreeMap<String, Value> = declared
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let submitted = args.get("controls").cloned().unwrap_or(Value::Null);
            if let Err(e) = validate_controls(&declared, &submitted) {
                return Err(ToolError::InvalidArguments(format!(
                    "{}: {}",
                    e.field, e.reason
                )));
            }
        }

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

        let (samples, sample_rate) = decode_pcm16(&synth)?;
        let samples_8k = downsample_to_8k(&samples, sample_rate);
        let mulaw = smiths_core::pcm16_to_pcmu(&samples_8k);

        // Stream the μ-law bytes as 20 ms RTP frames.
        let ssrc = fresh_ssrc();
        let mut seq: u16 = fresh_seq();
        let mut ts: u32 = 0;
        let mut frames_sent = 0usize;
        let started = Instant::now();

        for (i, chunk) in mulaw.chunks(FRAME_SAMPLES).enumerate() {
            let pkt = smiths_core::RtpPacket {
                marker: i == 0,
                payload_type: PT_PCMU,
                sequence: seq,
                timestamp: ts,
                ssrc,
                payload: chunk.to_vec(),
            };
            let bytes = pkt.encode();
            ctx.media
                .send_packet(endpoint, remote, &bytes)
                .await
                .map_err(|e| ToolError::Internal(format!("send_packet: {e}")))?;
            frames_sent += 1;
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(FRAME_SAMPLES as u32);
            // Pace frames in wall-clock — skip the sleep after the
            // last chunk so the tool returns promptly.
            if i + 1 < mulaw.chunks(FRAME_SAMPLES).len() {
                tokio::time::sleep(FRAME_INTERVAL).await;
            }
        }

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
fn decode_pcm16(synth: &Value) -> Result<(Vec<i16>, u32), ToolError> {
    let b64 = synth
        .get("audio_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Internal("synthesize: missing audio_base64".into()))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ToolError::Internal(format!("audio_base64 decode: {e}")))?;

    let pick_str = |key: &str| -> Option<&str> {
        synth.get(key).and_then(Value::as_str).or_else(|| {
            synth
                .get("format")
                .and_then(|f| f.get(key))
                .and_then(Value::as_str)
        })
    };
    let pick_u64 = |key: &str| -> Option<u64> {
        synth.get(key).and_then(Value::as_u64).or_else(|| {
            synth
                .get("format")
                .and_then(|f| f.get(key))
                .and_then(Value::as_u64)
        })
    };

    let codec = pick_str("codec").unwrap_or("pcm_s16le");
    if codec != "pcm_s16le" {
        return Err(ToolError::Internal(format!(
            "speak requires PCM16 LE; plugin returned codec `{codec}`"
        )));
    }
    let sample_rate = u32::try_from(pick_u64("sample_rate").unwrap_or(16_000))
        .map_err(|_| ToolError::Internal("sample_rate out of range".into()))?;
    let samples: Vec<i16> = bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok((samples, sample_rate))
}

/// Naive rate-adapt to 8 kHz by decimation or fall-through. PCMU needs
/// exactly 8 kHz; anything else (16 kHz, 22.05 kHz, 44.1 kHz, 48 kHz)
/// is resampled with a crude pick-every-Nth. Good enough for a
/// walking-skeleton `speak`; a production build would plug a proper
/// resampler in at the same seam.
fn downsample_to_8k(samples: &[i16], from_hz: u32) -> Vec<i16> {
    if from_hz == 8_000 {
        return samples.to_vec();
    }
    if from_hz < 8_000 {
        // Upsample would need interpolation; not a path a modern TTS
        // takes. Return the samples unchanged and let the output be
        // slower than intended — preferable to a silent failure.
        return samples.to_vec();
    }
    let step = f64::from(from_hz) / 8_000.0;
    let out_len = ((samples.len() as f64) / step).floor() as usize;
    (0..out_len)
        .map(|i| {
            let src = ((i as f64) * step).floor() as usize;
            samples[src.min(samples.len() - 1)]
        })
        .collect()
}

/// Non-cryptographic SSRC for one speak invocation — collision-free
/// across calls in a session thanks to the monotonic counter.
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

/// `make_call(target)` — place an outbound SIP INVITE to a remote URI
/// via the engine's UAC. Returns `{call_id}` once the dialog is
/// established (200 OK + ACK).
pub struct MakeCallTool;

#[async_trait]
impl Tool for MakeCallTool {
    fn name(&self) -> &'static str {
        "make_call"
    }

    fn description(&self) -> &'static str {
        "Place an outbound SIP INVITE to `target` (a `sip:user@host[:port]` URI) \
         and return the Call-ID of the established dialog."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "SIP URI to dial (e.g. `sip:alice@10.0.0.1:5060`)."
                }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let target = args
            .get("target")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("target required".into()))?;
        let originator = ctx.originator.as_ref().ok_or_else(|| {
            ToolError::NotFound("no outbound-call originator configured; enable the SIP UAC".into())
        })?;
        let call_id = originator
            .place_call(target)
            .await
            .map_err(map_call_error)?;
        Ok(json!({ "call_id": call_id, "target": target }))
    }
}

/// `end_call(call_id)` — tear down an outbound dialog previously
/// established by `make_call`.
pub struct EndCallTool;

#[async_trait]
impl Tool for EndCallTool {
    fn name(&self) -> &'static str {
        "end_call"
    }

    fn description(&self) -> &'static str {
        "Send BYE on an outbound dialog previously created by `make_call`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": {
                    "type": "string",
                    "description": "Call-ID returned from `make_call`."
                }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = args
            .get("call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
        let originator = ctx.originator.as_ref().ok_or_else(|| {
            ToolError::NotFound("no outbound-call originator configured; enable the SIP UAC".into())
        })?;
        originator.hangup(call_id).await.map_err(map_call_error)?;
        Ok(json!({ "call_id": call_id, "status": "ended" }))
    }
}

/// Translate a `CallError` into a `ToolError` the adapters already
/// know how to wire.
fn map_call_error(e: smiths_core::call::CallError) -> ToolError {
    use smiths_core::call::CallError;
    match e {
        CallError::InvalidTarget(m) => ToolError::InvalidArguments(m),
        CallError::NotFound(m) => ToolError::NotFound(m),
        CallError::Rejected { status, reason } => {
            ToolError::Internal(format!("peer rejected: {status} {reason}"))
        }
        CallError::Timeout { millis } => ToolError::Internal(format!("timeout after {millis} ms")),
        CallError::Internal(m) => ToolError::Internal(m),
    }
}

/// `reload_plugin` — drain a loaded plugin's sidecar, re-parse its
/// manifest, and re-spawn. Useful when a plugin file was edited on
/// disk without restarting the engine.
pub struct ReloadPluginTool;

#[async_trait]
impl Tool for ReloadPluginTool {
    fn name(&self) -> &'static str {
        "reload_plugin"
    }

    fn description(&self) -> &'static str {
        "Re-spawn one loaded AI plugin from disk (drains the current \
         sidecar, re-runs the describe_capabilities handshake)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin": { "type": "string", "description": "Plugin name (as registered)." }
            },
            "required": ["plugin"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = args
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("plugin required".into()))?;
        match ctx.plugins.reload(name).await {
            Ok(()) => Ok(json!({ "plugin": name, "status": "reloaded" })),
            Err(e) => Err(ToolError::Internal(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use crate::tool::test_support::{default_config, empty_registry, null_media};
    use smiths_core::EventBus;
    use tokio_util::sync::CancellationToken;

    fn ctx_with_state() -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        (
            ToolContext::new(state, empty_registry(), default_config(), null_media()),
            cancel,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_calls_defaults_to_all() {
        let (ctx, _c) = ctx_with_state();
        let out = ListCallsTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_call_status_missing_returns_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = GetCallStatusTool
            .call(json!({"call_id": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_returns_ok_schema() {
        let (ctx, _c) = ctx_with_state();
        let out = HealthTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out["status"], "ok");
        assert!(out["uptime_secs"].is_number());
    }

    #[test]
    fn registry_contains_builtins() {
        let reg = builtin_registry();
        assert_eq!(reg.len(), 13);
        for name in [
            "list_calls",
            "get_call_status",
            "health",
            "list_ai_providers",
            "describe_provider",
            "synthesize",
            "transcribe",
            "llm_chat",
            "embed",
            "speak",
            "make_call",
            "end_call",
            "reload_plugin",
        ] {
            assert!(reg.get(name).is_some(), "missing tool: {name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn make_call_without_originator_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = MakeCallTool
            .call(json!({"target": "sip:a@127.0.0.1"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_call_without_originator_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = EndCallTool
            .call(json!({"call_id": "x"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn speak_without_call_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = SpeakTool
            .call(
                json!({"call_id": "nope", "plugin": "tts", "text": "hi"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embed_without_plugin_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = EmbedTool
            .call(json!({"plugin": "nope", "inputs": ["hi"]}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embed_rejects_empty_inputs() {
        let (ctx, _c) = ctx_with_state();
        let err = EmbedTool
            .call(json!({"plugin": "x", "inputs": []}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_plugin_unknown_is_error() {
        let (ctx, _c) = ctx_with_state();
        let err = ReloadPluginTool
            .call(json!({"plugin": "ghost"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn synthesize_without_plugin_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = SynthesizeTool
            .call(json!({"plugin": "nope", "text": "hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_without_plugin_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = TranscribeTool
            .call(json!({"plugin": "nope", "audio_base64": ""}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn llm_chat_rejects_empty_messages() {
        let (ctx, _c) = ctx_with_state();
        let err = LlmChatTool
            .call(json!({"plugin": "x", "messages": []}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_ai_providers_empty_by_default() {
        let (ctx, _c) = ctx_with_state();
        let out = ListAiProvidersTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn describe_provider_missing_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = DescribeProviderTool
            .call(json!({"plugin": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }
}
