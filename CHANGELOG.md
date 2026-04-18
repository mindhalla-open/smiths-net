# Changelog

All notable changes to **smiths-net** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[0.5.0]: https://github.com/mindhalla/smiths-net/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/mindhalla/smiths-net/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/mindhalla/smiths-net/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mindhalla/smiths-net/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.1.0
[0.0.0]: https://github.com/mindhalla/smiths-net/releases/tag/v0.0.0
