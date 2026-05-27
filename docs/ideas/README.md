# Ideas & perspective technologies

Brainstorm surface for smiths-net. Not a roadmap — see `docs/plans/` for
scheduled work. This file holds **hypotheses**, sized roughly, filtered
through the architecture we already have. Read
`docs/architecture/04-post-mvp-scope.md` first; that's the contract this
doc respects.

## Evaluation framework

Every entry below is scored against four axes. An idea that fails axis 1
or 2 is filed under "don't build, even if trendy".

1. **Does it solve a real user problem?** Not a vendor problem, not a
   resume-driven problem — an actual operator or caller need.
2. **Does it fit the plugin model?** We keep the core small on purpose.
   Ideas that force core surgery need a *much* higher payoff bar.
3. **Is the latency budget compatible?** Media has a 150 ms mouth-to-ear
   budget, signaling 500 ms end-to-end. Anything that breaks this on the
   hot path is an instant no.
4. **Does it compose with existing MVP guardrails?** (`docs/architecture/04-post-mvp-scope.md §Summary matrix`)

Categories below list concrete tech, an honest take on payoff vs cost,
and where it slots in our tiers.

---

## 1. AI, LLMs, voice agents

### Already scheduled

- **Local + cloud AI provider plugins** (P22) — Whisper, Piper, Ollama,
  OpenAI, Anthropic behind the `ai.*` capability namespace.
- **Vector store for RAG** (P23) — Qdrant / pgvector / Weaviate behind
  `storage.vector`.

### Worth adding

- **Real-time ASR during the call.** Sidecar plugin subscribed to
  `on_rtp_frame` (opt-in, degrades to 300 ms chunks on the control
  plane). Live captions, live translation, intent extraction. **Payoff:
  huge.** Enables IVR without phrase trees, voice agents, compliance
  monitoring. Target: Whisper-streaming or faster-whisper on CPU, Moshi
  for lower-latency conversational mode.
- **Voice agents (LLM at the other end of the call).** Caller talks to
  an LLM impersonating a persona — support agent, booking flow, triage
  nurse. Stack: ASR → LLM → TTS, each a P22 plugin. Orchestrator sidecar
  wires them together. **Payoff: genuine — this is the most commercially
  interesting direction in 2026.** Competition: Vapi, Retell, Bland,
  Deepgram. Our edge would be self-hostable + open plugin model.
- **Call summarization / action extraction.** Post-call Whisper →
  LLM → structured JSON. CDR enrichment via `storage.cdr`. Works today
  once P22+P23 land; no new core.
- **Voice cloning / consented synthesis.** XTTSv2 / StyleTTS2 / 11Labs
  as a TTS provider. Adds consent + watermark metadata in the capability
  schema. **Risks: abuse.** Needs explicit consent flow in the plugin
  manifest.
- **Emotion / sentiment per turn.** Lightweight WASM plugin on the audio
  path (VAD + SER model). Feeds dashboards. **Small payoff, low cost.**
- **Agent-to-agent negotiation (A2A).** Already listed as P20. Deeper:
  two *voice* agents speaking to each other over the bridge with the
  engine as mediator. Interesting research direction, unclear commercial
  need today. Ship P20 first, revisit.
- **RAG over call history.** The engine stores CDRs + transcripts;
  embeddings go into the vector store; MCP tool
  `search_calls_semantic(query, k)` surfaces similar past calls. **Big
  value for support / sales workflows.** Fits P22+P23 cleanly.

### Overhyped / skip

- **AGI orchestrating entire PBXs.** We're building infrastructure, not
  fantasy. Ship boring reliability first.
- **Fine-tuning models per customer inside the engine.** Offload to
  external training infra; the engine just runs inference.

---

## 2. Blockchain & smart contracts

Honest framing: **not in the hot path, ever** — consensus latency (≥100 s
of ms even on "fast" chains) is incompatible with voice. But a handful of
adjacent use cases are legitimate and plugin-shaped.

### Plausible

- **Audit-trail CDR anchor.** Every N minutes the engine publishes a
  merkle root of completed CDRs to a public chain (Bitcoin OP_RETURN,
  Ethereum calldata, Solana memo). Tamper-evident without exposing PII.
  **Slot: sidecar writing to `storage.cdr` publishes + on-chain publish
  hook.** Real users: regulated industries (healthcare, finance, legal
  recording).
- **Decentralized identity for SIP users.** ENS / Handshake / did:web
  resolve to credentials used by the `CredentialStore` trait. URI like
  `sip:alice.eth@engine` becomes routable. Niche but ideologically
  aligned with the "self-hostable" ethos.
- **Tamper-evident recordings.** SHA-256 of the recording + signed
  timestamp on-chain; the blob lives in `storage.recording`. Legal /
  evidentiary workflows.
- **Pay-per-minute voice.** Lightning Network / L2 rollup as payment
  backend. Sidecar opens a channel on `on_call_state_change(Early)`,
  streams sats per minute, closes on `Terminated`. Niche but clean plugin
  scope. See Reticulum, LN-URL for prior art.
- **Token-gated access rooms.** Holder of NFT X can join `sip:room@engine`.
  Just an auth plugin that checks on-chain state. Silly for most cases,
  real for DAO communities.

### Skip

- **Smart-contract-driven dial plan** — writing rules on-chain for
  something that changes hourly is absurd cost.
- **"Blockchain-native SIP" replacing RFC 3261** — solution in search of
  a problem; breaks interop with every existing softphone.
- **HA / clustering via blockchain** — Raft (P15) is strictly better for
  this.

---

## 3. IoT, smart home, industrial

### Already scheduled

- **Home Assistant + MQTT bridges** (P21).

### Worth adding

- **Matter / Thread integration.** Smart doorbells, intercoms, alarm
  panels. Matter is the first IoT standard that matters. Sidecar plugin
  speaking Matter maps button presses → `make_call` via MCP, and call
  events → Matter notifications.
- **Car telephony** (Android Auto / CarPlay SIP trunking). Well-defined
  profile exists. Plugin-addressable — we just need outbound trunking +
  Bluetooth HFP-style codec (CVSD, mSBC), which slots into P12
  (transcoding) or simply passthrough with the right codec advertised.
- **Industrial SIP**: paging, intercom zones, multicast announcements.
  SIP already supports this, we just need `m=audio` multicast awareness
  in SDP + the fabric sending to a group instead of a leg.
- **Alarm monitoring protocols** — SIA, ContactID over SIP. Niche
  vertical but has known commercial demand and fits as a sidecar.
- **Smart meters / telemetry over SIP-PRESENCE**. Existing RFCs; weird
  but real in some industrial deployments. Low priority.

### Research-y

- **Voice-first IoT control** without a screen — caller speaks to the
  engine, LLM parses intent, engine speaks to HA/MQTT. P22 + P21 gives
  this for free.

---

## 4. Mesh / P2P / decentralized networking

The honest view: **SIP is already federated** (DNS SRV, trunking). Most
"decentralized voice" tech is re-solving a problem SIP solved in 2002.
But a few angles are worth watching.

### Worth adding

- **WireGuard / Tailscale overlay** for private federations (already P16).
- **Reticulum** (LXMF) as an alternative low-bandwidth signaling plane
  for disaster / off-grid scenarios. Interesting adapter behind the
  `ControlProtocol` trait from P20. Not commercial, but alignment with
  mesh + resilience narrative.
- **Meshtastic bridge.** LoRa mesh radios + SIP intercom. Legit for
  remote sites, disaster response, expeditions. Sidecar plugin.
- **libp2p for peer discovery.** DHT-based lookup of `sip:user@handle`
  without DNS. Fits `ControlProtocol` adapter in P20.
- **Matrix interop.** Matrix already has VoIP. Bridge Matrix rooms ↔
  engine rendezvous keys for cross-network calling. Niche but clean.

### Skip

- **Putting RTP on a blockchain / DHT for media routing.** Latency
  kills it.
- **Tor for media.** ≥1 s RTT makes voice useless. Tor for signaling
  only is plausible in censorship-resistance scenarios.

---

## 5. Edge / local-first / offline

### Already well-aligned

The engine is already single-static-binary, local-first-by-default,
stdio-MCP-capable. These are edge virtues.

### Worth adding

- **Raspberry Pi / embedded profile.** Cross-compile target, musl +
  aarch64. Whisper tiny + Piper small + 2 concurrent calls fits on a
  Pi 5. Kitchen-table PBX. **Cheap, lots of demo value.**
- **Fully-offline mode.** Feature flag that refuses to load any
  `ai.*.cloud` plugin, forbids outbound DNS, runs only local models.
  Compliance / airgap / sovereignty narrative.
- **Federated learning for call quality.** Local models that learn
  user-specific noise profiles, share gradients (not audio). Research-y
  but on-brand.
- **WebAssembly Components (WIT / wit-bindgen).** Next-gen WASM. When
  wasmtime promotes Components to stable, our Tier A1 plugins gain
  richer interfaces without breaking the security model. Watch wasmtime
  release notes.

---

## 6. Advanced media

### High-payoff

- **AI noise suppression.** RNNoise (WASM-friendly) for PSTN calls.
  Plugin subscribing to `on_rtp_frame`. Rust crate `nnnoiseless` exists.
  Dramatic quality improvement for mobile / noisy environments.
- **VAD gating** to save bandwidth + AI compute. Same plugin tier.
- **Lyra / Opus DTX** for low-bitrate scenarios (Meshtastic, satellite).
  Codec support is passthrough today; we just need to advertise.
- **Spatial / binaural audio for conferences** — once P14 lands, sum
  with HRTF for multi-party immersion. Interesting for VR / meeting
  room use cases.
- **Real-time speech translation** (ASR + MT + TTS) in-call. "Press 2
  for Spanish" killer. Latency target: ≤2 s turn-around with
  Whisper-streaming + NLLB + Piper. Hard but possible.
- **Adaptive bitrate / FEC.** Opus has RED + in-band FEC. Wire it in
  properly once media-plugin hooks land.

### Niche / research

- **Real-time voice avatars** (Moshi, NeRF-style). Fun demo, real
  compute cost.
- **AI-based echo cancellation** replacing traditional AEC. Standard
  WebRTC's AEC is already excellent; AI version wins only for edge
  cases.

---

## 7. Next-gen UX / transport

### Already planned

- **HTTP/3, SIP-over-QUIC** (P17).
- **WebTransport** (P19).
- **FlatBuffers wire format** (P18).

### Worth adding

- **AR / VR spatial calls.** Meta Horizon, Apple Vision Pro. Protocol:
  WebTransport + spatial audio metadata in an SDP extension. The engine
  stays codec-agnostic; the metadata sits in `a=` lines.
- **Voice-first agent UI (no screen).** MCP already supports this —
  an agent running on the engine *is* the interface. User calls in,
  LLM drives the whole workflow.
- **Browser-native phone** on bare web standards. WebCodecs + WebRTC
  data channels + WebTransport can replace SIP.js in modern browsers.
  Plugin adapter, not core.
- **Post-SIP signaling.** If WebRTC's ICE + data channels wins fully,
  consider a lightweight adapter under `ControlProtocol`. Don't abandon
  SIP — coexist.

---

## 8. Security & privacy

### Must-haves eventually

- **Full E2E encryption.** ZRTP (RFC 6189) for SRTP key agreement
  without server knowledge of keys. Standard, boring, underused. Our
  SRTP passthrough is a stepping stone (P6).
- **MLS (Messaging Layer Security)** for multi-party. Overkill for
  1:1, but the standard for secure conferences.
- **Metadata-minimized signaling.** No Call-ID leakage in logs, no PII
  in metrics labels. Audit the tracing spans.

### Bleeding edge

- **Post-quantum SIP-TLS.** Kyber (ML-KEM now) hybrid with classical.
  `rustls` already has experimental support. Becomes mandatory on a
  government timeline (NIST targeting 2030–35). **Worth implementing
  early for that niche.**
- **Confidential computing.** Run sensitive plugin tiers (recording,
  credentials) inside Intel SGX / AMD SEV enclaves. Very niche, very
  regulated industry. Only worth doing with a customer driving it.
- **Zero-knowledge proofs for authentication.** "Prove I'm on the
  whitelist without revealing my identity." Research-y; no current
  demand.
- **Homomorphic encryption for call mediation.** Mediation without
  seeing media. Thousands of × slowdown still; cross off for now.

---

## 9. Observability, ops, analytics

- **Automatic call quality scoring.** POLQA / PESQ are licensed; open
  alternatives: Moos-style heuristic from jitter + loss + latency. Our
  metrics exporter already has the raw numbers.
- **Real-time call analytics via MCP.** Subscribe to `sip://calls`
  resource, feed a dashboard. Already possible once P5 is in.
- **Anomaly detection on call patterns.** Isolation forests / ESS on
  the event stream to flag fraud (toll fraud, pump-and-dump IVR abuse).
- **Predictive scaling.** Time-series forecast of call volume → k8s
  HPA hints. Ordinary infra work, nothing VoIP-specific.
- **Chaos-style fault injection.** Extend `smiths-testkit` with packet
  loss, jitter, out-of-order injectors. Critical for maturity.

---

## 10. Cutting-edge / watchlist (6–24 months)

Technologies not ripe today but worth tracking for later adoption:

| Tech                       | Why it might matter                        | Mature enough? |
|----------------------------|--------------------------------------------|----------------|
| **Moshi** (Kyutai)         | Sub-200 ms full-duplex speech LLM          | Early 2026     |
| **Wasmtime Components**    | Richer WASM plugin interfaces              | Stabilizing    |
| **NIST-PQC final specs**   | Post-quantum SIP-TLS                       | Finalized 2024–25; adoption now |
| **Matter 1.3+ voice**      | Voice assistant on Matter devices          | 2025–26        |
| **Google A2A**             | Cross-vendor agent protocol                | 2025 launch    |
| **WebTransport in Safari** | Browser voice without SIP.js               | Landed 2025    |
| **QUIC V2**                | Improvements to our future SIP-over-QUIC   | Watch IETF     |
| **Apple AppIntents / SIP** | iOS Siri → SIP actions                     | Developing     |
| **5G SIP (IMS/VoNR)**      | Carrier-grade 5G voice profile             | Known, stable; niche |
| **Differential privacy**   | Analytics on call data without PII leak    | Frameworks stable |
| **Neural codec advances**  | Lyra v2, EnCodec, SNAC — sub-6-kbps voice  | Research → prod in 1–2 y |

---

## 11. Honest priority — if we had 6 months post-MVP

Not every idea above is worth chasing. If forced to pick concretely:

**Ship in this order after MVP closes:**

1. **P22 AI providers + P23 storage** (already scheduled) — unlocks 80%
   of what makes smiths-net commercially interesting.
2. **Real-time ASR + voice agents.** First use case on top of P22/P23.
   This is where industry money is today.
3. **AI noise suppression** (RNNoise plugin). Highest quality-per-effort
   ratio.
4. **P7 DTMF + P9 dialplan + IVR** — necessary for any realistic
   telephony deployment.
5. **P10 ICE/STUN/TURN** — required the moment the engine leaves
   loopback / single-LAN demos.
6. **Blockchain CDR anchor** — only if a paying customer asks for it.
   Don't build spec, build against need.
7. **Matter / Home Assistant bridge** demo — cheap, strong marketing
   payoff for the "self-hosted smart home PBX" persona.
8. **Pi 5 / aarch64-musl profile.** Low effort, big demo surface.

**Watchlist, don't build yet:**

PQ-TLS, Wasmtime Components, confidential computing, ZKP auth, VR
calls, voice cloning, mesh-radio bridges. All legitimate, none
critical for the next year.

**Don't build, even if asked:**

On-chain dial plan, Tor media, "blockchain-native SIP", on-core LLM
fine-tuning, video conferencing from scratch (use Jitsi integration),
monolithic AGI "meta-agent" that tries to do everything.

---

## How to add to this doc

1. New entry? Fit it under a category.
2. Evaluate against the four axes at the top. If it fails axis 1 or 2,
   file under "skip / don't build".
3. Link it back to an architecture guardrail
   (`04-post-mvp-scope.md §Summary matrix`). If no guardrail applies,
   that's a sign the idea might need a trait / ABI extension before it
   can be done cleanly.
4. Prefer plugin-addressable over core extension. Every core extension
   is a tax on every future maintainer.
