//! Composite AI tools routed through the shared
//! [`smiths_core::AiDispatcher`] on [`ToolContext::dispatcher`]: the
//! caller names a capability, not a plugin, and the dispatcher picks
//! the best healthy provider with fail-over.

use std::time::Instant;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use smiths_core::DispatchError;

use super::media::{decode_pcm16, inject_pcm16_into_call};
use super::{live_media_leg, require_str};
use crate::tool::{Tool, ToolContext, ToolError};

/// Map a dispatcher error onto the tool error vocabulary.
fn dispatch_err(e: DispatchError) -> ToolError {
    match e {
        DispatchError::NoProvider(cap) => ToolError::NotFound(format!(
            "no `{cap}` provider is loaded — install the matching ai.* plugin"
        )),
        e @ DispatchError::AllFailed { .. } => ToolError::Internal(e.to_string()),
    }
}

/// Observe one pipeline's end-to-end wall clock. Falls through to a
/// fresh `Metrics::noop` when the context doesn't carry a handle;
/// the observation is still recorded, just onto a registry nobody is
/// reading.
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

/// Resolve the audio payload for the pipeline tools. Inline
/// `audio_base64` wins; otherwise the recording store is consulted;
/// otherwise a `NotFound` with a targeted error surfaces so the
/// caller sees *exactly* which config knob is missing.
fn resolve_audio_base64(
    call_id: &str,
    args: &Value,
    ctx: &ToolContext,
) -> Result<String, ToolError> {
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

/// Transcribe `audio` through the `ai.asr` lane and return the text.
async fn transcribe_via_dispatcher(
    ctx: &ToolContext,
    audio: &str,
    args: &Value,
) -> Result<String, ToolError> {
    let params = json!({
        "audio_base64": audio,
        "sample_rate":  args.get("sample_rate"),
        "language":     args.get("language"),
        "controls":     args.get("controls"),
    });
    let v = ctx
        .dispatcher
        .invoke("ai.asr", "transcribe", params)
        .await
        .map_err(dispatch_err)?;
    Ok(v.get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned())
}

/// One `ai.llm.chat` turn: `system` prompt + `user` message.
async fn chat_via_dispatcher(
    ctx: &ToolContext,
    system: &str,
    user: &str,
) -> Result<Value, ToolError> {
    ctx.dispatcher
        .invoke(
            "ai.llm.chat",
            "chat",
            json!({
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user",   "content": user},
                ]
            }),
        )
        .await
        .map_err(dispatch_err)
}

/// Pull the assistant's textual reply out of whatever shape the
/// `ai.llm.chat` plugin returned. Accepts the direct `{content}`,
/// the nested `{message: {content}}`, and the OpenAI-compatible
/// `{choices: [{message: {content}}]}` forms.
fn extract_chat_content(v: &Value) -> Option<String> {
    v.get("content")
        .or_else(|| v.get("message").and_then(|m| m.get("content")))
        .or_else(|| {
            v.get("choices")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
        })
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// `translate` — render `text` into language `to` by routing through
/// the dispatcher at capability `ai.llm.chat`.
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
        let text = require_str(&args, "text")?;
        let to = require_str(&args, "to")?;
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
        let v = chat_via_dispatcher(
            ctx,
            "You are a professional translator. Preserve formatting \
             and proper nouns. Reply with the translated text only.",
            &prompt,
        )
        .await?;
        let translated = extract_chat_content(&v).unwrap_or_else(|| v.to_string());
        Ok(json!({
            "text":       text,
            "to":         to,
            "from":       from,
            "translated": translated,
            "raw":        v,
        }))
    }
}

/// `transcribe_call(call_id)` — transcribe a call's audio via the
/// dispatcher's `ai.asr` lane. Audio comes from the recording store
/// or an inline `audio_base64`.
pub struct TranscribeCallTool;

#[async_trait]
impl Tool for TranscribeCallTool {
    fn name(&self) -> &'static str {
        "transcribe_call"
    }

    fn description(&self) -> &'static str {
        "Transcribe a call's audio via the best-fit `ai.asr` provider. \
         Audio is read from the recording store, or pass `audio_base64` \
         inline; routes through the AI dispatcher."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id":      { "type": "string", "description": "SIP Call-ID." },
                "audio_base64": { "type": "string",
                                  "description": "PCM16 LE audio bytes, base64-encoded. Overrides the recording store." },
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
        let result = async {
            let call_id = require_str(&args, "call_id")?;
            let audio = resolve_audio_base64(call_id, &args, ctx)?;
            let params = json!({
                "audio_base64": audio,
                "sample_rate":  args.get("sample_rate"),
                "language":     args.get("language"),
                "controls":     args.get("controls"),
            });
            let raw = ctx
                .dispatcher
                .invoke("ai.asr", "transcribe", params)
                .await
                .map_err(dispatch_err)?;
            let text = raw
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Ok(json!({ "call_id": call_id, "transcript": text, "raw": raw }))
        }
        .await;
        observe_pipeline(ctx, "transcribe_call", started.elapsed());
        result
    }
}

/// `summarize_call(call_id)` — ASR over `ai.asr`, then a summary
/// over `ai.llm.chat`. Returns both the transcript (so the agent
/// doesn't re-transcribe) and the summary text.
pub struct SummarizeCallTool;

#[async_trait]
impl Tool for SummarizeCallTool {
    fn name(&self) -> &'static str {
        "summarize_call"
    }

    fn description(&self) -> &'static str {
        "Transcribe a call via `ai.asr`, then summarize the transcript \
         via `ai.llm.chat`. Returns `{transcript, summary}`. Audio is \
         read from the recording store, or pass `audio_base64` inline."
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
        let result = summarize_call(&args, ctx).await;
        observe_pipeline(ctx, "summarize_call", started.elapsed());
        result
    }
}

async fn summarize_call(args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
    let call_id = require_str(args, "call_id")?;
    let audio = resolve_audio_base64(call_id, args, ctx)?;
    let max_sentences = args
        .get("max_sentences")
        .and_then(Value::as_u64)
        .unwrap_or(3);
    let transcript = transcribe_via_dispatcher(ctx, &audio, args).await?;
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
    let v = chat_via_dispatcher(ctx, &system, &transcript).await?;
    let summary = extract_chat_content(&v).unwrap_or_else(|| v.to_string());
    Ok(json!({
        "call_id":    call_id,
        "transcript": transcript,
        "summary":    summary,
    }))
}

// ---------------------------------------------------------------------------
// Voice style presets: "speak → restyle → speak".
// ---------------------------------------------------------------------------

/// A named voice-transformation preset. Bundles a **text-rewrite
/// style** (the LLM system prompt) with a **TTS voice**, so a single
/// preset changes both *what* is said and *how* it sounds.
struct StylePreset {
    name: &'static str,
    description: &'static str,
    /// LLM system prompt that rewrites the transcript into this style.
    system_prompt: &'static str,
    /// TTS voice id passed to the `ai.tts` provider.
    voice: &'static str,
}

/// Built-in presets. Voices map to the in-tree `ai-tts-mock` catalog
/// (`irina` / `dmitri` / `alice`); real TTS plugins expose their own.
const STYLE_PRESETS: &[StylePreset] = &[
    StylePreset {
        name: "formal",
        description: "Polished, professional wording in a neutral narrator voice.",
        system_prompt: "Rewrite the user's message in polished, formal, professional English. \
                        Preserve the meaning and any names or numbers. Reply with only the \
                        rewritten text — no preamble, no quotes.",
        voice: "irina",
    },
    StylePreset {
        name: "casual",
        description: "Relaxed, friendly wording.",
        system_prompt: "Rewrite the user's message in a relaxed, friendly, conversational tone. \
                        Keep it natural and short. Reply with only the rewritten text.",
        voice: "alice",
    },
    StylePreset {
        name: "pirate",
        description: "Swashbuckling pirate speak in a gruff voice.",
        system_prompt: "Rewrite the user's message as a swashbuckling pirate would say it \
                        (arr, matey, ahoy). Keep the underlying meaning. Reply with only the \
                        rewritten text.",
        voice: "dmitri",
    },
    StylePreset {
        name: "concise",
        description: "Trimmed to the essentials.",
        system_prompt: "Rewrite the user's message as concisely as possible without losing \
                        meaning or key facts. Reply with only the rewritten text.",
        voice: "irina",
    },
    StylePreset {
        name: "polite",
        description: "Extra-courteous phrasing.",
        system_prompt: "Rewrite the user's message to be warm, courteous and polite, while \
                        keeping its meaning. Reply with only the rewritten text.",
        voice: "alice",
    },
];

fn lookup_preset(name: &str) -> Option<&'static StylePreset> {
    STYLE_PRESETS
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
}

fn preset_names() -> String {
    STYLE_PRESETS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `list_style_presets` — enumerate the built-in voice presets so an
/// agent (or UI) can show the available styles.
pub struct ListStylePresetsTool;

#[async_trait]
impl Tool for ListStylePresetsTool {
    fn name(&self) -> &'static str {
        "list_style_presets"
    }

    fn description(&self) -> &'static str {
        "List the built-in voice style presets for `restyle_call`. Each \
         bundles a text-rewrite style with a TTS voice."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn call(&self, _args: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        let presets: Vec<Value> = STYLE_PRESETS
            .iter()
            .map(|p| json!({ "name": p.name, "description": p.description, "voice": p.voice }))
            .collect();
        Ok(json!({ "presets": presets }))
    }
}

/// `restyle_call` — the "speak → restyle → speak" pipeline. Transcribes
/// an utterance (`ai.asr`), rewrites it into a preset style
/// (`ai.llm.chat`), synthesizes it in the preset's voice (`ai.tts`),
/// and — when a live `call_id` is given — streams it back into the
/// call. Without `call_id` (or with `speak: false`) it returns the
/// styled audio instead, so the same tool drives "type → styled
/// speech" too.
pub struct RestyleCallTool;

#[async_trait]
impl Tool for RestyleCallTool {
    fn name(&self) -> &'static str {
        "restyle_call"
    }

    fn description(&self) -> &'static str {
        "Restyle an utterance into a preset voice (text + voice) via \
         ASR → LLM-rewrite → TTS. Provide `audio_base64` (an utterance) \
         or `text`. With a live `call_id` the styled speech is injected \
         into the call; otherwise the styled audio is returned. See \
         `list_style_presets`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "preset":       { "type": "string", "description": "Style preset name (see list_style_presets)." },
                "call_id":      { "type": "string", "description": "Live call to inject the styled speech into." },
                "audio_base64": { "type": "string", "description": "PCM16 LE utterance to restyle (base64)." },
                "text":         { "type": "string", "description": "Text to restyle directly, skipping ASR." },
                "sample_rate":  { "type": "integer", "description": "Sample rate of audio_base64 (default 8000)." },
                "language":     { "type": "string",  "description": "ASR language hint (BCP-47 or `auto`)." },
                "speak":        { "type": "boolean", "description": "Inject into the call. Default: true when call_id is set." }
            },
            "required": ["preset"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let started = Instant::now();
        let result = restyle_call(&args, ctx).await;
        observe_pipeline(ctx, "restyle_call", started.elapsed());
        result
    }
}

async fn restyle_call(args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
    let preset_name = require_str(args, "preset")?;
    let preset = lookup_preset(preset_name).ok_or_else(|| {
        ToolError::InvalidArguments(format!(
            "unknown preset `{preset_name}` — try: {}",
            preset_names()
        ))
    })?;

    // 1. Transcript: explicit `text` wins; otherwise transcribe audio.
    let transcript = if let Some(text) = args.get("text").and_then(Value::as_str) {
        text.to_owned()
    } else {
        let call_id = args.get("call_id").and_then(Value::as_str).unwrap_or("");
        let audio = resolve_audio_base64(call_id, args, ctx)?;
        transcribe_via_dispatcher(ctx, &audio, args).await?
    };
    if transcript.trim().is_empty() {
        return Err(ToolError::Internal(
            "nothing to restyle: empty transcript".into(),
        ));
    }

    // 2. Rewrite into the preset's text style.
    let llm = chat_via_dispatcher(ctx, preset.system_prompt, &transcript).await?;
    let styled = extract_chat_content(&llm).unwrap_or_else(|| transcript.clone());

    // 3. Synthesize in the preset's voice.
    let synth = ctx
        .dispatcher
        .invoke(
            "ai.tts",
            "synthesize",
            json!({
                "text":  styled,
                "voice": preset.voice,
                "output": {"codec": "pcm_s16le", "sample_rate": 16000},
            }),
        )
        .await
        .map_err(dispatch_err)?;

    // 4. Inject into the live call, or return the styled audio.
    let call_id = args.get("call_id").and_then(Value::as_str);
    let do_speak = args
        .get("speak")
        .and_then(Value::as_bool)
        .unwrap_or(call_id.is_some());
    if !do_speak {
        return Ok(json!({
            "preset":      preset.name,
            "voice":       preset.voice,
            "transcript":  transcript,
            "styled_text": styled,
            "audio":       synth,
        }));
    }
    let call_id = call_id
        .ok_or_else(|| ToolError::InvalidArguments("speak=true requires a call_id".into()))?;
    let (_, endpoint, remote) = live_media_leg(ctx, call_id)?;
    let (samples, sample_rate) = decode_pcm16(&synth)?;
    let (frames, _) = inject_pcm16_into_call(ctx, endpoint, remote, &samples, sample_rate).await?;
    Ok(json!({
        "call_id":     call_id,
        "preset":      preset.name,
        "voice":       preset.voice,
        "transcript":  transcript,
        "styled_text": styled,
        "frames_sent": frames,
    }))
}

/// `search_calls_semantic(query, k)` — embed the natural-language
/// `query` via the `ai.embed` capability, then run a top-k search
/// against the wired `[storage.vector]` backend. Every hit carries
/// its `id`, `score`, and indexed `metadata`.
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
        let result = search_calls_semantic(&args, ctx).await;
        observe_pipeline(ctx, "search_calls_semantic", started.elapsed());
        result
    }
}

async fn search_calls_semantic(args: &Value, ctx: &ToolContext) -> Result<Value, ToolError> {
    let query = require_str(args, "query")?;
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
    let embed = ctx
        .dispatcher
        .invoke("ai.embed", "embed", json!({ "inputs": [query] }))
        .await
        .map_err(dispatch_err)?;
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
/// `ai.embed` plugin returned. Accepts `{vectors: [[...],...]}`
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
    // Embeddings are f32 by convention; narrowing from the JSON f64
    // is the expected precision loss.
    #[allow(clippy::cast_possible_truncation)]
    let narrowed: Option<Vec<f32>> = arr.iter().map(|x| x.as_f64().map(|f| f as f32)).collect();
    narrowed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::{FakeProvider, StaticRegistry, TestEngine};
    use smiths_core::EndpointId;
    use smiths_core::storage::VectorStore as _;
    use std::sync::Arc;

    fn pipeline_engine() -> (
        TestEngine,
        Arc<FakeProvider>,
        Arc<FakeProvider>,
        Arc<FakeProvider>,
    ) {
        let asr = FakeProvider::new(
            "asr",
            "ai.asr",
            &json!({}),
            json!({"text": "we need the invoice by friday"}),
        );
        let llm = FakeProvider::new(
            "llm",
            "ai.llm.chat",
            &json!({}),
            json!({"message": {"content": "Invoice due Friday."}}),
        );
        // 40 ms of 16 kHz PCM16 → 2 PCMU frames when injected.
        let audio =
            base64::engine::general_purpose::STANDARD.encode(vec![0u8; 16_000 * 2 * 40 / 1000]);
        let tts = FakeProvider::new(
            "tts",
            "ai.tts",
            &json!({}),
            json!({"codec": "pcm_s16le", "sample_rate": 16000, "audio_base64": audio}),
        );
        let engine = TestEngine::with_registry(Arc::new(StaticRegistry(vec![
            asr.clone(),
            llm.clone(),
            tts.clone(),
        ])));
        (engine, asr, llm, tts)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn translate_without_provider_is_not_found() {
        let engine = TestEngine::new();
        let err = TranslateTool
            .call(json!({"text": "hello", "to": "es"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        let err = TranslateTool
            .call(json!({"text": "hello"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn translate_routes_through_the_shared_dispatcher() {
        let (engine, _, llm, _) = pipeline_engine();
        let out = TranslateTool
            .call(
                json!({"text": "hello", "to": "es", "from": "en"}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["translated"], "Invoice due Friday.");
        assert_eq!(out["to"], "es");
        let (_, params) = llm.last_call().unwrap();
        let user = params["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("The source language is en."));
        assert!(user.contains("into es"));
    }

    #[test]
    fn extract_chat_content_accepts_every_known_shape() {
        assert_eq!(
            extract_chat_content(&json!({"content": "hola"})).as_deref(),
            Some("hola")
        );
        assert_eq!(
            extract_chat_content(&json!({"message": {"role": "assistant", "content": "hola"}}))
                .as_deref(),
            Some("hola")
        );
        assert_eq!(
            extract_chat_content(&json!({"choices": [{"message": {"content": "hola"}}]}))
                .as_deref(),
            Some("hola")
        );
        assert!(extract_chat_content(&json!({"other": 1})).is_none());
    }

    #[test]
    fn extract_first_embedding_accepts_mock_and_openai_shapes() {
        assert_eq!(
            extract_first_embedding(&json!({"vectors": [[0.1, 0.2, 0.3]]})),
            Some(vec![0.1, 0.2, 0.3])
        );
        assert_eq!(
            extract_first_embedding(&json!({"data": [{"embedding": [1.0, 2.0]}], "model": "m"})),
            Some(vec![1.0, 2.0])
        );
        assert!(extract_first_embedding(&json!({"vectors": [["x"]]})).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_call_needs_audio_or_a_recording_store() {
        let engine = TestEngine::new();
        let err = TranscribeCallTool
            .call(json!({"call_id": "abc"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        // Audio present but no ASR provider loaded.
        let err = TranscribeCallTool
            .call(json!({"call_id": "abc", "audio_base64": ""}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_and_summarize_call_round_trip() {
        let (engine, asr, llm, _) = pipeline_engine();
        let out = TranscribeCallTool
            .call(
                json!({"call_id": "c1", "audio_base64": "AAAA", "language": "en"}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["transcript"], "we need the invoice by friday");
        assert_eq!(asr.last_call().unwrap().1["language"], "en");

        let out = SummarizeCallTool
            .call(
                json!({"call_id": "c1", "audio_base64": "AAAA", "max_sentences": 1}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["transcript"], "we need the invoice by friday");
        assert_eq!(out["summary"], "Invoice due Friday.");
        let (_, params) = llm.last_call().unwrap();
        assert!(
            params["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("1 sentence")
        );
        assert_eq!(
            params["messages"][1]["content"],
            "we need the invoice by friday"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summarize_call_rejects_missing_call_id_and_requires_audio() {
        let engine = TestEngine::new();
        let err = SummarizeCallTool
            .call(json!({"audio_base64": "AAA"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        let err = SummarizeCallTool
            .call(json!({"call_id": "abc"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_style_presets_and_unknown_preset() {
        let engine = TestEngine::new();
        let out = ListStylePresetsTool
            .call(json!({}), &engine.ctx)
            .await
            .unwrap();
        let names: Vec<&str> = out["presets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"pirate"));
        let err = RestyleCallTool
            .call(json!({"preset": "shakespeare", "text": "hi"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(m) if m.contains("pirate")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restyle_call_returns_audio_or_injects_into_the_call() {
        let (engine, asr, llm, tts) = pipeline_engine();
        // Text path, no call: styled audio comes back.
        let out = RestyleCallTool
            .call(
                json!({"preset": "pirate", "text": "send the invoice"}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["styled_text"], "Invoice due Friday.");
        assert_eq!(out["voice"], "dmitri");
        assert!(out["audio"]["audio_base64"].is_string());
        assert!(asr.last_call().is_none(), "text input must skip ASR");
        assert_eq!(tts.last_call().unwrap().1["voice"], "dmitri");
        assert!(
            llm.last_call().unwrap().1["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("pirate")
        );

        // Audio path with a live call: ASR runs and RTP is injected.
        engine
            .dialog_created(
                "c1",
                Some((EndpointId(3), "127.0.0.1:4003".parse().unwrap())),
            )
            .await;
        let out = RestyleCallTool
            .call(
                json!({"preset": "formal", "call_id": "c1", "audio_base64": "AAAA"}),
                &engine.ctx,
            )
            .await
            .unwrap();
        assert_eq!(out["transcript"], "we need the invoice by friday");
        assert_eq!(out["frames_sent"], 2);
        assert_eq!(engine.fabric.sent().len(), 2);
        assert!(asr.last_call().is_some());

        // speak=true without a call is invalid arguments.
        let err = RestyleCallTool
            .call(
                json!({"preset": "formal", "text": "x", "speak": true}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_calls_semantic_without_vector_store_or_query_is_rejected() {
        let engine = TestEngine::new();
        let err = SearchCallsSemanticTool
            .call(json!({"query": "billing issue"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        let err = SearchCallsSemanticTool
            .call(json!({"query": "   "}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_calls_semantic_embeds_and_queries_the_store() {
        let emb = FakeProvider::new(
            "emb",
            "ai.embed",
            &json!({}),
            json!({"vectors": [[1.0, 0.0]]}),
        );
        let engine = TestEngine::with_registry(Arc::new(StaticRegistry(vec![emb.clone()])));
        let store = Arc::new(smiths_core::MemoryVectorStore::new());
        store
            .upsert(&smiths_core::storage::VectorRecord {
                id: "rec-1".into(),
                vector: vec![1.0, 0.0],
                metadata: json!({"call_id": "c1", "transcript": "billing"}),
            })
            .unwrap();
        store
            .upsert(&smiths_core::storage::VectorRecord {
                id: "rec-2".into(),
                vector: vec![0.0, 1.0],
                metadata: json!({"call_id": "c2", "transcript": "support"}),
            })
            .unwrap();
        let vector: Arc<dyn smiths_core::VectorStore> = store;
        let ctx = engine.ctx.clone().with_vector(vector);
        let out = SearchCallsSemanticTool
            .call(json!({"query": "billing issue", "k": 1}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["count"], 1);
        assert_eq!(out["hits"][0]["id"], "rec-1");
        assert_eq!(
            emb.last_call().unwrap().1["inputs"],
            json!(["billing issue"])
        );
    }
}
