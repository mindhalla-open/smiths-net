//! Direct invocation of one named AI plugin: `synthesize`,
//! `transcribe`, `llm_chat`, `embed`. Each looks the plugin up,
//! validates the submitted controls against the capability
//! descriptor it advertised at load time, and forwards the call.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use smiths_core::ai::{AiProvider, CapabilityDescriptor, validate_controls};

use super::require_str;
use crate::tool::{Tool, ToolContext, ToolError};

/// Look up a plugin and validate `args.controls` (and, for `ai.tts`,
/// `args.voice`) against its descriptor for `capability`. Returns the
/// provider handle on success. Common prologue for every direct
/// AI-tool invocation.
pub(super) fn resolve_and_validate(
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
    if let Some(voice) = args.get("voice").and_then(Value::as_str)
        && !voice_is_known(descriptor, voice)
    {
        return Err(ToolError::InvalidArguments(format!(
            "voice `{voice}` unknown for plugin `{plugin_name}`"
        )));
    }
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

/// A voice is acceptable when the descriptor lists it, or when the
/// plugin declared no voice list at all (permissive).
fn voice_is_known(descriptor: &CapabilityDescriptor, voice: &str) -> bool {
    let Some(voices) = descriptor.extra.get("voices").and_then(Value::as_array) else {
        return true;
    };
    voices
        .iter()
        .any(|v| v.get("id").and_then(Value::as_str) == Some(voice))
}

/// `synthesize` — invoke a plugin's `ai.tts` synthesis, with strict
/// control validation before dispatch. Returns base64 PCM16 LE audio
/// the caller can stream into a call leg over RTP.
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
        let plugin_name = require_str(&args, "plugin")?;
        let text = require_str(&args, "text")?;
        let entry = resolve_and_validate(ctx, plugin_name, "ai.tts", &args)?;
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
                "codec":        { "type": "string",  "description": "Audio codec of audio_base64 (e.g. pcm_s16le, pcma, pcmu). Default pcm_s16le." },
                "controls":     { "type": "object",  "description": "Provider-specific controls." }
            },
            "required": ["plugin", "audio_base64"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let plugin_name = require_str(&args, "plugin")?;
        let audio = require_str(&args, "audio_base64")?;
        let entry = resolve_and_validate(ctx, plugin_name, "ai.asr", &args)?;
        let params = json!({
            "audio_base64": audio,
            "sample_rate": args.get("sample_rate"),
            "language": args.get("language"),
            "codec": args.get("codec"),
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
        let plugin_name = require_str(&args, "plugin")?;
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
        let plugin_name = require_str(&args, "plugin")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::{FakeProvider, StaticRegistry, TestEngine};

    fn engine_with(provider: Arc<FakeProvider>) -> TestEngine {
        TestEngine::with_registry(Arc::new(StaticRegistry(vec![provider])))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_tools_without_plugin_are_not_found() {
        let engine = TestEngine::new();
        assert!(matches!(
            SynthesizeTool
                .call(json!({"plugin": "nope", "text": "hi"}), &engine.ctx)
                .await,
            Err(ToolError::NotFound(_))
        ));
        assert!(matches!(
            TranscribeTool
                .call(json!({"plugin": "nope", "audio_base64": ""}), &engine.ctx)
                .await,
            Err(ToolError::NotFound(_))
        ));
        assert!(matches!(
            EmbedTool
                .call(json!({"plugin": "nope", "inputs": ["hi"]}), &engine.ctx)
                .await,
            Err(ToolError::NotFound(_))
        ));
        assert!(matches!(
            LlmChatTool
                .call(
                    json!({"plugin": "nope", "messages": [{"role": "user", "content": "hi"}]}),
                    &engine.ctx
                )
                .await,
            Err(ToolError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn synthesize_forwards_validated_params_to_the_plugin() {
        let tts = FakeProvider::new(
            "tts",
            "ai.tts",
            &json!({
                "voices": [{"id": "alice"}],
                "controls": {"speed": {"type": "number", "min": 0.5, "max": 2.0}}
            }),
            json!({"codec": "pcm_s16le", "sample_rate": 16000, "audio_base64": "AAAA"}),
        );
        let engine = engine_with(tts.clone());
        let out = SynthesizeTool
            .call(
                json!({"plugin": "tts", "text": "hi", "voice": "alice", "controls": {"speed": 1.5}}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["audio_base64"], "AAAA");
        let (method, params) = tts.last_call().unwrap();
        assert_eq!(method, "synthesize");
        assert_eq!(params["text"], "hi");
        assert_eq!(params["controls"]["speed"], 1.5);

        // Unknown control key is rejected by the descriptor schema.
        let err = SynthesizeTool
            .call(
                json!({"plugin": "tts", "text": "hi", "controls": {"pitch": 2}}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(m) if m.contains("pitch")));
        // Wrong capability is invalid arguments, not not-found.
        let err = TranscribeTool
            .call(
                json!({"plugin": "tts", "audio_base64": "AA=="}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(m) if m.contains("ai.asr")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_llm_chat_and_embed_round_trip() {
        let asr = FakeProvider::new(
            "asr",
            "ai.asr",
            &json!({}),
            json!({"text": "hello", "confidence": 0.9}),
        );
        let llm = FakeProvider::new(
            "llm",
            "ai.llm.chat",
            &json!({}),
            json!({"content": "hi there"}),
        );
        let emb = FakeProvider::new(
            "emb",
            "ai.embed",
            &json!({}),
            json!({"vectors": [[0.1, 0.2]]}),
        );
        let engine = TestEngine::with_registry(Arc::new(StaticRegistry(vec![
            asr.clone(),
            llm.clone(),
            emb.clone(),
        ])));

        let out = TranscribeTool
            .call(
                json!({"plugin": "asr", "audio_base64": "AAAA", "language": "en"}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["text"], "hello");
        assert_eq!(asr.last_call().unwrap().1["language"], "en");

        let out = LlmChatTool
            .call(
                json!({"plugin": "llm", "messages": [{"role": "user", "content": "hi"}]}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["content"], "hi there");
        assert_eq!(llm.last_call().unwrap().0, "chat");

        let out = EmbedTool
            .call(json!({"plugin": "emb", "inputs": ["a", "b"]}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(out["vectors"][0][1], 0.2);
        assert_eq!(emb.last_call().unwrap().1["inputs"], json!(["a", "b"]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embed_and_llm_chat_reject_empty_or_malformed_inputs() {
        let engine = TestEngine::new();
        let err = EmbedTool
            .call(json!({"plugin": "x", "inputs": []}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = EmbedTool
            .call(json!({"plugin": "x", "inputs": ["a", 1]}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = LlmChatTool
            .call(json!({"plugin": "x", "messages": []}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
