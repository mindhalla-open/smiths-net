# Changelog

All notable changes to **smiths-net** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-04-18

### Added — bidirectional plugin RPC (streaming notifications)

- **`smiths-sidecar::PluginNotification`** — a plugin-to-engine
  JSON-RPC notification (no `id`). The sidecar's reader now fans
  each inbound notification frame out via a
  `tokio::sync::broadcast::Sender<PluginNotification>` on the
  `Sidecar`. `Sidecar::subscribe_notifications()` hands out fresh
  `Receiver`s so any consumer (control plane, MCP tool, recording
  hook) can observe the stream without interfering with the
  request/response correlator.
- **`smiths-core::Event::Plugin(PluginEvent)`** new event bus
  variant. `PluginEvent::Notification { plugin, method, params }`
  carries the frame across the engine seam so subsystems that don't
  link `smiths-sidecar` can still react to plugin-initiated events.
- **`smiths-plugin::loader` bridge** — every loaded plugin now gets
  a background task that republishes its notifications onto the
  `EventBus` as `Event::Plugin`. Handles
  `broadcast::RecvError::Lagged` with a warn and keeps draining;
  exits cleanly when the sidecar closes. `load_plugins` / `load_one`
  grew a `bus: Option<EventBus>` parameter.
- **`smiths-mcp` MCP notification forwarding** — the control-plane
  MCP session now turns `PluginEvent::Notification` into
  `notifications/plugin/{method}` JSON-RPC frames on the MCP wire
  (`{plugin, data}` params). Agents can subscribe to streaming
  plugin output (e.g. live ASR partials) without polling.
- **`ai-asr-mock` example** — grew a `stream: boolean` control.
  When `controls.stream = true`, the plugin emits two `emit_partial`
  JSON-RPC notifications with cumulative `{call_id, text,
  is_final}` fragments before returning the final transcript. An
  integration test (`smiths-plugin/tests/streaming.rs`) loads the
  mock, invokes it, and asserts the two partials flow through the
  engine's bus in order.

### Added — WASM host next layer (manifest tier + state + deadlines)

- **`smiths-wasm::WasmEngine`** gained:
  - `plugin_state(plugin)` — per-plugin persistent KV
    (`Arc<DashMap<Vec<u8>, Vec<u8>>>`) that survives the ephemeral
    `Store` we build per `run_entry` call.
  - Host fns `smiths::state_set(k_ptr, k_len, v_ptr, v_len) -> i32`
    and `smiths::state_get(k_ptr, k_len, out_ptr, out_cap) -> i32`
    — `state_get` returns the value's full length (so the guest can
    detect truncation) or `-1` on a miss.
  - `run_with_deadline(module, entry, fuel, plugin, Duration)` —
    arms a cancellable one-shot `DeadlineTimer` that calls
    `Engine::increment_epoch` on expiry. The store's epoch deadline
    is set to `1`, so the guest traps on the next instruction with
    `WasmError::Timeout`. Deadline-free `run_entry` still works.
  - `call_describe(module, plugin)` — invokes the guest's
    `describe() -> i64` export (high 32 = ptr, low 32 = len into the
    exported `memory`) and returns the byte range.
- **`smiths-plugin::WasmProvider`** — new provider backend that
  registers alongside sidecars. `load` compiles the `.wasm`, calls
  `describe()`, parses the result as `CapabilityDescriptor(s)`,
  sanity-checks against the manifest's `provides`, and clamps the
  descriptor's `plugin` field to the manifest name. `invoke` stays
  stubbed at this tier — the next host-surface slice
  (`send_sip` / `send_rtp` / permission checks) unblocks dispatch.
- **`AiRegistry` is now dual-backend** — stores sidecars and WASM
  providers in parallel `DashMap`s. `len`, `is_empty`,
  `capabilities`, `snapshot` (trait), and `shutdown_all` span both.
  The inherent sidecar-specific `get(name) -> Arc<PluginEntry>` is
  retained for streaming / reload consumers.
- **`smiths-plugin::loader`** recognises `type = "wasm"` manifests
  and dispatches to `WasmProvider::load`. Fails-partial with a
  descriptive error when no `WasmEngine` is supplied. CLI builds
  one engine at boot and threads it through; tests pass `None` when
  they don't exercise the WASM path.
- **`rust-logger` example** — now exports `describe() -> i64`
  returning the `(ptr << 32) | len` of a static JSON
  `CapabilityDescriptor` for `ai.log`, plus a `plugin.toml` so the
  engine registers it as a WASM plugin. New integration test
  (`smiths-plugin/tests/wasm_loader.rs`) stages an inline-WAT
  `describe`-only module end-to-end through `load_plugins` and
  asserts the capability is surfaced.

## [0.7.0] - 2026-04-19

### Added — UAC + outbound call control (`make_call` / `end_call`)

- **`smiths-sip::UacClient`** — engine-side User Agent Client. One-shot
  INVITE transaction with a configurable budget (default 30 s):
  parses the target URI, allocates a media endpoint via the shared
  `MediaFabric`, builds an SDP offer through the `SdpNegotiator`,
  subscribes for the response branch on a new `ResponseRouter`,
  sends the INVITE, skips 1xx, ACKs the 2xx end-to-end (fresh
  branch), stores the dialog, publishes
  `SipEvent::DialogCreated { call_id, media_endpoint, remote_rtp }`.
  `hangup(call_id)` sends BYE, waits for 200, publishes
  `SipEvent::DialogTerminated`, releases the media endpoint.
- **`smiths-sip::ResponseRouter`** — shared branch-keyed oneshot
  correlator. UAS forwards any response it sees; UAC subscribes
  before each outbound request. Drops stale branches with a debug
  log. Four unit tests cover deliver / cancel / replace / unknown.
- **`smiths-core::call::CallOriginator` trait** — the MCP control
  plane talks to this, not `smiths-sip` directly. `UacClient`
  implements it; `ToolContext.originator: Option<Arc<dyn …>>` gates
  the tools cleanly when no UAC is configured (UAS-only deployments).
- **`SdpNegotiator` grew two methods** — `build_offer(local_ip,
  local_rtp_port)` (UAC-side offer emission) and `parse_remote_rtp(
  answer_body)` (UAC parses peer's RTP endpoint out of a 200 OK).
  `smiths-sdp::Negotiator` implements both; UAC never touches the
  SDP parse tree.
- **`make_call(target)` + `end_call(call_id)` MCP tools** (2 new
  built-ins, registry now 13 tools). Input schema validates the SIP
  URI shape; output returns `{call_id, target}` / `{call_id,
  status}`. Errors map through `CallError` → `ToolError` so rate
  limit + audit + metrics paths work unchanged.
- **UAS** gained `with_response_router` builder — when set, responses
  arriving on the UAS socket are routed to the UAC by Via branch.
  Without the router the UAS keeps the old drop-responses behaviour.
- **CLI** stands up the UAC from the first configured UDP bind
  (shares the transport + router with the UAS), attaches
  `Arc<dyn CallOriginator>` to `ToolContext`. Other SIP binds stay
  UAS-only.
- Integration test `uac_places_call_and_hangs_up_against_fake_uas` —
  real UAC places a call against a `FakeUas` responder, asserts
  `DialogCreated` fires with correct `call_id` + `media_endpoint` +
  `remote_rtp`, then `hangup` fires `DialogTerminated`.

## [0.6.0] - 2026-04-19

### Added — Engine-side `speak` + MCP over HTTP + SSE

- **Engine-side `speak(call_id, plugin, text, voice?, controls?)` tool.**
  The agent no longer streams RTP itself. On invocation the engine
  looks up the call's media endpoint, calls the plugin's `synthesize`,
  decodes the returned PCM16, downsamples to 8 kHz, μ-law-encodes, and
  streams 20 ms RTP frames through `MediaFabric::send_packet` with a
  stable per-invocation SSRC. Returns
  `{call_id, plugin, frames_sent, duration_ms, ssrc}`.
- **`MediaFabric::send_packet(src, dest, bytes)` trait method** +
  `UdpMediaFabric` impl. The primitive `speak` builds on; non-bridging
  path for raw RTP emission.
- **`SipEvent::DialogCreated` carries media info.** Now
  `{ call_id, media_endpoint, remote_rtp }`. `ControlState`'s
  `CallSnapshot` stores both so the `speak` tool can resolve
  `call_id → (endpoint, remote)` without a new trait.
- **`smiths-core::{rtp, codec}` modules promoted from testkit.**
  Pure, dependency-free RTP packet builder/parser + G.711 μ-law
  conversion. `smiths-media` and `smiths-testkit` re-export for
  backward compat.
- **`ToolContext` carries `Arc<dyn MediaFabric>`** so audio-injecting
  tools have a first-class handle.
- **Reference plugin `plugins/examples/ai-embed-mock/`** completes the
  AI quartet. Deterministic SHA-256-seeded 128-dim vectors, optional
  L2 normalization.
- **`embed(plugin, inputs[], controls?)` MCP tool** dispatches to any
  `ai.embed` plugin; strict control validation shared with the other
  AI tools.
- **MCP over HTTP + SSE (`smiths-mcp::mcp_http`).** Two routes:
  `POST /mcp` (JSON-RPC, shares `dispatch` + audit + rate-limit +
  metrics with stdio; actor label `mcp-http`), and `GET /mcp/events`
  (text/event-stream forwarding bus-driven notifications with 15 s
  keep-alive). Configured via `[mcp] enabled_http / http_bind`.
- New integration tests: `speak_injects_rtp_into_live_call` (real
  engine + `ai-tts-mock` subprocess, verifies PCMU RTP with stable
  SSRC reaches UA), `post_tools_call_health_round_trip`,
  `post_initialize_advertises_resources_and_tools`,
  `sse_stream_receives_dialog_created_notification`.

### Added — Control-plane hardening (Resources, auth, rate limit, audit, reload)

- **`Resource` trait + `ResourceRegistry`.** Shipped impls:
  `health://status`, `sip://calls`, `config://current` (with secret
  redaction). Both MCP and A2A serve `resources/list` +
  `resources/read`.
- **Per-tool token-bucket rate limiter** (`smiths-mcp::RateLimiter`)
  configured by `[mcp] rate_limit { per_sec, burst }`. Shared across
  MCP stdio, MCP HTTP, and A2A via a single `invoke_audited` helper.
- **Structured audit log** — one `info!` per tool call at target
  `smiths_mcp::audit` with `actor`, `tool`, `args_hash` (SHA-256),
  `outcome`, `duration_ms`, `error`.
- **Bearer-token auth for A2A HTTP** — `[a2a] bearer_token` gates
  `/a2a`; `/health` and `/.well-known/agent.json` stay public.
- **`reload_plugin` tool + `AiRegistry::reload` trait method.** Drains
  the current sidecar and respawns from the captured plugin directory.
- **`ToolContext` gained `Arc<Config>`** so tools / resources read
  engine settings without reaching back into CLI wiring.

### Added — Sidecar hardening (restart policy)

- **`RestartPolicy` with exponential backoff** in `smiths-sidecar`. A
  supervisor task detects child exit via stdout EOF, respawns up to
  `max_retries` with configurable `initial_backoff` / `max_backoff` /
  `backoff_multiplier`. In-flight RPCs at crash time resolve to
  `Error::Closed`; `no_restart()` keeps the old suicide-on-crash
  behaviour. Five new tests including `sidecar_respawns_after_crash`
  and `concurrent_calls_all_complete` (32 parallel RPCs).

### Added — Phase 6 first slice (TLS + Prometheus)

- **`smiths-sip::TlsTransport`** — `rustls` + SNI, inbound-only.
  `[sip] tls_cert_path / tls_key_path` configures PEM cert + key on
  disk. Shared framing module with the TCP transport. Self-signed
  `rcgen`-based integration test (`tests/tls.rs`).
- **Prometheus exporter.** `smiths-core::metrics::Metrics` registers
  `sip_requests_total{method}`, `sip_responses_total{code}`,
  `sip_dialogs_active`, `tool_invocations_total{tool,outcome}`,
  `tool_duration_seconds{tool}` (histogram). UAS increments on every
  request / response; `invoke_audited` records tool latency. CLI
  exposes `/metrics` (OpenMetrics text) on the existing health HTTP
  server.

### Added — Phase 3 walking skeleton (WASM host)

- **`smiths-wasm::WasmEngine`.** Wasmtime-backed host with per-call
  fuel metering, one host function (`smiths::log`), inline-WAT tests
  exercising host calls, trap isolation, fuel exhaustion, missing
  exports, and OOB memory reads.
- **`smiths-plugin::Dispatcher` trait + `MemoryDispatcher`.** Priority
  ordering, per-event time budget skipping the slow tail, re-register
  semantics.
- **`plugins/examples/rust-logger/`** — minimal no_std cdylib targeting
  `wasm32-unknown-unknown`, compiles from source; walking-skeleton
  smoke test for the wasmtime host.

### Added — Phase 2 slice 1 (media trait seams + SSRC router)

- **`MediaEndpoint` trait + `EndpointKind`** (`Host` / `ServerReflexive`
  / `Relayed`) in `smiths-core::media`. `MediaFabric::allocate` now
  returns `Arc<dyn MediaEndpoint>`.
- **`MediaSession` trait** — forwarding-session lifecycle.
  `smiths-media::Bridge` implements it.
- **Even-RTP / odd-RTCP port allocator** (`smiths-media::port_allocator`).
- **SSRC-rewriting passthrough router** — `smiths-media::bridge` parses
  RTP headers, rewrites SSRC per leg with a stable per-direction
  engine SSRC, drops non-RTP packets. New `g711_bridge` test
  verifies payload preserved **and** egress SSRC != ingress SSRC.

### Added — Phase 1 completion (TCP + INVITE auth + testkit + fuzz + sipp)

- **TCP SIP transport** (`smiths-sip::TcpTransport`) — Content-Length
  + double-CRLF framing, per-peer mpsc writers, inbound accept loop
  + lazy outbound connect. Wired into the CLI alongside UDP.
- **INVITE digest auth** — `UasServer::invite_auth_ok` mirrors the
  REGISTER challenge path. Dedupe now skips ACK so ACKs for rejected
  INVITEs don't loop the transaction (RFC 3261 §17.1.1.3).
- **`invite_401_cancel` integration test** — INVITE → 401 +
  WWW-Authenticate → ACK → BYE returns 481 (proves no dialog leaked).
- **Testkit helpers promoted:** `FakeUac` (renamed from `TestUac`) +
  `FakeUas` + `CapturedRequest`. `FakeUac::invite_expect_rejection`
  drives auth-challenge tests.
- **SIP parser fuzz harness** — `fuzz/` crate via `cargo-fuzz` +
  `libfuzzer-sys`, target `sip_parser`, workspace-excluded.
- **sipp REGISTER load scenario** (`scenarios/sipp/register.xml`) with
  digest auth + run-command docs.
- **`#[instrument]` coverage audit** — spans added to UAS
  `handle_invite` / `handle_register` / `handle_bye`, both stream
  transports' `spawn_reader`, `UdpMediaFabric::{allocate, bridge}`,
  plugin `load_plugins` / `load_one`, `Sidecar::{spawn, call_with_timeout}`.

### Changed — Architecture refactor: `smiths-mcp` consumes core traits

- **`smiths-mcp` no longer depends on `smiths-plugin`.** The AI-plugin
  contract (`CapabilityDescriptor`, `validate_controls`, `AiProvider`,
  `AiRegistry` traits, `ProviderError`) moved into `smiths-core::ai`.
  `smiths-plugin` implements the traits; `smiths-mcp` consumes them
  through the core seam. Mirrors the `MediaFabric` / `SdpNegotiator`
  pattern. `docs/architecture/01-crate-layout.md` updated to match.
- **`smiths-plugin` owns the host tiers.** Re-exports
  `smiths-sidecar`, `smiths-wasm`, `smiths-script` as
  `plugin::{sidecar, wasm, script}` — the single documented
  cross-sibling exception.
- **`smiths-script` crate scaffolded.** Stub today; placeholder for
  the embedded DSL host (Rhai / Lua / Starlark).
- **`docs/` moved to a git submodule** at
  `git@github.com:friday-mindhalla/smiths-net-docs.git`. Parent repo
  pins a commit via `.gitmodules`.

### Added — `transcribe` + `llm_chat` MCP tools (P4 slice 3)

- **`transcribe`** MCP tool: accepts base64 PCM16 + language hint,
  dispatches to any plugin providing `ai.asr`, returns transcript
  with confidence + duration. Full `resolve_and_validate` helper
  factored out so `transcribe` / `llm_chat` / future `embed` share
  the same plugin-lookup + strict control-validation prologue.
- **`llm_chat`** MCP tool: `messages`-array in, `{message, usage,
  finish_reason}` out. Rejects empty `messages`. Dispatches to any
  plugin providing `ai.llm.chat`.
- **Reference plugin `plugins/examples/ai-asr-mock/`** — pure-stdlib
  Python sidecar advertising a realistic `ai.asr` descriptor
  (`languages: [auto, ru, en]`, `features`, `input_formats`,
  `controls: {language, beam_size}`). Transcription stubbed to a
  duration-derived placeholder; shape identical to what Whisper
  would return.
- **Reference plugin `plugins/examples/ai-llm-mock/`** — advertises
  `ai.llm.chat` with `context_window`, `features: ["system_prompt"]`,
  `controls: {temperature, max_tokens}`; canned-response rule table
  keyed on the last user turn.
- **`voice_agent.py` goes zero-AI-code**: dropped `stub_stt` /
  `stub_llm` entirely. New `mcp_transcribe()` + `mcp_llm_chat()`
  helpers invoke the engine's tools; every inference hop now flows
  through MCP → sidecar plugin. Three tools involved per call:
  `transcribe` → `llm_chat` → `synthesize`.
- Two new unit tests: `transcribe_without_plugin_is_not_found`,
  `llm_chat_rejects_empty_messages`. Builtins registry test updated
  (5 → 6 → 8 tools).
- Release binary: 3.4 MB → **3.5 MB** (two extra tools + refactored
  validation helper).

### Added — Digest auth + REGISTER (Phase 1 catch-up)

- **`smiths-sip::auth::digest`** module — RFC 2617 + RFC 8760
  primitives: `ha1` / `ha2` / `response_qop_auth` / `response_no_qop`,
  `Algorithm::{Md5, Sha256}` with `parse` and `hash_hex`, and
  `parse_authorization` for the `Digest` header dialect (quote-aware).
  Two RFC 2617 test vectors pinned as regressions.
- **`Registrar`** — stateful challenge/response engine on top of the
  existing `CredentialStore`: issues short-lived nonces (5-minute
  default TTL, configurable), re-challenges on bad response / stale
  nonce / unknown user with fresh nonce and optional `stale=true`,
  and uses constant-time equality on the response comparison.
- **UAS handles `REGISTER`**: no registrar → 200 OK (dev mode).
  Registrar attached → 401 Unauthorized + `WWW-Authenticate: Digest
  realm=…, nonce=…, qop="auth", algorithm=MD5` on missing or bad
  auth, 200 OK on valid auth. `UasServer::with_registrar(reg)`
  builder.
- **`RequestSummary`** gained `request_uri` + `authorization` fields;
  `summarize_request` extracts both.
- Nine new `auth::digest` unit tests (RFC vectors, parser, MD5/SHA-256
  round-trips, bad password, unknown user, stale nonce).
- Three new integration tests in `crates/smiths-sip/tests/register.rs`:
  `register_challenge_then_authenticate`,
  `register_wrong_password_re_challenges`,
  `register_without_registrar_is_accepted_blindly`.
- Workspace deps gained `md-5` `0.10`, `sha2` `0.10`, `hex` `0.4`.

### Added — Plugin invocation loop (P4 slice 2)

- **`smiths-plugin::controls`**: strict JSON-schema-ish validator with
  structured `ValidationError { field, reason, hint }`. Supports the
  subset plugins actually use today — `type`, `minimum`, `maximum`,
  `enum` — and rejects unknown control keys with a `supported: [...]`
  hint the agent can self-correct from. Eight unit tests cover the
  happy and every failure path.
- **`synthesize` MCP tool** (sixth built-in, served over MCP stdio +
  A2A HTTP): looks up the plugin, verifies it provides `ai.tts`,
  checks the voice against the declared list, runs the controls
  through `validate_controls`, dispatches `synthesize` to the plugin
  sidecar, returns the plugin's audio payload. Invalid input
  short-circuits with `-32602 invalid-argument` before ever touching
  the plugin.
- **`ai-tts-mock` plugin** grew a real `synthesize` handler: shells
  out to macOS `say` with a voice-id → macOS-voice map, reads the
  8 kHz WAV back, and returns `{codec, sample_rate, frames,
  duration_ms, audio_base64, voice}`. Linux fallback is silent audio
  so CI still works. Validation (voice, codec, sample_rate) lives on
  both sides.
- **`voice_agent.py`** rewired: dropped its local `subprocess say`
  TTS, gained `McpStdioClient.call_tool(name, args)` with full
  request/response correlation (multi-threaded pending-queue), and
  now does `mcp.call_tool("synthesize", ...)` → base64 decode → RTP
  stream. The agent's code contains **zero** speech-synthesis logic
  now — it's pure control over MCP.

### Changed

- `ToolRegistry` has 6 built-ins (was 5); `registry_contains_builtins`
  test updated.
- `smiths-plugin` re-exports `PluginEntry` so `smiths-mcp` can hold a
  clone of the sidecar + descriptors inside `SynthesizeTool`.

## [0.5.0] - 2026-04-18

Phase 4 slice 1 — the plugin platform's foundation lands. Two crates
that were stubs since v0.0.0 (`smiths-sidecar`, `smiths-plugin`)
become real: subprocess supervisor with JSON-RPC 2.0 over stdio,
`CapabilityDescriptor` + `AiRegistry`, fail-partial plugin scanner.
MCP grows two new tools (`list_ai_providers`, `describe_provider`)
served identically over MCP stdio and A2A HTTP. A reference Python
plugin (`ai-tts-mock`) exercises the full handshake end-to-end.
Actual invocation (`speak` / `transcribe` / `llm_chat`) lands next
slice.

### Added — Plugin platform (P4 slice 1: sidecar handshake)

- **`smiths-sidecar`** promoted from stub to real: subprocess
  supervisor + JSON-RPC 2.0 over newline-delimited stdio,
  `tokio::process` under the hood. `Sidecar::spawn()` runs the
  plugin with its directory as CWD, stderr forwarded to engine
  tracing with `plugin=<name>`. Request/response correlation by id
  in a shared pending-queue, per-call timeouts, `kill_on_drop` safety
  net. Two self-contained tests via a shell-script stub.
- **`smiths-plugin`** promoted from stub to real:
  - `Manifest` (TOML, `deny_unknown_fields`) with `name`, `version`,
    `type = "sidecar"` (wasm/script reserved), `entry`, `provides`,
    `abi`, `description`. Rejects unsupported ABI majors with a
    clear error.
  - `CapabilityDescriptor` — common envelope (`capability`, `plugin`,
    `model_id`, `abi`, `latency_ms`, `concurrency`) plus an opaque
    `extra` for capability-specific fields (voices, controls, ...).
    Validates `ai.*` namespace.
  - `AiRegistry` — concurrent registry of loaded plugins + their
    descriptors, keyed by plugin name. `snapshot()`, `capabilities()`,
    `shutdown_all()` for the lifecycle.
  - `load_plugins(root, registry)` — fail-partial scanner: walks the
    plugins directory, spawns each as a sidecar, runs the
    `describe_capabilities` handshake, sanity-checks descriptors
    against the manifest's `provides`, registers successes. Missing
    root dir is **not** an error (operators turn plugins on by
    creating the directory).
  - 8 unit/integration tests, including a full shell-script plugin
    roundtrip.
- **MCP tools** grown from 3 → 5:
  - `list_ai_providers` — summary of every loaded plugin with
    filterable capability list.
  - `describe_provider` — full capability descriptor(s) for one plugin.
  - Both obey the standard `Tool` trait, so they're served identically
    over MCP stdio and A2A HTTP.
- **`smiths-cli` / `[plugins]` config**:
  - New `[plugins]` section, default `dir = "plugins"`.
  - At startup, CLI calls `load_plugins` and logs a summary
    (`plugins ready loaded=[...]` / per-plugin `plugin load failed`
    warnings).
  - `ai_registry.shutdown_all()` drains sidecars during graceful
    shutdown.
- **Reference plugin** at `plugins/examples/ai-tts-mock/`:
  - `plugin.toml` declares `provides = ["ai.tts"]`, abi `1.0`.
  - `main.py` (pure stdlib) implements `describe_capabilities` /
    `shutdown` / `ping` per the spec. Returns a realistic `ai.tts`
    descriptor: three voices (Russian + English), PCM+PCMU output,
    rate/pitch/volume controls with full JSON-Schema constraints,
    streaming hints, latency advisories.
  - Synthesis itself is stubbed — the next slice (P4 + P22) wires
    `speak` to RTP injection.
- **`ToolContext`** grew an `AiRegistry` field; `smiths-mcp` now
  depends on `smiths-plugin` to wire the capability surface.
- Release binary: 3.1 MB → **3.4 MB** (plugin loader + JSON-RPC
  supervisor).

## [0.4.0] - 2026-04-18

Phase 5 slice 2 — MCP grows a real push channel, and the first
end-to-end voice-agent demo lands on top of it. The agent spawns the
engine, drains `notifications/call/*` frames over stdio, parks a SIP
UA on a rendezvous key, and runs a full STT → LLM → TTS pipeline
against bridged RTP. TTS is real (macOS `say`); STT and LLM are
stubbed at exactly the call sites where the post-MVP `ai.*` plugins
will slot in.

### Added — MCP server-pushed notifications

- MCP stdio server now emits JSON-RPC notifications on dialog
  lifecycle: `notifications/call/created` and
  `notifications/call/terminated`, published by subscribing to the
  engine's `SipEvent::DialogCreated` / `DialogTerminated`. Single-loop
  multiplex on stdout — no mutex needed.
- `smiths_mcp::mcp::run_stdio` signature grew an `EventBus` argument.

### Changed — `--mcp stdio` is now additive

- `--mcp stdio` no longer suppresses SIP, health HTTP, or A2A.
  MCP stdio runs alongside whatever else is configured, so an agent can
  spawn the engine as a subprocess *and* have the engine serve real
  incoming calls at the same time. stdin EOF still terminates the
  process (the shutdown token is triggered).
- Logs routed to stderr when `--mcp stdio` is active so stdout stays
  on the JSON-RPC wire.

### Added — Voice-agent demo (`examples/python-client/`)

- **`voice_agent.py`** — spawns the engine with `--mcp stdio`, drains
  MCP push notifications, parks a `SipUAC` on rendezvous key
  `voicebot`, and on an incoming call runs a **STT → LLM → TTS**
  pipeline against the bridged RTP. TTS is real (macOS `say`); STT and
  LLM are stubbed pending the plugin system (P22 of post-MVP). The
  stubs sit exactly where the real `ai.*` plugin calls will land.
- **`voice_caller.py`** — simulated inbound caller: dials
  `sip:voicebot@engine`, streams a greeting WAV, records the agent's
  reply, and hangs up.
- README gained a "Voice agent" section with run instructions and an
  honest breakdown of what's real vs. mocked + where the real plugins
  slot in.

## [0.3.0] - 2026-04-18

Phase 5 first slice — real control plane. The engine grows a typed
`Tool` abstraction served by two protocol adapters (MCP stdio + A2A
HTTP) over the same registry. A live event-bus subscriber feeds tools
a current view of dialogs. Two new Python demos drive both adapters
with pure stdlib.

### Added — Control plane: MCP + A2A

- `smiths-mcp` crate promoted from stub to real implementation.
  - **`Tool` trait + `ToolRegistry`**: adapter-agnostic operations.
    Both MCP and A2A register the same tool set.
  - **`ControlState`**: subscribes to the SIP event bus and maintains a
    live view of dialogs (live + recently-terminated). Tools read from
    it; adapters never touch dialog state directly.
  - **Built-in tools**: `list_calls` (filter by phase), `get_call_status`,
    `health`. All return structured JSON per a declared JSON Schema.
  - **MCP stdio adapter** (`mcp` module): JSON-RPC 2.0 over line-
    delimited stdin/stdout. Implements `initialize`, `initialized` /
    `notifications/initialized`, `ping`, `tools/list`, `tools/call`,
    `shutdown`. Protocol version `2024-11-05`. Logs diverted to stderr
    so stdout stays clean.
  - **A2A HTTP adapter** (`a2a` module): JSON-RPC 2.0 over HTTP POST
    `/a2a`, discovery via `/.well-known/agent.json`, plain `/health`.
    Same tool set as MCP.
  - **Resource trait**: scaffold for the next pass (resources not yet
    implemented; tool set is sufficient for this release).
  - Eleven unit + integration tests covering control-state lifecycle,
    tools, MCP dispatch, and JSON-RPC error frames.
- `smiths-core::config` gained `[mcp]` and `[a2a]` sections
  (`McpConfig { enabled_http, http_bind }`,
  `A2aConfig { enabled, bind }`).
- `smiths-cli`:
  - New `--mcp stdio` flag. When set, the binary runs only the MCP
    stdio server — no SIP, no health HTTP, logs routed to stderr.
    Exits on stdin EOF or SIGTERM.
  - In default mode, spawns a `ControlState` drain task and optionally
    the A2A HTTP server when `a2a.enabled = true`.
  - `smiths-ready` log line now includes `a2a_enabled`.
- Python samples (`examples/python-client/`):
  - **`mcp_demo.py`** — spawns the engine in `--mcp stdio` mode, walks
    the full JSON-RPC handshake (`initialize`, tools/list, tools/call).
    Pure stdlib — no `mcp` SDK dependency.
  - **`a2a_demo.py`** — `urllib`-only HTTP client: reads the agent
    card, lists tools, invokes each. Demonstrates that A2A and MCP are
    the same tool set over a different wire.
- README updated: explains the two control-plane adapters, includes a
  ready-to-paste Claude Code MCP config block.
- Release binary: 2.8 MB → **3.1 MB** (axum HTTP for A2A + MCP plumbing).

## [0.2.0] - 2026-04-18

Phase 1 slice 2 — full INVITE/ACK/BYE dialog lifecycle with SDP
offer/answer, a byte-transparent media bridge between two UAs
(audio end-to-end), a real-binary e2e test harness, a pure-stdlib
Python client sample, and a clean-architecture refactor that removes
every cross-sibling crate dep.

### Changed — Clean-architecture refactor (no cross-sibling deps)

- **Trait seams in `smiths-core`.** Three new modules host cross-crate
  abstractions so siblings never import one another:
  - `core::media` — `MediaFabric` trait (async `allocate` / `bridge` /
    `release_*`), opaque `EndpointId` / `BridgeId` tokens (both
    `Serialize`), `MediaError`.
  - `core::sdp` — `SdpNegotiator` trait + `NegotiationOutcome` enum
    (`Accepted { answer_body, remote_media } | Mismatch | Malformed`).
    SIP only sees the outcome; the parse tree stays in `smiths-sdp`.
  - `core::call` — serializable `DialogRecord`, `DialogState`,
    `DialogKey`. Satisfies the **HA snapshot guardrail** — every
    field is pure data, runtime resources live behind token IDs.
- **`UdpMediaFabric` in `smiths-media`.** Owns every RTP socket;
  hands out opaque tokens to the signaling layer. `bridge_forwards_*`
  tests exercise the full path.
- **`Negotiator: SdpNegotiator` in `smiths-sdp`.** Parses the offer,
  extracts the peer RTP endpoint, and returns `NegotiationOutcome`
  from a single trait method. Old `NegotiationResult::Answer` path
  remains for intra-crate use.
- **`smiths-sip` depends on `smiths-core` only.** `smiths-sdp` and
  `smiths-media` moved to `[dev-dependencies]` — integration tests
  wire the real impls, the library itself does not.
- **`UasServer::new`** now takes `(transport, bus, Arc<dyn MediaFabric>,
Arc<dyn SdpNegotiator>)`. The UAS holds `DialogRecord`s + a
  `DialogKey → BridgeId` map; every socket lives in the fabric.
  Rendezvous pairing, `BYE` teardown, and endpoint release go through
  trait methods.
- **`BindSpec` newtype in `core::config`.** Replaces `Vec<SocketAddr>`
  in `SipConfig::bind` with a `Vec<BindSpec>`; today parses `"ip:port"`
  literals, rejects interface-name syntax (`"wg0:5060"`) with a clear
  error pointing at roadmap P16 — **proxy/VPN guardrail** satisfied at
  the type level.

### Changed — Supporting

- `UasServer::new` signature changed (see above). All integration tests
  and the CLI updated to wire `UdpMediaFabric` + `Negotiator` through
  the trait objects.
- `smiths-cli` gains `smiths-media` and `smiths-sdp` as deps so it can
  instantiate one shared fabric and per-bind negotiators.
- `Cargo.toml` workspace: added `async-trait` to shared dependencies.
- `docs/architecture/01-crate-layout.md` — new "Layering invariant — no
  cross-sibling deps" section documenting the dependency-inversion
  seam; dep graph and responsibility table updated.

### Added — Python client sample

- `examples/python-client/` — pure-stdlib Python 3.9+ demo: a tiny SIP
  UAC (`SipUAC`), μ-law codec, RTP v2 packet builder, WAV I/O helpers,
  sine-wave generator. Three runnable scripts:
  - `demo_call.py` — in-process two-UA round-trip through the engine.
  - `speaker.py` / `listener.py` — two-terminal (or two-host) variant
    using a shared rendezvous key.
- Exercises the engine's rendezvous bridge over real UDP with no Python
  dependencies. README documents install, two-UA usage, troubleshooting,
  and the MCP migration path (Phase 5).

### Added

spawns the real `smiths-net` binary with a temp TOML on ephemeral
ports, polls `/health`, drives `OPTIONS` + an unknown method over UDP,
sends `SIGTERM`, and asserts a clean exit. Pure Rust, no external
tooling (unix only for now — Windows signal path is a follow-up).

- `tempfile` added to `[workspace.dependencies]`.
- UAS now answers `INVITE` with `100 Trying` + `200 OK` (with a `Contact`
  header), creates an early in-memory dialog, confirms it on `ACK`, and
  tears it down on `BYE` with `200 OK`. `BYE` against an unknown dialog
  returns `481`. Dialog state is keyed by `(Call-ID, local-tag,
remote-tag)` per RFC 3261.
- `SipEvent::DialogCreated` and `SipEvent::DialogTerminated` published on
  the event bus.
- Three new integration tests (`crates/smiths-sip/tests/invite.rs`):
  `invite_establishes_dialog_ack_then_bye`,
  `bye_without_dialog_returns_481`,
  `invite_retransmit_replays_same_200`.

### Changed

- `UasServer::new` now returns `Result<Self, Error>` — it reads the
  transport's local address to precompute a `Contact` header.
- `INVITE` responses now carry an SDP answer body; `build_response` /
  `respond` take an explicit body slice and compute `Content-Length`.
- Release binary size: 2.6 MB → **2.8 MB** (SDP + media bridge +
  fabric + negotiator trait plumbing; still comfortably under 20 MB).

### Added — Media bridge (audio end-to-end)

- `smiths-media::bridge` with `Bridge` and `Leg` — a byte-transparent
  two-leg UDP forwarder. One `recv_from` / `send_to` task per direction
  driven by a shared `CancellationToken`. Unit-tested on loopback.
- **Rendezvous bridging in the UAS.** Two `INVITE`s whose Request-URI
  user-part matches (e.g. both to `sip:room-1@engine`) are paired: the
  engine extracts each offer's media endpoint from SDP, spins up a
  `Bridge` between the engine's allocated sockets, and the two UAs
  exchange RTP through us. A `BYE` from either side tears the bridge
  down.
- `smiths-sip` now depends on `smiths-media` (acknowledged sibling-dep
  debt; future work will route bridge lifecycle through the event bus).
- `smiths-testkit` grew a real test toolkit:
  - `TestUac` — INVITE with a PCMU-only SDP offer, reads `100`/`200`,
    parses the engine's SDP answer, sends ACK and BYE.
  - `rtp::RtpPacket` — minimal RTP v2 encode/decode (no extensions /
    CSRCs / padding).
  - `codec` — bit-exact G.711 μ-law encoder/decoder.
  - `signal::sine_wave` — tone generator returning `Vec<i16>`.
  - `wav::write_mono_pcm16` — minimal RIFF/WAVE writer (PCM-16 mono)
    so a human can open the received audio.
- New integration test `two_uas_call_preserves_audio_byte_for_byte`:
  two `TestUac`s INVITE `sip:call-1@engine`, engine bridges, UA-A sends
  50 frames of 1 kHz sine encoded as PCMU @ 8 kHz, UA-B receives and
  asserts a middle-tail slice is bit-identical with the sent μ-law
  stream. The received audio is also written to
  `/tmp/smiths-call-received.wav` for manual listening.
- Internal extensions supporting the bridge:
  - `RequestSummary` gained `ruri_user`; `summarize_request` parses the
    user-part out of `sip:…@…` / `sips:…@…` / `<sip:…@…>` Request-URIs.
  - `sdp_remote_rtp` extracts the peer RTP endpoint from an SDP offer
    (media-level `c=` with session-level fallback).

### Added — SDP

- New `smiths-sdp` crate with a minimal RFC 8866 subset:
  - Types: `SessionDescription`, `Origin`, `ConnectionInfo`,
    `MediaDescription`, `MediaKind`, `RtpMap`, `Direction`.
  - Parser: accepts `v=`, `o=`, `s=`, `c=`, `t=`, `m=`, `a=rtpmap`,
    and the four direction attributes. Tolerates bare `\n` endings.
  - `Display` impls serialize back to wire SDP with CRLF endings.
  - Offer/answer `Negotiator` with PCMU / PCMA / Opus passthrough.
    Picks the first offered codec whose `(name, clock)` matches the
    engine's supported list; static payload types without `rtpmap`
    (legacy PCMU=0, PCMA=8) are recognized. Returns
    `NegotiationResult::Mismatch` → MVP guardrail for transcoding.
  - Eight unit tests (parse, round-trip, bare-LF, codec pick, legacy PT,
    direction reversal, mismatch, missing-version rejection).
- UAS wired to the negotiator:
  - `INVITE` with `Content-Type: application/sdp` is parsed and
    negotiated; if Mismatch → `488 Not Acceptable Here`; on malformed
    SDP → `400 Bad Request`.
  - On accept, allocates a fresh UDP socket (OS-chosen ephemeral port)
    and publishes the port in the SDP answer's `m=`/`c=`. The socket is
    held on the dialog record so step 3 can forward RTP through it.
- New `Content-Type` / body extraction in `summarize_request` and
  header/body splitting used by both parser and response builder.
- Integration tests (`crates/smiths-sip/tests/sdp.rs`):
  `invite_with_sdp_offer_gets_sdp_answer`,
  `invite_with_only_unknown_codecs_returns_488`.

## [0.1.0] - [2026-04-18]

Phase 1 slice — SIP signaling over UDP with an `OPTIONS`-answering UAS
and the MVP guardrails (`Transport` trait, `CredentialStore` trait) in
place. Full RFC 3261 transaction FSMs, TCP/TLS transports, REGISTER, and
digest challenge/response land in subsequent passes.

### Added

- `smiths-sip` crate:
  - `Transport` trait with message-level (not byte-stream) semantics —
    MVP guardrail for later TCP, TLS, QUIC, SOCKS-tunneled, WebTransport
    backings.
  - `UdpTransport` implementation with a spawned reader task feeding an
    mpsc channel, cancellation-aware shutdown.
  - `UasServer` that parses with `rsip`, answers `OPTIONS` with `200 OK`
    and rejects other methods with `405 Method Not Allowed`, preserving
    all mandatory headers (`Via`, `From`, `To` with added `tag`,
    `Call-ID`, `CSeq`) per RFC 3261 §8.2.6.
  - UDP retransmission dedupe via a bounded `DashMap` keyed by `Via`
    branch.
  - `auth::CredentialStore` trait + `InMemoryCredentialStore` — MVP
    guardrail for pluggable subscriber databases (SQLite, Postgres,
    LDAP, sidecar) without core changes.
  - `Error` type via `thiserror`.
- `smiths-core`:
  - `SipConfig` (`bind`, `transports`, `drain_timeout_secs`) with defaults
    (`0.0.0.0:5060`, UDP only).
  - `SipTransport` enum covering UDP / TCP / TLS; only UDP is wired in
    this phase.
  - `SipEvent::{RequestReceived, ResponseSent, ParseError}` published
    on the event bus.
- `smiths-cli`:
  - Per-bind UDP SIP spawn at startup.
  - Graceful shutdown drains SIP tasks before the health endpoint and
    publishes `SystemEvent::ShutdownComplete`.
- `examples/config.toml`: `[sip]` section with defaults.
- Integration tests (`crates/smiths-sip/tests/options.rs`):
  - `options_returns_200_ok`
  - `unknown_method_returns_405`
  - `retransmission_replays_cached_response`
- Six new unit tests (summary parsing, response building, tag uniqueness,
  credential store CRUD).
- `dashmap`, `rsip`, `bytes` added to `[workspace.dependencies]`.

### Changed

- Workspace version bumped `0.0.0` → `0.1.0`.
- Release binary size: 2.4 MB → **2.6 MB** (rsip + dashmap overhead;
  still comfortably under the 20 MB target).
- `smiths-cli` log line at startup now includes `sip_binds` and
  `sip_transports`.

## [0.0.0] - [2026-04-17]

Phase 0 — Foundation. Workspace scaffolding, core runtime primitives, and a
boot-and-shutdown binary. No SIP / media / plugins yet.

### Added

- Cargo workspace (`resolver = "3"`, edition 2024, MSRV 1.85) with 11 crates:
  `smiths-core`, `smiths-proto`, `smiths-sip`, `smiths-sdp`, `smiths-media`,
  `smiths-plugin`, `smiths-wasm`, `smiths-sidecar`, `smiths-mcp`,
  `smiths-cli`, `smiths-testkit` (all but `smiths-core` and `smiths-cli`
  are placeholders).
- Workspace lints: `unsafe_code = "forbid"`, clippy pedantic warn with
  pragmatic allows.
- Centralized dependency versions in `[workspace.dependencies]`.
- Release profile tuned for size (thin LTO, 1 codegen unit, stripped
  symbols).
- `rust-toolchain.toml` pinning stable channel with rustfmt + clippy.
- GitHub Actions CI: fmt + clippy + test + release build.
- `examples/config.toml` with commented defaults.
- `smiths-core`:
  - `config::Config` layered loader (defaults → TOML file → `SMITHS__*` env,
    `deny_unknown_fields`).
  - `bus::EventBus` over `tokio::sync::broadcast`.
  - `event::{Event, SystemEvent}` with `#[non_exhaustive]`.
  - `shutdown::Shutdown` over `tokio_util::CancellationToken`, handling
    SIGINT/SIGTERM on unix and Ctrl-C elsewhere.
  - `error::Error` via `thiserror`.
  - Seven green unit tests (bus round-trip, config defaults/env/TOML, shutdown
    cancel).
- `smiths-cli` binary `smiths-net`:
  - `clap` CLI (`--config`, `--log`), with `RUST_LOG` honored as override.
  - `tracing-subscriber` JSON or pretty output selected by config.
  - Axum `GET /health` endpoint with graceful shutdown tied to the cancel
    token.
- Release binary size: **2.4 MB** (target was < 20 MB).

### Docs

- `README.md` project pitch.
- `CONTRIBUTING.md` — prerequisites, dev loop, conventions, PR checklist.
- `LICENSE` — Apache-2.0.
- `docs/openswitch.md` — authoritative spec.
- `docs/architecture/` — overview, crate layout, plugin system, MCP + ops,
  post-MVP scope.
- `docs/plans/` — roadmap, MVP phase docs (0 through 6), post-MVP phases
  (P7–P23), and a live implementation TODO.

### Fixed

- `.gitignore`: added `.DS_Store` to the ignore list.

[0.7.0]: https://github.com/mindhalla/smiths-net/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/mindhalla/smiths-net/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/mindhalla/smiths-net/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/mindhalla/smiths-net/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mindhalla/smiths-net/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mindhalla/smiths-net/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.1.0
[0.0.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.0.0
