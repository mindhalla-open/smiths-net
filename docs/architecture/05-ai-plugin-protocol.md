# AI plugin protocol

Design spec for the **`ai.*` capability layer** — how STT, TTS, LLM,
and embedding plugins plug into smiths-net's plugin ABI and surface
through MCP to the agent. This is the contract behind post-MVP phase
**P22** (`docs/plans/post-mvp.md`) and the guardrail defined in
`04-post-mvp-scope.md §17`.

**Status**: design, not implemented. When P22 lands, this doc is the
contract plugin authors and the engine runtime both respect.

## Why this exists

Every VoIP stack that tried pluggable STT/TTS before us ran into the
same wall: plugin interfaces hardcoded a fixed set of knobs (voice,
rate, volume) and either silently dropped anything extra or refused
to expose model-specific controls. FreeSWITCH's `mod_tts_commandline`,
`mod_flite`, `mod_unimrcp` — same story. The operator ends up
patching the plugin source for every new model.

Smiths-net's design reverses that: **plugins describe themselves, the
engine validates strictly, the agent sees exactly what each model
can do.** No hardcoded control list. No silent drops.

## Design principles

1. **Self-describing.** Every plugin returns a capability descriptor
   on load — the engine treats it as the only source of truth about
   what the model supports.
2. **Strict validation.** Unknown controls → hard error with
   diagnostics, never silent drop. The agent **always** gets a clean
   answer about what went wrong.
3. **Agent-visible capabilities.** The agent queries the engine over
   MCP and gets the full matrix of providers + controls — no
   out-of-band docs, no guessing.
4. **Open control schema.** Controls are JSON Schema per provider;
   we don't try to unify them into a lowest-common-denominator set.
   Portable agents use the intersection; power-users use provider-
   specific controls.
5. **Fail loud.** Errors carry structured diagnostics (field,
   reason, supported alternatives).
6. **Engine owns media I/O.** Plugins get bytes and give bytes; they
   never open sockets. RTP pacing, jitter, codec conversion — all
   engine-side.

## Capability namespace

| Capability            | Purpose                                             |
|-----------------------|-----------------------------------------------------|
| `ai.tts`              | Text → speech, delivered into a call leg            |
| `ai.asr`              | Speech → text (one-shot: transcribe a buffer)       |
| `ai.asr.stream`       | Speech → text (streaming: live partial transcripts) |
| `ai.llm.completion`   | Prompt → text (plain completion)                    |
| `ai.llm.chat`         | Chat messages → assistant message (tool use, json)  |
| `ai.embed`            | Text → vector                                        |
| `ai.nlu.intent`       | Text → structured intent (future)                   |

A plugin declares its capabilities in `plugin.toml`:

```toml
name     = "ai-tts-piper"
type     = "sidecar"
entry    = "./bin/tts-piper"
provides = ["ai.tts"]
abi      = "1.0"
```

Multiple capabilities in one plugin are allowed (e.g. a unified
`ai-openai` plugin providing `["ai.tts", "ai.asr", "ai.llm.chat",
"ai.embed"]`).

## Lifecycle

```
  load plugin.toml  →  spawn (WASM / sidecar)
      │
      ▼
  describe_capabilities()  →  []CapabilityDescriptor
      │
      ▼
  engine validates + registers  →  exposed via MCP
      │
      ▼
  invocations                 call_tool(...) → plugin → response
      │                       streaming: push notifications
      ▼
  on_shutdown()  →  drain + close
```

`describe_capabilities()` is the only mandatory call at boot. The
returned descriptors are cached for the life of the plugin instance
and re-published whenever the plugin hot-reloads. The engine refuses
to register a plugin whose descriptor is malformed or whose declared
controls use invalid JSON Schema.

## Capability descriptors

A capability descriptor is the **wire document** exposed through MCP
and used by the engine for validation.

### Common fields (all capabilities)

```json
{
  "capability":   "ai.tts",
  "plugin":       "ai-tts-piper",
  "model_id":     "piper-ru-irina-medium",
  "abi":          "1.0",
  "description":  "Fast neural TTS via ONNX.",
  "latency_ms":   { "p50": 120, "p95": 300 },
  "concurrency":  { "max_in_flight": 4 }
}
```

- `capability` — one of the reserved names above.
- `plugin` / `model_id` — identify the provider + the underlying model.
- `abi` — descriptor schema version. This doc = `"1.0"`.
- `latency_ms` — advisory; used by the router for priority decisions.
- `concurrency.max_in_flight` — how many parallel invocations the
  engine is allowed to run against this plugin before queueing.

### `ai.tts`

```json
{
  "capability": "ai.tts",
  "plugin":     "ai-tts-piper",
  "model_id":   "piper-ru-irina-medium",
  "abi":        "1.0",
  "voices": [
    {
      "id":     "irina",
      "lang":   "ru",
      "gender": "female",
      "description": "neutral narrator",
      "supported_sample_rates": [22050, 16000]
    }
  ],
  "default_voice": "irina",
  "output_formats": [
    { "codec": "pcm_s16le", "sample_rates": [8000, 16000, 22050] },
    { "codec": "pcmu",      "sample_rates": [8000] }
  ],
  "controls": {
    "rate":    { "type": "number",  "minimum": 0.5, "maximum": 2.0, "default": 1.0, "unit": "ratio" },
    "pitch":   { "type": "number",  "minimum": -12, "maximum": 12,  "unit": "semitones" },
    "volume":  { "type": "number",  "minimum": 0.0, "maximum": 1.0, "default": 1.0 }
  },
  "ssml":      { "supported": false, "dialects": [] },
  "streaming": { "supported": true, "first_chunk_ms": 150, "chunk_ms": 200 }
}
```

- `voices` — every supported voice as a concrete record, not a loose
  enum, because each voice carries its own language / gender / sample
  rates.
- `controls` — JSON Schema **per model**. The engine won't accept
  a key that isn't here. Provider-specific keys are welcome
  (`stability`, `speaker_boost`, `seed`, ...); they just show up in
  this map.
- `ssml.supported = true` unlocks an additional `ssml_input` control
  on the `speak` call; otherwise passing SSML is an error.
- `streaming.supported = true` means the plugin can emit audio chunks
  while synthesising; the engine paces them out to RTP as they arrive.

### `ai.asr` / `ai.asr.stream`

```json
{
  "capability": "ai.asr.stream",
  "plugin":     "ai-asr-whisper",
  "model_id":   "whisper.cpp-large-v3",
  "abi":        "1.0",
  "languages":  ["auto", "ru", "en", "es", "..."],
  "features":   ["timestamps", "word_level_timestamps", "vad", "diarization"],
  "input_formats": [
    { "codec": "pcm_s16le", "sample_rates": [16000, 8000] }
  ],
  "streaming": {
    "supported":          true,
    "partial_interval_ms": 500,
    "max_audio_secs":     300
  },
  "controls": {
    "language":    { "type": "string",  "enum_from": "languages", "default": "auto" },
    "temperature": { "type": "number",  "minimum": 0.0, "maximum": 1.0, "default": 0.0 },
    "beam_size":   { "type": "integer", "minimum": 1,   "maximum": 10,  "default": 5 },
    "diarize":     { "type": "boolean", "default": false,
                     "requires": ["features.diarization"] }
  }
}
```

- `features` is an **advertised feature set**; controls that depend on
  a feature carry `"requires": [...]` so validation stays honest.
- `enum_from` lets a control's valid set be derived from a sibling
  field (here, `languages`) instead of being duplicated.
- Two separate capabilities (`ai.asr` and `ai.asr.stream`) because
  semantics differ: non-streaming returns final text once; streaming
  emits partials and a final. A plugin can provide both.

### `ai.llm.chat`

```json
{
  "capability":     "ai.llm.chat",
  "plugin":         "ai-llm-ollama",
  "model_id":       "llama3.1-8b-instruct",
  "abi":            "1.0",
  "context_window": 8192,
  "max_output":     4096,
  "features":       ["tools", "json_mode", "system_prompt"],
  "roles":          ["system", "user", "assistant", "tool"],
  "streaming":      { "supported": true, "token_interval_ms": 30 },
  "controls": {
    "temperature": { "type": "number",  "minimum": 0.0, "maximum": 2.0, "default": 0.7 },
    "top_p":       { "type": "number",  "minimum": 0.0, "maximum": 1.0, "default": 0.9 },
    "max_tokens":  { "type": "integer", "minimum": 1,   "maximum": 4096, "default": 1024 },
    "stop":        { "type": "array",   "items": { "type": "string" } },
    "json_mode":   { "type": "boolean", "requires": ["features.json_mode"] }
  }
}
```

### `ai.embed`

```json
{
  "capability":    "ai.embed",
  "plugin":        "ai-embed-bge",
  "model_id":      "bge-m3",
  "abi":           "1.0",
  "dimension":     1024,
  "max_input_tokens": 8192,
  "distance":      ["cosine", "l2", "dot"],
  "controls":      {}
}
```

## MCP tool surface

All tools below live in the engine's MCP tool registry. They share
the unified `Tool` trait (`crates/smiths-mcp/src/tool.rs`) and so
benefit from the same validation + audit + rate-limit story as
`list_calls`/`health`.

### Discovery

| Tool                     | Purpose                                               |
|--------------------------|-------------------------------------------------------|
| `list_ai_providers`      | Every registered `ai.*` plugin with abbreviated descriptor |
| `describe_provider`      | Full descriptor for one plugin (`plugin` arg)         |
| `list_voice_providers`   | Filtered `list_ai_providers` for `ai.tts` only        |
| `list_asr_providers`     | Same for `ai.asr` / `ai.asr.stream`                   |
| `list_llm_providers`     | Same for `ai.llm.*`                                   |
| `list_embed_providers`   | Same for `ai.embed`                                   |

The agent calls one of these once (usually at session start) and
caches. Descriptors don't change mid-session unless a plugin
reloads, in which case the engine pushes
`notifications/capabilities/changed`.

### TTS

```jsonc
// tools/call  speak
{
  "call_id":   "cid-abc",               // required — which leg to speak on
  "text":      "Алло, Алиса слушает вас",
  "provider":  "ai-tts-piper",          // optional; capability-routed if absent
  "voice":     "irina",                 // optional; uses default_voice if absent
  "controls":  { "rate": 1.1, "pitch": -2 },
  "ssml":      false,                    // default false; true requires capability
  "wait":      false                     // default false → returns speak_handle
}
```

Response (with `wait: false`):

```json
{ "speak_handle": "spk-7421", "estimated_duration_ms": 1840 }
```

Events (MCP notifications, agent subscribes):

- `notifications/tts/started`  `{ speak_handle, call_id }`
- `notifications/tts/progress` `{ speak_handle, played_ms, total_ms }`  (optional, not every plugin)
- `notifications/tts/finished` `{ speak_handle, call_id, result: "ok" | "cancelled" | "error", ... }`

```jsonc
// tools/call  stop_speaking
{ "speak_handle": "spk-7421" }         // or { "call_id": "cid-abc" } to cancel all
```

### ASR (streaming)

```jsonc
// tools/call  start_transcription
{
  "call_id":   "cid-abc",
  "provider":  "ai-asr-whisper",
  "leg":       "remote",                // "remote" | "local"; default "remote"
  "language":  "ru",
  "controls":  { "beam_size": 5 }
}
```

Response:

```json
{ "stream_handle": "asr-1002" }
```

Events:

- `notifications/asr/partial` `{ stream_handle, call_id, text, is_final: false, confidence, words: [...] }`
- `notifications/asr/final`   `{ stream_handle, call_id, text, is_final: true,  confidence, words: [...], duration_ms }`

`stop_transcription(stream_handle)` cancels; the plugin is told, the
final pending chunk may still arrive as `final` before the stream ends.

### ASR (one-shot, over a buffer)

```jsonc
// tools/call  transcribe
{
  "provider":   "ai-asr-whisper",
  "audio_ref":  "call:cid-abc:last:30s",   // or "blob:<sha256>" pre-uploaded
  "language":   "ru"
}
```

`audio_ref` is a URI the engine knows how to resolve: either a trailing
N-seconds slice of a call's audio buffer, or a blob previously uploaded
via `upload_audio`. Keeps the tool interface agent-centric — agents
don't handle audio bytes.

### LLM

```jsonc
// tools/call  llm_chat
{
  "provider": "ai-llm-ollama",
  "messages": [
    { "role": "system",    "content": "You are a polite Russian phone operator." },
    { "role": "user",      "content": "Здравствуйте, это Алиса?" }
  ],
  "controls": { "temperature": 0.3, "max_tokens": 256 },
  "stream":   false                  // default false → one-shot
}
```

Response (non-streaming):

```json
{ "message": { "role": "assistant", "content": "Да, Алло. Слушаю вас." },
  "usage":   { "input_tokens": 58, "output_tokens": 12 } }
```

Streaming variant: `stream: true` → returns `{ "stream_handle": "llm-44" }`
and delivers `notifications/llm/delta` + `notifications/llm/final`.

### Embeddings

```jsonc
// tools/call  embed
{ "provider": "ai-embed-bge", "inputs": ["query 1", "query 2"] }
// → { "embeddings": [[...1024 floats...], [...]], "usage": { "tokens": 14 } }
```

### Per-call defaults

```jsonc
// tools/call  set_call_ai_defaults
{
  "call_id": "cid-abc",
  "tts":  { "provider": "ai-tts-piper", "voice": "irina", "controls": { "rate": 1.1 } },
  "asr":  { "provider": "ai-asr-whisper", "language": "ru" },
  "llm":  { "provider": "ai-llm-ollama", "controls": { "temperature": 0.2 } }
}
```

Stored on the dialog record (serializable, HA-safe). Subsequent
`speak(call_id, text)` with no `provider`/`voice`/`controls`
inherits. Per-call overrides always win over session defaults.

## Validation

Every invocation is checked against the plugin's descriptor *before*
reaching the plugin. The engine's job is to keep garbage away from
the model; the plugin's job is to run the model cleanly.

Validation steps:

1. **Provider resolution.**
   - If `provider` is set → look up by `plugin` name. Missing → error.
   - Else → capability-route (see next section).
2. **Capability match.** The invocation targets a capability
   (`ai.tts`, etc.) — plugin must declare it.
3. **Enum fields** (`voice`, `language`, output codec) — must be in
   the advertised list.
4. **Controls** — each key checked against the schema; type,
   min/max, `enum`, `requires` all enforced.
5. **Feature gating.** Controls with `requires: ["features.x"]` fail
   if the provider doesn't advertise feature `x`.
6. **Encoding / sample-rate compatibility** for TTS/ASR against the
   call leg's codec.

On failure:

```json
{
  "jsonrpc": "2.0",
  "id": 42,
  "error": {
    "code":    -32002,
    "message": "invalid argument",
    "data": {
      "capability": "ai.tts",
      "plugin":     "ai-tts-piper",
      "field":      "controls.stability",
      "reason":     "not supported by provider",
      "supported":  ["rate", "pitch", "volume"]
    }
  }
}
```

The structured `data.supported` field lets the agent **self-correct**
without asking a human.

## Routing & fallback

Two modes:

### Explicit provider

Agent sets `provider: "ai-tts-piper"`. Engine uses exactly that
plugin. On failure (plugin down, model error): surface the error to
the agent, **do not fall back** silently. The agent decides whether
to retry on another provider.

### Capability-routed

Agent omits `provider`. Engine picks from its ordered preference
list:

```toml
# smiths-net.toml
[ai.tts]
priority = ["ai-tts-piper", "ai-tts-openai"]
[ai.asr]
priority = ["ai-asr-whisper", "ai-asr-deepgram"]
[ai.llm.chat]
priority = ["ai-llm-ollama", "ai-llm-anthropic", "ai-llm-openai"]
```

Selection: first provider that is (a) healthy (recent success rate
above threshold), (b) not over `concurrency.max_in_flight`, (c)
accepts the requested controls + voice.

On mid-call failure (timeout / error): engine retries next in the
list once, then surfaces. Metrics:

- `smiths_ai_failovers_total{capability, from, to}`
- `smiths_ai_invocations_total{capability, plugin, outcome}`
- `smiths_ai_latency_seconds{capability, plugin}` (histogram)

## Streaming semantics

### TTS (engine → call leg)

```
agent  ──speak(text)──►  engine  ──synthesize_start──►  plugin
                                        ...audio chunks...
                                 ◄──send_audio_chunk──
                                        ...until done...
                                 ◄────synthesize_done──
engine  ──(paced RTP packets into call leg)──►  remote UA
agent  ◄──notifications/tts/finished──  engine
```

Plugins that can't stream return the full buffer at the end; the
engine still paces RTP at the call's clock rate so the remote UA's
playback behaves normally.

### ASR (call leg → agent)

```
engine  ──(leg audio bytes)──►  plugin
                                 ...partial transcripts...
                          ◄──emit_partial──
agent  ◄──notifications/asr/partial──  engine
                                 ...at VAD end or timeout...
                          ◄──emit_final──
agent  ◄──notifications/asr/final──  engine
```

The plugin decides when to emit — usually on VAD silence or when the
rolling buffer is full. The engine just routes.

## Concrete model mappings

How today's real-world models map into the above descriptors. These
are intended implementations, not speculation.

### TTS

| Model             | `plugin`         | Voices           | Notable controls                                  | SSML | Streaming |
|-------------------|------------------|------------------|---------------------------------------------------|------|-----------|
| Piper (ONNX)      | `ai-tts-piper`   | per-voice file   | `rate`, `pitch`, `volume`                         | no   | yes       |
| XTTS v2           | `ai-tts-xtts`    | speaker-wav ref  | `language`, `speaker_wav`, `temperature`          | no   | yes       |
| ElevenLabs        | `ai-tts-eleven`  | cloud catalog    | `stability`, `similarity_boost`, `style`, `speaker_boost` | cloud dialect | yes |
| OpenAI TTS        | `ai-tts-openai`  | fixed enum       | `speed`                                           | no   | no (short chunks only) |
| Azure Neural TTS  | `ai-tts-azure`   | catalog          | `rate`, `pitch`, `style`, `styledegree`           | SSML v1 | yes    |
| Google Cloud TTS  | `ai-tts-gcp`     | catalog          | `speaking_rate`, `pitch`, `volume_gain_db`        | SSML v1 | yes    |
| Piper is open and local | — | — | — | — | — |

### ASR

| Model                 | `plugin`          | Features                                          |
|-----------------------|-------------------|---------------------------------------------------|
| Whisper.cpp           | `ai-asr-whisper`  | `timestamps`, `word_level_timestamps`, `vad`      |
| faster-whisper        | `ai-asr-fwhisper` | same; lower latency                               |
| Deepgram Nova         | `ai-asr-deepgram` | streaming-first; `diarize`, `punctuate`, `smart_format` |
| AssemblyAI            | `ai-asr-assembly` | `speaker_labels`, `content_safety`                |
| Google Cloud Speech   | `ai-asr-gcp`      | phrase hints                                      |

### LLM

| Model                    | `plugin`           | Notable controls                                  |
|--------------------------|--------------------|---------------------------------------------------|
| Ollama (any local)       | `ai-llm-ollama`    | `temperature`, `top_p`, `top_k`, `num_predict`    |
| llama.cpp server         | `ai-llm-llamacpp`  | `temperature`, `mirostat`, `repeat_penalty`       |
| OpenAI chat              | `ai-llm-openai`    | `temperature`, `top_p`, `max_tokens`, `stop`, `tools` |
| Anthropic                | `ai-llm-anthropic` | `temperature`, `max_tokens`, `stop_sequences`, `tools` |
| Gemini                   | `ai-llm-gemini`    | `temperature`, `top_p`, `top_k`, `max_output_tokens` |
| Groq                     | `ai-llm-groq`      | same surface as OpenAI                            |

### Embeddings

| Model            | `plugin`          | Dimension | Languages |
|------------------|-------------------|-----------|-----------|
| BGE-M3           | `ai-embed-bge`    | 1024      | multi     |
| GTE-large        | `ai-embed-gte`    | 1024      | en        |
| E5-large         | `ai-embed-e5`     | 1024      | multi     |
| OpenAI embed-3   | `ai-embed-openai` | 1536 / 3072 | multi   |
| Cohere embed     | `ai-embed-cohere` | 1024 / 4096 | multi   |

## Error model

JSON-RPC error codes used across AI tools:

| Code    | Meaning                                                        |
|---------|----------------------------------------------------------------|
| `-32001`| Provider not found (unknown plugin name)                       |
| `-32002`| Invalid argument (control / voice / language / schema violation)|
| `-32003`| Provider error (model timeout, upstream 5xx, OOM, ...)         |
| `-32004`| Provider unavailable (disabled, over quota, not healthy)       |
| `-32005`| Call not found / ended (for call-scoped tools)                  |
| `-32006`| Capability not installed (no plugin provides it)                |

Every error carries `data` with `capability`, `plugin` (if any),
`field` (where applicable), `reason`, and `supported` (for `-32002`).

## Security & sandboxing

- Plugins run as they already do — Tier A1 (WASM, sandboxed) or
  Tier B (sidecar subprocess). AI plugins are typically Tier B
  because they ship native model runtimes.
- Plugin manifest's `permissions` list controls which host APIs
  the plugin can call: at minimum `ai.recv_audio`, `ai.send_audio`,
  `ai.emit_partial` for speech plugins; `ai.respond` for LLM.
- Engine validates input **before** dispatch to plugin — a malicious
  or buggy agent cannot feed the plugin a value the descriptor didn't
  advertise.
- Rate-limit + audit live on the MCP tool boundary (already
  implemented), so per-agent quotas on AI calls are free.

## Versioning

- Capability name carries an implicit `v1` — breaking schema changes
  bump the name: `ai.tts.v2`.
- Plugins advertise `abi` in their descriptor (descriptor schema
  version, starts at `"1.0"`).
- Engine refuses to register a descriptor with an `abi` it doesn't
  understand, logs a clean diagnostic, keeps running.
- Within a major `abi`, new optional fields may appear — plugins
  must tolerate unknown fields.

## What this gives the agent

The agent's full AI integration boils down to **pure text-plus-JSON
operations over MCP**:

```python
# One-time discovery (cached per session):
voices = await mcp.call_tool("list_voice_providers")
llms   = await mcp.call_tool("list_llm_providers")

# On an incoming call:
await mcp.call_tool("set_call_ai_defaults", {
    "call_id": call_id,
    "tts":  {"provider": "ai-tts-piper", "voice": "irina", "controls": {"rate": 1.1}},
    "asr":  {"provider": "ai-asr-whisper", "language": "ru"},
    "llm":  {"provider": "ai-llm-ollama", "controls": {"temperature": 0.2}},
})
await mcp.call_tool("start_transcription", {"call_id": call_id})

# Loop over asr notifications:
async for notif in mcp.notifications():
    if notif["method"] == "notifications/asr/final":
        text = notif["params"]["text"]
        reply = await mcp.call_tool("llm_chat", {"messages": [...]})
        await mcp.call_tool("speak", {
            "call_id": call_id,
            "text":    reply["message"]["content"],
        })
```

No RTP, no μ-law, no WAV files, no subprocess management. That's the
target architecture post-P22.

## Out of scope for this doc

- Exact WASM guest ABI (bytes in the linear memory) — separate doc
  alongside P3 plugin work.
- Exact sidecar protobuf wire — part of `smiths-proto` schema
  freeze (P3/P4).
- Observability (metrics + tracing spans) for AI calls — covered by
  the generic tool audit layer; specifics in the metrics catalog
  once P22 lands.
- Cost tracking per provider — future tool `usage_report(range)`
  reading from `storage.cdr`.

## Open questions

1. **Partial-audio ref.** For `transcribe` over the "last N seconds
   of a call's audio", the engine must buffer PCM per call. How
   long? Probably a per-call circular buffer, configurable, default
   30 s. Memory budget: 30 s × 8 kHz × 2 B × N calls = ~480 KB/call
   which is fine for hundreds of concurrent calls.

2. **Barge-in** (TTS cut off by caller starting to speak). Need a
   combination of VAD on the incoming leg + auto-cancel of the
   active `speak_handle`. A config flag on `set_call_ai_defaults`
   opts in. Details to design when we start implementing — the
   protocol above accommodates it via `stop_speaking`.

3. **Provider cost awareness.** Should descriptors carry a `cost`
   field (USD per 1k chars / per minute)? Useful for routing. Can
   add as optional field in abi 1.x without breaking anything.

4. **Tool-use LLM calls across multiple providers.** Standardising
   "function calling" JSON shape across OpenAI / Anthropic / local
   is a mess. For now: pass `tools` through as opaque provider-
   specific JSON, mark as provider-specific control.

## References

- `02-plugin-system.md §Tier B` — sidecar plugin lifecycle + ABI.
- `04-post-mvp-scope.md §17` — AI providers guardrail.
- `docs/plans/post-mvp.md §P22` — phase plan and acceptance criteria.
- `03-mcp-and-ops.md` — tool / resource registry on the MCP side.
