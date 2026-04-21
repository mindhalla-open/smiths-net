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
use smiths_core::ai::{
    AiDispatcher, AiProvider, CapabilityDescriptor, DispatchError, validate_controls,
};

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
    reg.register(ListCdrTool);
    reg.register(SendDtmfTool);
    reg.register(TranslateTool);
    reg.register(TranscribeCallTool);
    reg.register(SummarizeCallTool);
    reg.register(SearchCallsSemanticTool);
    reg.register(PutScriptTool);
    reg
}

/// `put_script(name, source, engine)` — slice 4.1. Pushes a new
/// script into a loaded script-tier plugin's directory and kicks a
/// reload. Scoped narrowly: `name` must match a plugin already
/// loaded; the engine writes the new body to the plugin's entry
/// file atomically (tempfile + rename) and fires the standard
/// hot-reload path — the same one file-watcher edits go through,
/// so a failing swap surfaces the ordinary rollback.
pub struct PutScriptTool;

#[async_trait]
impl Tool for PutScriptTool {
    fn name(&self) -> &'static str {
        "put_script"
    }

    fn description(&self) -> &'static str {
        "Push a new script body into a loaded script-tier plugin. \
         The engine writes atomically to the plugin's entry file and \
         reloads; the previous version is retained for auto-rollback \
         on 5 consecutive errors."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":   { "type": "string", "description": "Plugin name (must already be loaded)." },
                "source": { "type": "string", "description": "New script body." },
                "engine": {
                    "type": "string",
                    "enum": ["rhai"],
                    "description": "DSL engine. Only `rhai` today."
                }
            },
            "required": ["name", "source"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("name required".into()))?;
        let source = args
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("source required".into()))?;
        let engine = args.get("engine").and_then(Value::as_str).unwrap_or("rhai");
        if engine != "rhai" {
            return Err(ToolError::InvalidArguments(format!(
                "unsupported engine `{engine}`; only `rhai` is wired today"
            )));
        }

        let entry = ctx.plugins.reload_script_source(name, source).await;
        match entry {
            Ok(()) => Ok(json!({
                "name":   name,
                "engine": engine,
                "status": "reloaded"
            })),
            Err(e) => {
                let msg = e.to_string();
                // The plugin crate phrases "plugin not loaded or not
                // script-backed" / "not supported by this registry"
                // — both mean the resource the caller asked for
                // isn't there, which is `NotFound` rather than an
                // internal error.
                let missing = msg.contains("not loaded")
                    || msg.contains("not script")
                    || msg.contains("not supported");
                if missing {
                    Err(ToolError::NotFound(msg))
                } else {
                    Err(ToolError::Internal(msg))
                }
            }
        }
    }
}

/// `search_calls_semantic(query, k)` — slice 3.4. Embeds the
/// natural-language `query` via the `ai.embed` capability, then runs
/// a top-k search against the wired `[storage.vector]` backend.
/// Returns every hit's `id`, `score`, and indexed `metadata` — the
/// caller typically seeded the metadata with `{call_id, transcript,
/// started_at_unix, ...}` when upserting, so a hit is immediately
/// actionable without a second lookup.
pub struct SearchCallsSemanticTool;

#[async_trait]
impl Tool for SearchCallsSemanticTool {
    fn name(&self) -> &'static str {
        "search_calls_semantic"
    }

    fn description(&self) -> &'static str {
        "Embed `query` via `ai.embed` and return the top-`k` nearest \
         indexed records from the vector store. Metadata attached at \
         upsert time (call_id, transcript, ...) rides back on every hit."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Natural-language query." },
                "k":     { "type": "integer", "minimum": 1, "maximum": 50,
                           "description": "Top-k hits (default 5)." },
                "plugin": { "type": "string",
                            "description": "Override embed plugin (optional; dispatcher picks otherwise)." }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let started = Instant::now();
        let result = search_calls_semantic_inner(&args, ctx).await;
        observe_pipeline(ctx, "search_calls_semantic", started.elapsed());
        result
    }
}

async fn search_calls_semantic_inner(args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments("query required".into()))?;
    if query.trim().is_empty() {
        return Err(ToolError::InvalidArguments(
            "query must not be empty".into(),
        ));
    }
    let k = usize::try_from(args.get("k").and_then(Value::as_u64).unwrap_or(5))
        .map_err(|_| ToolError::InvalidArguments("k out of range".into()))?;

    let Some(vector_store) = &ctx.vector else {
        return Err(ToolError::NotFound(
            "no `[storage.vector]` backend is wired; \
             set `backend = \"memory\"` (or `\"sidecar\"`) to enable semantic search"
                .into(),
        ));
    };

    let dispatcher = smiths_core::AiDispatcher::new(Arc::clone(&ctx.plugins));
    let embed_params = json!({ "inputs": [query] });
    let embed = match dispatcher.invoke("ai.embed", "embed", embed_params).await {
        Ok(v) => v,
        Err(smiths_core::DispatchError::NoProvider(cap)) => {
            return Err(ToolError::NotFound(format!(
                "no `{cap}` provider is loaded — install an ai.embed plugin"
            )));
        }
        Err(e @ smiths_core::DispatchError::AllFailed { .. }) => {
            return Err(ToolError::Internal(format!("embed failed: {e}")));
        }
    };
    let vector = extract_first_embedding(&embed)
        .ok_or_else(|| ToolError::Internal(format!("embed response shape unexpected: {embed}")))?;

    let hits = vector_store
        .search(&vector, k)
        .map_err(|e| ToolError::Internal(format!("vector search: {e}")))?;

    Ok(json!({
        "query": query,
        "k":     k,
        "count": hits.len(),
        "hits":  hits,
    }))
}

/// Pull the first embedding vector out of whatever shape the
/// `ai.embed` plugin returned. Accepts `{vectors: [[...], ...]}`
/// (in-tree mock), `{embeddings: [...]}` (OpenAI-compat), and
/// `{data: [{embedding: [...]}]}` (raw `OpenAI`).
fn extract_first_embedding(v: &Value) -> Option<Vec<f32>> {
    let arr = v
        .get("vectors")
        .and_then(Value::as_array)
        .and_then(|a| a.first().and_then(Value::as_array))
        .or_else(|| {
            v.get("embeddings")
                .and_then(Value::as_array)
                .and_then(|a| a.first().and_then(Value::as_array))
        })
        .or_else(|| {
            v.get("data")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|e| e.get("embedding"))
                .and_then(Value::as_array)
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        out.push(x.as_f64()? as f32);
    }
    Some(out)
}

/// Resolve the audio payload for the pipeline tools. Inline
/// `audio_base64` wins; otherwise the recording store is consulted;
/// otherwise a `NotFound` with a targeted error surfaces so the
/// caller sees *exactly* which config knob is missing.
fn resolve_audio_base64(
    call_id: &str,
    args: &Value,
    ctx: &ToolContext,
) -> Result<String, ToolError> {
    use base64::Engine as _;

    if let Some(inline) = args.get("audio_base64").and_then(Value::as_str) {
        return Ok(inline.to_owned());
    }
    let Some(store) = &ctx.recording else {
        return Err(ToolError::NotFound(
            "no `[storage.recording]` backend is wired; \
             pass `audio_base64` inline on this call"
                .into(),
        ));
    };
    let bytes = store.get(call_id).map_err(|e| match e {
        smiths_core::storage::StorageError::NotFound(_) => ToolError::NotFound(format!(
            "no recording on disk for call `{call_id}`; \
             pass `audio_base64` inline or check the retention window"
        )),
        other => ToolError::Internal(format!("recording lookup: {other}")),
    })?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Observe one pipeline's end-to-end wall clock. Falls through to a
/// fresh `Metrics::noop()` when the context doesn't carry a handle
/// (older tests construct `ToolContext` without one); the observation
/// is still recorded, just onto a registry nobody is reading.
fn observe_pipeline(ctx: &ToolContext, pipeline: &str, elapsed: std::time::Duration) {
    let target = ctx
        .metrics
        .clone()
        .unwrap_or_else(smiths_core::Metrics::noop);
    target
        .ai_pipeline_duration
        .get_or_create(&smiths_core::metrics::AiPipelineLabel {
            pipeline: pipeline.to_owned(),
        })
        .observe(elapsed.as_secs_f64());
}

/// `transcribe_call(call_id)` — slice 3.3. Routes through the AI
/// dispatcher at capability `ai.asr`, so the caller doesn't pick a
/// plugin. The audio source is the call's recording — which today
/// is only available when the operator passed `audio_base64` on the
/// arguments payload. A dedicated recording store (slice 3.4) will
/// let this tool self-resolve audio from the call id alone.
pub struct TranscribeCallTool;

#[async_trait]
impl Tool for TranscribeCallTool {
    fn name(&self) -> &'static str {
        "transcribe_call"
    }

    fn description(&self) -> &'static str {
        "Transcribe a call's audio via the best-fit `ai.asr` provider. \
         Pass `audio_base64` alongside `call_id` until the recording \
         store lands (slice 3.4); routes through the AI dispatcher."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id":      { "type": "string", "description": "SIP Call-ID." },
                "audio_base64": { "type": "string",
                                  "description": "PCM16 LE audio bytes, base64-encoded. Required until \
                                                  the recording store lands." },
                "sample_rate":  { "type": "integer", "description": "Sample rate of the audio (default 8000)." },
                "language":     { "type": "string",  "description": "BCP-47 tag or `auto`." },
                "controls":     { "type": "object",  "description": "Provider-specific controls." }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let started = Instant::now();
        let result = transcribe_call_inner(&args, ctx).await;
        observe_pipeline(ctx, "transcribe_call", started.elapsed());
        result
    }
}

async fn transcribe_call_inner(args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
    let call_id = args
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
    let audio = resolve_audio_base64(call_id, args, ctx)?;

    let dispatcher = smiths_core::AiDispatcher::new(Arc::clone(&ctx.plugins));
    let params = json!({
        "audio_base64": audio,
        "sample_rate":  args.get("sample_rate"),
        "language":     args.get("language"),
        "controls":     args.get("controls"),
    });
    match dispatcher.invoke("ai.asr", "transcribe", params).await {
        Ok(v) => {
            let text = v
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Ok(json!({
                "call_id":   call_id,
                "transcript": text,
                "raw":        v,
            }))
        }
        Err(smiths_core::DispatchError::NoProvider(cap)) => Err(ToolError::NotFound(format!(
            "no `{cap}` provider is loaded — install an ai.asr plugin"
        ))),
        Err(e @ smiths_core::DispatchError::AllFailed { .. }) => {
            Err(ToolError::Internal(e.to_string()))
        }
    }
}

/// `summarize_call(call_id)` — slice 3.3 flagship composite tool.
/// Pipes ASR over the dispatcher's `ai.asr` lane, then hands the
/// transcript to `ai.llm.chat` with a summary prompt. Returns both
/// the transcript (so the agent doesn't re-transcribe) and the
/// summary text.
pub struct SummarizeCallTool;

#[async_trait]
impl Tool for SummarizeCallTool {
    fn name(&self) -> &'static str {
        "summarize_call"
    }

    fn description(&self) -> &'static str {
        "Transcribe a call via `ai.asr`, then summarize the transcript \
         via `ai.llm.chat`. Returns `{transcript, summary}`. Pass \
         `audio_base64` until the recording store lands (slice 3.4)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id":      { "type": "string", "description": "SIP Call-ID." },
                "audio_base64": { "type": "string",
                                  "description": "PCM16 LE audio bytes, base64-encoded." },
                "sample_rate":  { "type": "integer", "description": "Sample rate (default 8000)." },
                "language":     { "type": "string",  "description": "BCP-47 tag or `auto`." },
                "max_sentences":{ "type": "integer", "minimum": 1, "maximum": 20,
                                  "description": "Target summary length (default 3)." }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let started = Instant::now();
        let result = summarize_call_inner(&args, ctx).await;
        observe_pipeline(ctx, "summarize_call", started.elapsed());
        result
    }
}

async fn summarize_call_inner(args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
    let call_id = args
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
    let audio = resolve_audio_base64(call_id, args, ctx)?;
    let max_sentences = args
        .get("max_sentences")
        .and_then(Value::as_u64)
        .unwrap_or(3);

    let dispatcher = smiths_core::AiDispatcher::new(Arc::clone(&ctx.plugins));

    let asr_params = json!({
        "audio_base64": audio,
        "sample_rate":  args.get("sample_rate"),
        "language":     args.get("language"),
    });
    let transcript = match dispatcher.invoke("ai.asr", "transcribe", asr_params).await {
        Ok(v) => v
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        Err(smiths_core::DispatchError::NoProvider(cap)) => {
            return Err(ToolError::NotFound(format!(
                "no `{cap}` provider is loaded — install an ai.asr plugin"
            )));
        }
        Err(e @ smiths_core::DispatchError::AllFailed { .. }) => {
            return Err(ToolError::Internal(format!("asr failed: {e}")));
        }
    };
    if transcript.trim().is_empty() {
        return Err(ToolError::Internal(
            "transcription returned empty text; cannot summarize".into(),
        ));
    }

    let system = format!(
        "You are a concise meeting-note taker. Summarize the following \
         phone-call transcript in {max_sentences} sentence(s) or fewer. \
         Preserve action items and proper nouns. Reply with the summary \
         text only — no preamble."
    );
    let llm_params = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user",   "content": transcript.clone()},
        ]
    });
    let summary = match dispatcher.invoke("ai.llm.chat", "chat", llm_params).await {
        Ok(v) => extract_chat_content(&v).unwrap_or_else(|| v.to_string()),
        Err(smiths_core::DispatchError::NoProvider(cap)) => {
            return Err(ToolError::NotFound(format!(
                "no `{cap}` provider is loaded — install an ai.llm.chat plugin"
            )));
        }
        Err(e @ smiths_core::DispatchError::AllFailed { .. }) => {
            return Err(ToolError::Internal(format!("llm failed: {e}")));
        }
    };

    Ok(json!({
        "call_id":    call_id,
        "transcript": transcript,
        "summary":    summary,
    }))
}

/// `translate` — render `text` into language `to` by routing through
/// the [`AiDispatcher`] at capability `ai.llm.chat` (slice 3.1, P4).
///
/// The caller does not pick a plugin; the dispatcher selects the
/// highest-priority healthy `ai.llm.chat` provider, failing over on
/// timeout or error. This is the canonical "built on top of the
/// dispatcher" tool — other multi-plugin helpers should follow the
/// same shape instead of hand-rolling provider selection.
pub struct TranslateTool;

#[async_trait]
impl Tool for TranslateTool {
    fn name(&self) -> &'static str {
        "translate"
    }

    fn description(&self) -> &'static str {
        "Translate `text` into language `to` (BCP-47 tag or natural name) \
         via the best-fit `ai.llm.chat` provider. Routes through the AI \
         dispatcher with automatic fail-over."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Source text to translate." },
                "to":   { "type": "string",
                          "description": "Target language (BCP-47 like `es-MX` or a name like `Spanish`)." },
                "from": { "type": "string",
                          "description": "Optional source-language hint. Omit for auto-detect." }
            },
            "required": ["text", "to"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("text required".into()))?;
        let to = args
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("to required".into()))?;
        let from = args.get("from").and_then(Value::as_str);

        let source_clause = from.map_or_else(
            || "Detect the source language.".to_owned(),
            |f| format!("The source language is {f}."),
        );
        let prompt = format!(
            "{source_clause} Translate the following text into {to}. \
             Return only the translation — no commentary, no quoting, no \
             language tags.\n\nText:\n{text}"
        );

        let dispatcher = AiDispatcher::new(Arc::clone(&ctx.plugins));
        let params = json!({
            "messages": [
                {"role": "system", "content":
                    "You are a professional translator. Preserve formatting \
                     and proper nouns. Reply with the translated text only."},
                {"role": "user", "content": prompt}
            ]
        });
        match dispatcher.invoke("ai.llm.chat", "chat", params).await {
            Ok(v) => {
                let translated = extract_chat_content(&v).unwrap_or_else(|| v.to_string());
                Ok(json!({
                    "text":         text,
                    "to":           to,
                    "from":         from,
                    "translated":   translated,
                    "raw":          v,
                }))
            }
            Err(DispatchError::NoProvider(cap)) => Err(ToolError::NotFound(format!(
                "no `{cap}` provider is loaded — install an ai.llm.chat plugin"
            ))),
            Err(e @ DispatchError::AllFailed { .. }) => Err(ToolError::Internal(e.to_string())),
        }
    }
}

/// Pull the assistant's textual reply out of whatever shape the
/// `ai.llm.chat` plugin returned. Accepts both the direct
/// `{content: "..."}` and nested `{message: {content: "..."}}` forms —
/// different backends (Ollama, OpenAI-compat) normalize differently.
fn extract_chat_content(v: &Value) -> Option<String> {
    if let Some(s) = v.get("content").and_then(Value::as_str) {
        return Some(s.to_owned());
    }
    if let Some(s) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        return Some(s.to_owned());
    }
    if let Some(s) = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        return Some(s.to_owned());
    }
    None
}

/// `list_cdr` — bounded query over the CDR store (slice 2.3, P23).
///
/// Returns `{count, rows: [...]}` where each row is a
/// [`smiths_core::storage::CallDetailRecord`]. When no backend is
/// wired, `count` is 0 and `rows` is empty — operators tell the
/// difference from "genuinely no calls yet" by reading
/// `[storage] backend` off `config://current`.
pub struct ListCdrTool;

#[async_trait]
impl Tool for ListCdrTool {
    fn name(&self) -> &'static str {
        "list_cdr"
    }

    fn description(&self) -> &'static str {
        "List call-detail records. Optional filters: time range, From/To substring, result."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "since_unix":  { "type": "integer", "description": "Only CDRs where started_at_unix >= this." },
                "until_unix":  { "type": "integer", "description": "Only CDRs where started_at_unix <= this." },
                "from_like":   { "type": "string",  "description": "Case-insensitive substring match on From URI." },
                "to_like":     { "type": "string",  "description": "Case-insensitive substring match on To URI." },
                "result":      { "type": "string",  "description": "Exact match on result (e.g. 'answered')." },
                "limit":       { "type": "integer", "minimum": 1, "maximum": 1000,
                                  "description": "Max rows (default 100)." }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let Some(store) = ctx.cdr.as_ref() else {
            return Ok(json!({"count": 0, "rows": []}));
        };
        let mut filter = smiths_core::storage::CdrFilter::new();
        if let Some(v) = args.get("since_unix").and_then(Value::as_i64) {
            filter.since_unix = Some(v);
        }
        if let Some(v) = args.get("until_unix").and_then(Value::as_i64) {
            filter.until_unix = Some(v);
        }
        if let Some(v) = args.get("from_like").and_then(Value::as_str) {
            filter.from_like = Some(v.to_owned());
        }
        if let Some(v) = args.get("to_like").and_then(Value::as_str) {
            filter.to_like = Some(v.to_owned());
        }
        if let Some(v) = args.get("result").and_then(Value::as_str) {
            filter.result = Some(v.to_owned());
        }
        if let Some(v) = args.get("limit").and_then(Value::as_u64) {
            filter.limit = u32::try_from(v)
                .map_err(|_| ToolError::InvalidArguments("`limit` must fit in u32".into()))?;
        }
        let rows = store
            .list(&filter)
            .map_err(|e| ToolError::Internal(format!("cdr list: {e}")))?;
        Ok(json!({ "count": rows.len(), "rows": rows }))
    }
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

/// `send_dtmf(call_id, digits)` — emit an RFC 4733 telephone-event
/// stream over an active call's media leg (slice 2.4, P7).
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
        let call_id = args
            .get("call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
        let digits = args
            .get("digits")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("digits required".into()))?;
        if digits.is_empty() {
            return Err(ToolError::InvalidArguments(
                "digits must be non-empty".into(),
            ));
        }
        let duration_ms = args
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(160);
        let duration_ms = u32::try_from(duration_ms)
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
                let bytes = pkt.encode();
                ctx.media
                    .send_packet(endpoint, remote, &bytes)
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
        assert_eq!(reg.len(), 20);
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
            "list_cdr",
            "send_dtmf",
            "translate",
            "transcribe_call",
            "summarize_call",
            "search_calls_semantic",
            "put_script",
        ] {
            assert!(reg.get(name).is_some(), "missing tool: {name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_script_without_script_plugin_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = PutScriptTool
            .call(
                json!({"name": "route-rhai", "source": "fn describe_capabilities(){[]}", "engine": "rhai"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_script_rejects_unknown_engine() {
        let (ctx, _c) = ctx_with_state();
        let err = PutScriptTool
            .call(
                json!({"name": "x", "source": "y", "engine": "javascript"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_calls_semantic_without_vector_store_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = SearchCallsSemanticTool
            .call(json!({"query": "billing issue"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_calls_semantic_rejects_empty_query() {
        let (ctx, _c) = ctx_with_state();
        let err = SearchCallsSemanticTool
            .call(json!({"query": "   "}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn search_extracts_from_mock_shape() {
        let v = json!({"vectors": [[0.1, 0.2, 0.3]]});
        assert_eq!(extract_first_embedding(&v), Some(vec![0.1, 0.2, 0.3]));
    }

    #[test]
    fn search_extracts_from_openai_data_shape() {
        let v = json!({
            "data": [{"embedding": [1.0, 2.0, 3.0]}],
            "model": "text-embedding-3-small"
        });
        assert_eq!(extract_first_embedding(&v), Some(vec![1.0, 2.0, 3.0]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn translate_without_provider_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = TranslateTool
            .call(json!({"text": "hello", "to": "es"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn translate_rejects_missing_target() {
        let (ctx, _c) = ctx_with_state();
        let err = TranslateTool
            .call(json!({"text": "hello"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn translate_extracts_from_flat_content() {
        let v = json!({"content": "hola"});
        assert_eq!(extract_chat_content(&v).as_deref(), Some("hola"));
    }

    #[test]
    fn translate_extracts_from_nested_message() {
        // Ollama-style response.
        let v = json!({"message": {"role": "assistant", "content": "hola"}});
        assert_eq!(extract_chat_content(&v).as_deref(), Some("hola"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_call_requires_audio_until_recording_store_lands() {
        let (ctx, _c) = ctx_with_state();
        // call_id alone = NotFound with guidance, not a panic.
        let err = TranscribeCallTool
            .call(json!({"call_id": "abc"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_call_without_asr_provider_is_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = TranscribeCallTool
            .call(json!({"call_id": "abc", "audio_base64": ""}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summarize_call_requires_audio() {
        let (ctx, _c) = ctx_with_state();
        let err = SummarizeCallTool
            .call(json!({"call_id": "abc"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summarize_call_rejects_missing_call_id() {
        let (ctx, _c) = ctx_with_state();
        let err = SummarizeCallTool
            .call(json!({"audio_base64": "AAA"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn translate_extracts_from_choices() {
        // OpenAI-compatible response.
        let v = json!({
            "choices": [
                {"message": {"role": "assistant", "content": "hola"}}
            ]
        });
        assert_eq!(extract_chat_content(&v).as_deref(), Some("hola"));
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
