//! Concrete tool implementations shipped in-box.
//!
//! Keep this file small. Any tool here is served identically by the
//! MCP and A2A adapters. New tools land as their own module when they
//! grow beyond a few lines.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use smiths_core::ai::{AiProvider, CapabilityDescriptor, validate_controls};

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
    use crate::tool::test_support::{default_config, empty_registry};
    use smiths_core::EventBus;
    use tokio_util::sync::CancellationToken;

    fn ctx_with_state() -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        (
            ToolContext::new(state, empty_registry(), default_config()),
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
        assert_eq!(reg.len(), 10);
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
            "reload_plugin",
        ] {
            assert!(reg.get(name).is_some(), "missing tool: {name}");
        }
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
