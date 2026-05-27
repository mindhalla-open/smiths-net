# Post-MVP Scope (from `openswitch.md §7`)

Each item below is **not** part of v1. This doc exists so MVP design choices
do not silently block them. Every item has a category and an integration
sketch.

## Categories

- **Plugin-addressable** — implementable under the existing two-tier plugin
  ABI with no changes to the engine core.
- **Core extension** — needs new modules or substantial changes inside
  `smiths-core` / `smiths-sip` / `smiths-media`.
- **Infra-level** — primarily operational (external store, load balancer,
  orchestration), minimal engine change.

Each entry lists: **what it is**, **category**, **where it slots in**, and
**what MVP must avoid doing** so we don't paint ourselves into a corner.

---

## 1. Video calls

- **Category**: core extension.
- **Where it slots in**: `smiths-sdp` already treats `m=` lines generically;
  `smiths-media` would grow per-`m`-line sessions (currently "one audio leg
  pair per call"). RTP session code is reusable.
- **MVP guardrails**: Call FSM stores media sessions as a list keyed by
  m-line index, not a pair of named fields. SDP negotiator returns a list of
  `(mid, codec, port)` tuples. One audio entry today; N tomorrow.

## 2. Conferencing / mixing

- **Category**: core extension (new crate).
- **Where it slots in**: a new `smiths-mixer` crate sits between media
  sessions. Router switches from 1:1 forwarder to an N:N mixer bus when the
  call is marked as a conference.
- **MVP guardrails**: `smiths-media::router` abstracts the forwarding step
  behind a `MediaFabric` trait (implemented by `PassthroughFabric` in v1).
  A future `MixerFabric` replaces it without touching call FSM or SIP.

## 3. Real-time transcoding

- **Category**: core extension (optional crate + CPU budget).
- **Where it slots in**: a `smiths-transcode` crate with per-codec
  encoders/decoders, invoked by the media fabric when legs negotiate
  different codecs. Needs an explicit CPU budget because it is expensive.
- **MVP guardrails**: codec negotiation already rejects mismatches with
  `488`. The negotiator returns `NegotiationResult::Mismatch` rather than
  failing opaquely, so a future transcoder hook can branch on it.

## 4. Dialplan / IVR

- **Category**: plugin-addressable.
- **Where it slots in**: a sidecar plugin subscribed to
  `on_sip_request` (INVITE) and `on_call_state_change`. It drives routing
  by emitting `send_sip` or returning rewritten messages. IVR menus are
  sidecar-owned state machines that play prompts via `send_rtp`.
- **MVP guardrails**: ensure `on_sip_request` can rewrite the Request-URI
  and `send_rtp` can inject pre-encoded audio. Both are already in the ABI.

## 5. Subscriber database

- **Category**: plugin-addressable (or infra-level).
- **Where it slots in**: a sidecar plugin handles REGISTER auth lookups
  and produces digest credentials. Core asks via a new host call
  `auth_lookup(realm, username) → credentials` — trivial to add under the
  existing `permissions` model.
- **MVP guardrails**: digest auth module (`smiths-sip::auth`) loads
  credentials through a `CredentialStore` trait with an in-memory impl for
  v1. External backend = new impl, no other changes.

## 6. HA / clustering

- **Category**: core extension + infra-level.
- **Where it slots in**: Call FSM serializes to a snapshot type; a
  replication plugin (sidecar, pluggable transport) streams snapshots to
  peers. On failover, a new node rehydrates dialogs.
- **MVP guardrails**: keep Call FSM state in one place (`smiths-core::call`)
  and make it `serde::Serialize`. Do not scatter call state across modules.
  Prepare but do not implement snapshot emission.

## 7. ICE / STUN / TURN / full NAT traversal

- **Category**: core extension inside `smiths-media`.
- **Where it slots in**: candidate gathering happens before SDP offer; a
  new `smiths-ice` crate owns STUN/TURN client logic, returning candidate
  lists to `smiths-sdp`. Media sockets learn to handle ICE binding
  requests.
- **MVP guardrails**: media socket creation goes through a
  `MediaEndpoint` abstraction that currently returns a plain
  `UdpSocket`. Future `IceMediaEndpoint` returns an ICE-aware socket with
  the same byte-level interface.

## 8. FAX (T.38)

- **Category**: core extension (new transport) + possibly plugin.
- **Where it slots in**: T.38 uses UDPTL, not RTP. `smiths-media`
  extends with a `UdptlSession` alongside `RtpSession`. Switch happens on
  `m=image udptl t38` in SDP.
- **MVP guardrails**: don't assume "media session == RTP session" in
  `smiths-core`. Use a `MediaSession` trait; `RtpSession` is the only
  impl in v1.

## 9. DTMF relay (RFC 2833 / inband)

- **Category**: plugin-addressable (detection) + tiny core hook (events).
- **Where it slots in**: RFC 2833 is an RTP payload type; a WASM plugin
  subscribed to `on_rtp_frame` can detect telephone-event payloads and
  emit `emit_event("dtmf", {digit, duration})` on the plugin bus. Inband
  detection needs DSP — sidecar with a Python/Go library.
- **MVP guardrails**: `on_rtp_frame` already gets payload type. Add
  `emit_event` / `subscribe_event` host funcs (already in the ABI spec)
  and we're set. No core changes beyond ABI implementation.

## 10. Distributed sessions / replication / persistent storage

- **Category**: infra-level + tie-in with HA (#6).
- **Where it slots in**: same as #6. Persistent storage of dialogs survives
  crashes, not just failover.
- **MVP guardrails**: same as #6 — `Serialize` call state, keep it
  centralized.

---

---

# Beyond `openswitch.md §7` — transport & ecosystem modernization

The items above came from the original spec. The following were added
later as post-MVP goals: modern proxy / VPN, HTTP/2 & /3 (and beyond),
FlatBuffers, WebTransport, agent-to-agent (A2A) protocols, IoT / smart
home. Same category + slot-in + guardrail structure.

## 11. Modern proxy + VPN transports

- **What**: SIP and media reachable through SOCKS5 / HTTP-CONNECT proxies
  for corporate egress, and co-deployed with VPN stacks (WireGuard,
  OpenVPN, Tailscale) for private-network overlays.
- **Category**: plugin-addressable for proxy chaining; infra-level for VPN.
- **Where it slots in**: `smiths-sip::transport::Transport` trait already
  abstracts socket creation. New impls: `SocksTransport`, `HttpConnectTransport`
  wrap an inner transport. VPN is orchestration — the engine binds to a
  `wg0` / `ts0` interface with no code change. For embedded WireGuard,
  a `boringtun`-based submodule can co-run in the binary (feature-gated).
- **MVP guardrails**: the `Transport` trait takes `(local_bind, peer) →
  Stream` rather than assuming direct kernel sockets. Config accepts any
  interface name, not just addresses.

## 12. HTTP/2, HTTP/3, and beyond

- **What**: HTTP/2 and HTTP/3 (QUIC) for MCP transport, and "SIP over
  QUIC" (draft-ietf-sipcore-sip-in-quic) for signaling. Keep the
  abstraction ready for HTTP/4 or future revisions.
- **Category**: core extension (transport layer).
- **Where it slots in**:
  - MCP: `axum` already supports h2; adding h3 via `quinn` +
    `h3-quinn` behind feature `mcp-http3`.
  - SIP: a new `QuicTransport` implementation of the `Transport` trait
    in `smiths-sip`. Framing per SIP-over-QUIC draft.
- **MVP guardrails**: the SIP transport trait does not assume byte-stream
  vs datagram — it exposes "send one SIP message" and "receive one SIP
  message" primitives. HTTP transport for MCP is already pluggable via
  `axum`'s hyper version; keep `axum` ≥ 0.7 so h2/h3 upgrade is clean.

## 13. FlatBuffers wire format

- **What**: FlatBuffers as an alternative to Protobuf for plugin IPC —
  zero-copy access, useful for high-throughput sidecar plugins and
  hot-path WASM hooks.
- **Category**: core extension (wire format layer).
- **Where it slots in**: `smiths-proto` introduces a `WireFormat` trait
  with `encode` / `decode`. Protobuf is the default impl; FlatBuffers
  impl lives beside it. Plugin manifest declares
  `wire_format = "proto" | "flatbuffers"`; dispatcher uses the declared
  format for that plugin. Core-internal bus types stay Rust-native
  (never serialized internally).
- **MVP guardrails**: no crate outside `smiths-proto` imports
  `prost`-generated types *by their concrete names*. They re-export
  through `smiths-proto::msg::*`. Swap of the wire format affects one
  crate.

## 14. WebTransport

- **What**: WebTransport (over HTTP/3) for browser-native signaling and
  data channels, enabling web clients without a full WebRTC stack on the
  engine side.
- **Category**: core extension (new transport); rides on top of the
  HTTP/3 work in #12.
- **Where it slots in**: a `WebTransportListener` implementing both the
  SIP `Transport` trait (for signaling frames) and a new "data stream"
  abstraction in `smiths-media` for application data (text, control,
  small media). Complements, not replaces, WebRTC once ICE (#7) lands.
- **MVP guardrails**: `smiths-media` should not assume "media == RTP
  over UDP" (shared with #8). The same `MediaSession` trait covers
  future WebTransport data sessions.

## 15. Agent-to-agent (A2A) protocols

- **What**: Beyond MCP (which is primarily LLM ↔ tool), support
  agent-to-agent protocols: Google A2A, Agent Communication Protocol
  (ACP), generic HTTP webhooks, and emerging standards. Agents talk to
  agents through the engine — the engine mediates voice + data.
- **Category**: core extension at the control-plane layer
  (generalization of `smiths-mcp`).
- **Where it slots in**: rename-in-spirit `smiths-mcp` to a broader
  `ControlProtocol` trait. MCP is one adapter; A2A, ACP, raw HTTP
  webhooks, and (later) matrix/xmpp are others. Tools and resources stay
  the same underneath — the trait only handles framing, auth, and
  discovery.
- **MVP guardrails**: tools and resources are defined as Rust traits
  in `smiths-mcp::tool` / `::resource`, **not** as MCP-specific structs.
  The MCP adapter wraps them. Keep MCP wire types isolated to the
  adapter module.

## 17. AI model integration (local + cloud)

- **What**: the engine and its plugins **use** AI internally — ASR
  (Whisper, Vosk), TTS (Piper, ElevenLabs), LLM completion (local via
  Ollama / llama.cpp / vLLM, cloud via OpenAI / Anthropic / Gemini /
  Bedrock / Azure OpenAI), embeddings, classifiers. Distinct from MCP,
  which exposes the engine *to* agents; this is AI *inside* the engine.
- **Category**: plugin-addressable in full. Core never links an AI SDK.
- **Where it slots in**: AI is delivered by sidecar plugins (and WASM for
  lightweight CPU-only models). A plugin declares an **AI capability** in
  its manifest: `provides = ["ai.asr", "ai.tts", "ai.llm.completion",
  "ai.embed"]`. Other plugins and MCP tools request a capability via a
  host function `ai_invoke(capability, request) → response`; the
  dispatcher routes to a provider plugin (by priority + availability).
  Local vs cloud is just *which plugin is loaded* — the core sees a
  single abstraction.
- **MVP guardrails**: add a `provides = [...]` field to `plugin.toml`
  now; loader validates and stores it, even though routing by
  capability is post-MVP. Cost: ~20 lines. Benefit: plugin manifests
  are already forward-compatible. Also reserve the `ai.*` capability
  namespace in docs so early plugin authors don't squat on it.

## 18. Database paradigm flexibility

- **What**: pluggable data backends for every persistence concern —
  subscriber credentials (SQL / KV), call detail records (SQL / time
  series), recording metadata (document), presence / session state
  (KV / Redis), conversation embeddings and RAG (vector — pgvector,
  Qdrant, Weaviate, Milvus), HA / cluster state (KV / SQL / Raft log).
  The engine links *zero* database drivers; each concern goes through
  a trait backed by a plugin.
- **Category**: plugin-addressable in full.
- **Where it slots in**: small typed traits in `smiths-core::storage`:
  - `CredentialStore` (shared with §5 / P8)
  - `CdrSink` — call detail records
  - `RecordingStore` — blob + metadata
  - `PresenceStore` — KV-shaped
  - `VectorStore` — for embeddings
  - `ClusterState` — used by P15 (HA)
  Each trait has an **in-memory or SQLite default** so MVP works out of
  the box, and a **plugin adapter** that forwards calls over the
  sidecar ABI. Plugins declare `provides = ["storage.cdr",
  "storage.vector", ...]`.
- **MVP guardrails**: define the trait set in `smiths-core::storage`
  **now** with in-memory impls; don't let any other crate call a concrete
  DB client. `CredentialStore` is the only one exercised in MVP (digest
  auth). The rest are stubs — costs almost nothing but locks in the
  boundary.

## 20. Embedded DSL runtime for policy / dialplan

- **What**: a scripting sub-tier inside Tier A — `.rhai` (or Lua /
  Starlark) files loaded by a new `smiths-script` crate, running under
  the same host-function surface, permissions model, and hook ABI as
  WASM. Purpose: dialplan and policy rules that are painful as WASM
  (toolchain friction, recompile per edit) and fragile as sidecars
  (runtime drift, dependency hell). Edit-to-live-call latency is
  essentially file save.
- **Category**: core extension (one new crate) + small ABI tweak
  (add `"script"` to the manifest `type` enum and a `script_engine`
  field).
- **Where it slots in**: `smiths-script` implements the same `Hook` trait
  the dispatcher already uses for WASM and sidecar. Host functions reuse
  the same Rust closures through a thin engine-specific binding layer —
  no fork of the capability list. Hot reload rides on the plugin
  lifecycle already specified in `02-plugin-system.md §Lifecycle`.
- **MVP guardrails**: the dispatcher must dispatch through the `Hook`
  trait — adding a third impl is a one-line registration, not an
  `if tier == "wasm" else if tier == "sidecar"` chain. Host-function
  signatures in `smiths-plugin` must stay language-agnostic — no
  `wasmtime::Caller` or `wasmtime::Store` types leaking into shared
  trait definitions. The manifest loader must treat the `type` field as
  an open enum (forward-compatible), not a fixed two-variant match.

## 19. IoT / smart-home bridges

- **What**: Interop with MQTT, CoAP, Matter, Zigbee gateways, Home
  Assistant. Use cases: SIP intercom → Home Assistant event, smart
  doorbell places a SIP call, voice control triggers IoT actions through
  the engine's plugin bus.
- **Category**: plugin-addressable in full. No core change.
- **Where it slots in**: sidecar plugins bridge SIP events ↔ IoT
  protocols. `on_call_state_change` → MQTT publish; MQTT subscription →
  `make_call` via host function. Reference plugin
  `plugins/examples/ha-bridge/` (Home Assistant, Python) lands with P9
  (dialplan) or standalone.
- **MVP guardrails**: the plugin bus (`emit_event` / `subscribe_event`)
  must carry opaque payload bytes, not SIP-specific structs. It already
  does per the ABI — keep it that way.

---

## Summary matrix

| Item                          | Category                    | MVP guardrail to keep |
|-------------------------------|-----------------------------|-----------------------|
| Video                         | core extension              | m-line list, not pair |
| Conferencing / mixing         | core extension              | `MediaFabric` trait   |
| Transcoding                   | core extension              | `NegotiationResult::Mismatch` branch |
| Dialplan / IVR                | plugin-addressable          | `on_sip_request` can rewrite RURI; `send_rtp` works |
| Subscriber DB                 | plugin-addressable          | `CredentialStore` trait in auth |
| HA / clustering               | core + infra                | FSM centralized + `Serialize` |
| ICE / STUN / TURN             | core extension              | `MediaEndpoint` abstraction |
| FAX (T.38)                    | core extension              | `MediaSession` trait, not concrete RTP |
| DTMF (RFC 2833 / inband)      | plugin-addressable          | `emit_event` host func in ABI |
| Distributed / persistent      | core + infra                | FSM `Serialize` (shared with #6) |
| Proxy / VPN transports        | plugin-addressable + infra  | `Transport` trait, interface-agnostic bind |
| HTTP/2, HTTP/3, beyond        | core extension              | SIP transport trait: send/recv message, not bytes |
| FlatBuffers wire format       | core extension              | no `prost` types leak out of `smiths-proto` |
| WebTransport                  | core extension              | shared `MediaSession` trait (w/ #8) |
| A2A protocols                 | core extension (ctrl plane) | tools/resources as Rust traits, MCP as adapter |
| AI models (local + cloud)     | plugin-addressable          | `provides = [...]` field in `plugin.toml`; reserve `ai.*` namespace |
| Database paradigm flexibility | plugin-addressable          | `storage::*` traits in `smiths-core`; no DB clients in other crates |
| IoT / smart home              | plugin-addressable          | plugin bus payloads are opaque bytes |
| Embedded DSL runtime (policy) | core extension              | `Hook` trait dispatches, no `if tier ==` chain; host-fn signatures language-agnostic; manifest `type` is open enum |

These **guardrails are part of MVP** — they cost almost nothing to adopt
now and would be expensive retrofits later. PR reviews in v1 should check
each new module against this list.

---

## Staged DTLS-SRTP rollout (slices 1.2–1.5)

DTLS-SRTP is the second half of "SRTP + TLS-signaled media"; SDES
landed in v0.20.0 but only covers SIP-to-SIP. WebRTC interop is the
real use case and it's large enough to split across four slices so
each one ships something usable:

| Slice | Target  | What it lands                                                                 | What still doesn't work |
|-------|---------|-------------------------------------------------------------------------------|--------------------------|
| 1.2   | v0.25.0 | SDP surface (`fingerprint`, `setup`, `ice-*`, `candidate`) + cert helper. UAS `488` + `Warning: 399` on DTLS-SRTP offers. | Any real DTLS handshake — offers get a clear diagnostic, not a silent drop. |
| 1.3   | v0.26.0 | `smiths-dtls` crate wrapping `webrtc-dtls`. Per-leg handshake, fingerprint verify, `use_srtp` extension (AES_CM_128_HMAC_SHA1_80). | ICE — DTLS works against a direct peer; no NAT traversal yet. |
| 1.4   | v0.27.0 | `smiths-ice` crate: STUN binding, host-candidate gathering, connectivity checks. SDP `a=candidate` emit/parse. | Trickle ICE, TURN, symmetric-NAT traversal. Loopback works end-to-end. |
| 1.5   | v0.28.0 | Headless-Chromium harness in `smiths-testkit`, trickle ICE, DTLS/SRTP error-path polish. | — this is when WebRTC interop is **closed** (roadmap item 2 done). |

### Why the SDP surface lands alone (slice 1.2)

Three reasons:

1. **Clear operator diagnostic.** Without it, a WebRTC client offering
   `UDP/TLS/RTP/SAVP` gets a naked `488` with no hint that DTLS-SRTP is
   missing. That's hostile both to early adopters and to ops: nothing
   in the response distinguishes "we don't know this codec" from "we
   don't speak DTLS-SRTP yet."
2. **Unblocks later slices.** Slice 1.3's handshake code reads
   `a=fingerprint:` off the parsed offer; slice 1.4's ICE reads
   `a=candidate:`. Landing the types first means each follow-on slice
   starts from a plumbed data model rather than re-parsing strings.
3. **Cert helper is cheap and reusable.** `SelfSignedCert::generate`
   via `rcgen` is < 50 LOC; having it in `smiths-core::dtls` now means
   slice 1.3 can focus on the handshake itself, not cert provisioning.

### What "WebRTC interop" means as done

After slice 1.5 closes, the engine can:

- Accept a `UDP/TLS/RTP/SAVP` offer with ICE candidates + fingerprint.
- Gather host candidates, run STUN connectivity checks.
- Perform a DTLS handshake per leg, extract SRTP keys via
  `extract_srtp_keying_material`.
- Bridge between a Chromium-driven caller and a SIP counterparty.

What's still not in scope after 1.5: TURN, trickle-beyond-loopback,
full browser-to-browser through the engine as a B2BUA (that's
conferencing, P14), DTLS-SRTP for IPv6-only networks.

