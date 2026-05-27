# Post-MVP Phases (v2+)

Features deferred from `openswitch.md §7`. Not scheduled; ordered here by
expected ROI and dependency order. Architectural slots are defined in
`docs/architecture/04-post-mvp-scope.md` — read that first for each item.

Each phase below is sized rough (S / M / L / XL). An **S** is a few days;
**XL** is a multi-month initiative.

---

## P7 — DTMF relay (RFC 2833 + inband)  [size: S]

**Depends on**: MVP guardrail "`emit_event` / `subscribe_event` in ABI" is
in place.

**Work**
- Ship a WASM plugin `dtmf-2833` that parses telephone-event RTP payloads
  and emits `emit_event("dtmf", {digit, duration, leg})`.
- Ship a sidecar plugin `dtmf-inband` (Python + `dsp-stream`-like lib) for
  inband detection on legs that opt in.
- Add MCP resource `sip://calls/{id}/dtmf` streaming DTMF events.

**Acceptance**: test UA sending RFC 2833 DTMF produces events visible to
an MCP subscriber within 100 ms of the tone.

---

## P8 — Subscriber DB backend  [size: S]

**Depends on**: MVP guardrail "`CredentialStore` trait in `smiths-sip::auth`".

**Work**
- New `CredentialStore` implementations: SQLite (bundled) and HTTP
  (generic webhook).
- Host call `auth_lookup(realm, user) → credentials` for plugin-provided
  backends.
- Config section `[auth]` with `backend = "sqlite" | "http" | "plugin"`.

**Acceptance**: REGISTER flow authenticates against a SQLite DB and an
HTTP webhook in separate test scenarios.

---

## P9 — Dialplan / IVR framework  [size: M]

**Category**: plugin-addressable. No core crate needed.

**Work**
- Reference sidecar plugin `dialplan-yaml` that routes by YAML rules
  (`from`, `to`, `time-of-day` matchers → destination rewrite).
- Reference IVR kit in `plugins/examples/ivr-kit/` (Python): plays
  prompts via `send_rtp`, detects DTMF via subscription, drives a YAML
  state machine.
- Document the pattern: IVR == sidecar + DTMF plugin + prompt files.

**Acceptance**: demo flow — "press 1 for sales, 2 for support" routes the
call to two different destinations, recorded in a `smiths-testkit` e2e
test.

---

## P10 — ICE / STUN / TURN  [size: L]

**Depends on**: MVP guardrail "`MediaEndpoint` abstraction in
`smiths-media`".

**Work**
- New crate `smiths-ice`: STUN binding (RFC 8489), TURN client (RFC
  8656), candidate gathering, connectivity checks.
- `IceMediaEndpoint` impl of `MediaEndpoint`.
- SDP extensions: `a=candidate`, `a=ice-ufrag`, `a=ice-pwd`,
  `a=ice-options`.
- Config section `[ice]` with STUN/TURN server list and credentials.

**Acceptance**: WebRTC browser (Chromium) establishes an audio call
through the engine from behind symmetric NAT.

---

## P11 — Video calls  [size: M]

**Depends on**: MVP guardrail "m-line list, not pair".

**Work**
- Extend SDP negotiator for `m=video` with H.264 passthrough.
- Extend Call FSM and media router for N media sessions per call.
- No transcoding — passthrough only (VP8/VP9/H.264).

**Acceptance**: two UAs establish a video call with audio + video RTP
bridged by the engine; no decode/encode anywhere in the engine.

---

## P12 — Transcoding  [size: L]

**Depends on**: P11 complete (m-line list proven), MVP guardrail
"`NegotiationResult::Mismatch` branch".

**Work**
- New crate `smiths-transcode`: Opus↔G.711 at minimum.
- Per-call CPU budget; refuse transcoded calls when budget is exhausted,
  returning `488`.
- Configurable codec preference matrix.
- Metrics: `smiths_transcode_active`, `smiths_transcode_cpu_ms_total`.

**Acceptance**: caller offering only Opus connects to callee offering
only G.711, both hear audio, CPU budget enforced under load test.

---

## P13 — FAX (T.38)  [size: M]

**Depends on**: MVP guardrail "`MediaSession` trait, not concrete RTP".

**Work**
- `UdptlSession` impl of `MediaSession` in `smiths-media`.
- SDP handling for `m=image udptl t38`.
- Reference tests with `spandsp` fax sender/receiver.

**Acceptance**: T.38 fax sent end-to-end through the engine round-trips
a one-page PDF without errors across 20 attempts.

---

## P14 — Conferencing / mixing  [size: L]

**Depends on**: MVP guardrail "`MediaFabric` trait".

**Work**
- New crate `smiths-mixer`: N:N audio mixer (sum + AGC, voice activity
  detection optional).
- `MixerFabric` impl of the media fabric trait; router selects fabric
  per call.
- MCP tools: `create_conference`, `join_conference`, `leave_conference`.

**Acceptance**: three UAs join a conference via MCP; each hears the
other two; join/leave does not disturb active participants.

---

## P15 — HA / clustering + persistent storage  [size: XL]

**Depends on**: MVP guardrails "FSM centralized" and "FSM `Serialize`".

**Work**
- Snapshot + replay of Call FSM state.
- Replication plugin (Raft via `openraft`, or simpler primary/secondary
  with a shared store).
- Pluggable state backends: in-memory (default), SQLite (single-node
  durability), Redis / Postgres (multi-node).
- Failover procedure documented; cluster health exposed via MCP
  `cluster://status`.
- External SIP load balancer guidance (Kamailio dispatcher module, AWS
  NLB for 5060 TLS).

**Acceptance**: kill the primary node during an active call; a standby
takes over within 5 s with the call still established (re-INVITE with
unchanged SDP acceptable).

---

---

# Transport & ecosystem modernization (P16–P21)

These follow §7 items but expand scope beyond telephony: modern proxy /
VPN, HTTP/2 + HTTP/3, FlatBuffers, WebTransport, agent-to-agent
protocols, and IoT bridges. Architectural slots in
`docs/architecture/04-post-mvp-scope.md §Beyond §7`.

## P16 — Proxy & VPN transports  [size: S]

**Depends on**: MVP guardrail "`Transport` trait abstracts socket creation"
and "config accepts interface names".

**Work**
- SOCKS5 and HTTP-CONNECT wrapping transports.
- Documented patterns for WireGuard / Tailscale co-deployment (engine
  binds to overlay interface).
- Optional embedded `boringtun` behind feature `wireguard`.

**Acceptance**: engine registers and receives a call while only reachable
via SOCKS5 egress; same flow via Tailscale MagicDNS hostname.

---

## P17 — HTTP/3 transport (MCP + SIP-over-QUIC)  [size: M]

**Depends on**: MVP guardrail "SIP transport trait: send/recv message,
not raw bytes"; axum ≥ 0.7 kept current for hyper/h2/h3 upgrade path.

**Work**
- MCP HTTP transport: h2 now (free with axum), h3 behind feature
  `mcp-http3` using `quinn` + `h3-quinn`.
- SIP-over-QUIC `Transport` impl per the IETF draft; opt-in via config.
- Performance bench vs TCP SIP under loss.

**Acceptance**: Claude Code connects to the engine over h2 and h3; a
lossy-link SIPP scenario completes faster over QUIC than over TCP.

---

## P18 — FlatBuffers plugin wire format  [size: S]

**Depends on**: MVP guardrail "no `prost` types leak out of
`smiths-proto`".

**Work**
- `WireFormat` trait; protobuf + flatbuffers impls.
- Manifest field `wire_format = "proto" | "flatbuffers"`.
- Reference WASM plugin benchmark (encode/decode) showing the tradeoff.

**Acceptance**: existing proto-based plugins unchanged; a new FlatBuffers
plugin processes `on_rtp_frame` at ≥ 2× throughput of the proto version
on a dev laptop.

---

## P19 — WebTransport  [size: M]

**Depends on**: P17 (HTTP/3 stack), MVP guardrail "`MediaSession` trait
covers non-RTP media".

**Work**
- `WebTransportListener` in `smiths-sip` for signaling.
- Data-stream `MediaSession` impl in `smiths-media` for application
  data.
- Demo: browser client (plain fetch + WebTransport) makes and ends a
  call through the engine.

**Acceptance**: a static HTML page using WebTransport places a call via
the engine's MCP-ish control channel; audio path via WebRTC (from P10 /
P11 work) or data channel echo for demo.

---

## P20 — Agent-to-agent (A2A) protocols  [size: M]

**Depends on**: MVP guardrail "tools/resources as Rust traits, MCP as
adapter".

**Work**
- Extract `ControlProtocol` trait out of `smiths-mcp`.
- Adapters: MCP (existing), Google A2A, ACP, generic HTTP webhooks.
- Discovery: well-known URIs, `.well-known/agent.json` (where specs
  define it).
- Security reuse: the existing auth + rate-limit layers work across
  adapters.

**Acceptance**: an A2A-speaking agent (reference: Google A2A sample
client) invokes `make_call` on the engine and reads `sip://calls/{id}`
equivalent, with the same Rust handler code serving both MCP and A2A
clients.

---

## P21 — IoT / smart-home bridges  [size: S]

**Depends on**: MVP guardrail "plugin bus payloads are opaque bytes".

**Work**
- Reference sidecar `plugins/examples/ha-bridge/` — Python, bridges
  Home Assistant events ↔ call events.
- Reference sidecar `plugins/examples/mqtt-bridge/` — generic MQTT
  pub/sub.
- Documentation on mapping SIP intercom / doorbell flows to Matter /
  Zigbee gateways.

**Acceptance**: a Home Assistant automation starts a SIP call when a
smart doorbell is pressed; on call end, an MQTT message is published
with call duration.

---

## P22 — AI providers (local + cloud)  [size: M]

**Depends on**: MVP guardrail "`provides = [...]` field in `plugin.toml`
and reserved `ai.*` capability namespace".

**Work**
- `ai_invoke(capability, request) → response` host function; dispatcher
  routes by capability with priority + health + budget.
- Reference sidecar plugins:
  - `ai-asr-whisper` — local Whisper (`whisper.cpp`), capability
    `ai.asr`.
  - `ai-tts-piper` — local Piper, capability `ai.tts`.
  - `ai-llm-ollama` — local LLM via Ollama, capability
    `ai.llm.completion`.
  - `ai-llm-openai` — cloud OpenAI, same capability; priority config
    decides local-first vs cloud-first.
  - `ai-embed-local` — local embeddings (gte / bge), capability
    `ai.embed`.
- MCP tools: `transcribe_call(call_id)`, `summarize_call(call_id)`,
  `translate(text, to)` — all implemented on top of capabilities.
- Unified request/response schema in `smiths-proto`.

**Acceptance**:
- Start engine with only local plugins (Whisper + Piper + Ollama); no
  network needed. Transcribe a live call, summarize via LLM, play TTS
  response into the call. All happens offline.
- Swap `ai-llm-ollama` for `ai-llm-openai` by config change; same MCP
  tool produces equivalent results from the cloud.
- Failover: on plugin timeout/error, dispatcher tries the next
  provider in priority order; metric `smiths_ai_failovers_total`
  increments.

---

## P23 — Pluggable storage backends  [size: M]

**Depends on**: MVP guardrail "`storage::*` traits defined in
`smiths-core::storage`, no DB clients in other crates".

**Work**
- Sidecar plugin adapters that forward `storage::*` trait calls over
  the plugin ABI using the `provides = ["storage.<kind>"]` declaration.
- Reference plugins:
  - `store-postgres` — `storage.credentials`, `storage.cdr`,
    `storage.recording`.
  - `store-sqlite` — same, for single-node deployments (also the
    bundled default).
  - `store-redis` — `storage.presence`, `storage.kv`.
  - `store-qdrant` — `storage.vector` (RAG, call-semantic search).
  - `store-influx` — `storage.cdr` time-series variant.
- MCP tools surface a minimum: `list_cdr(filter)`,
  `search_calls_semantic(query, k)` (needs P22 embeddings + P23
  vector).
- Schema migration strategy per backend documented; engine never runs
  migrations — the plugin or its operator does.

**Acceptance**:
- Default build with SQLite backing everything — single binary, no
  external deps, CDR visible via MCP.
- Swap to Postgres + Redis + Qdrant via config + loaded plugins; same
  MCP queries work; semantic call search returns relevant calls by
  conversation content.
- Storage plugin crash does not crash the engine; requests queue up to
  configurable depth then fail fast (metric
  `smiths_storage_errors_total{store, kind}`).

---

## P24 — Embedded DSL runtime (policy / dialplan)  [size: M]

**Depends on**: MVP guardrails "`Hook` trait dispatches, no hard-coded
tier matching" and "host-function signatures language-agnostic". Pairs
naturally with P9 (dialplan) and P22 (AI providers — scripts can call
`ai_invoke` for LLM-driven routing decisions).

**Work**
- New crate `smiths-script`. Default engine Rhai; `mlua` (Lua 5.4) and
  `starlark-rust` selectable via compile-time features (`script-lua`,
  `script-starlark`) and mutually exclusive.
- Manifest: `type = "script"`, `entry = "./route.rhai"`,
  `script_engine = "rhai" | "lua" | "starlark"`,
  `[resources] ops_per_call = 100_000`, `wall_clock_us = 500`.
- Host-function bridge: the same Rust closures already registered with
  the WASM host get a thin engine-side shim. No duplication of the
  capability list; `permissions` from the manifest filters identically.
- Budget enforcement: op-count via each engine's native limiter
  (Rhai `Engine::set_max_operations`, Lua debug hook, Starlark eval
  step limit); wall-clock via a deadline checked at the same hook
  points. Exceeding either yields `Err(Budget)`, hook output discarded,
  metric incremented.
- Hot reload via `notify` on the plugins dir. Old script drains
  in-flight calls; new one takes the next call. Previous revision kept
  as the rollback target if the new one errors 5× in a row.
- Reference plugin `plugins/examples/route-rhai/` — a ~20-line Rhai
  script that rewrites the Request-URI based on From-domain.
- MCP tool `put_script(name, source, engine)` — push a new script from
  an LLM agent; validation, permissions check, then atomic swap.

**Acceptance**:
- A Rhai script subscribed to `on_sip_request` rewrites the Request-URI;
  engine routes the call accordingly (e2e test in `smiths-testkit`).
- Editing `route.rhai` on disk (no restart) changes routing for the
  next call; an in-flight call completes under the old script.
- Op-count budget exhaustion returns `Err(Budget)`; call proceeds with
  hook output discarded; `smiths_script_budget_exhausted_total` metric
  increments.
- Lua and Starlark engines pass the same acceptance suite when the
  binary is built with `--features script-lua` / `--features
  script-starlark`.
- Claude Code pushes a new script via MCP and the next test call
  follows the new rule.

---

## P25 — `smiths-net init` interactive config wizard  [size: S]

**Depends on**: nothing. Pure-CLI addition; every subsystem's config
surface is already typed in `smiths-core::Config`, so the wizard just
asks the operator what they'd set, validates with figment's existing
deserializer, and writes `config.toml`.

**Work**
- New `smiths-net init` subcommand: walks through
  `[sip.bind]` → transports → `[auth]` backend choice → TLS cert
  paths (optional) → `[plugins.sandbox]` presets (permissive / hardened
  / custom) → `[ice]` on/off → `[observability]` health bind. Each
  prompt defaults to the safe value; `<enter>` accepts.
- Writes the resulting TOML to a target path (`--out`, default
  `./smiths-net.toml`) and round-trips it through `Config::load` to
  prove it parses before exit.
- Sandbox presets: "permissive" matches current defaults (dev);
  "hardened" pre-fills `no_new_privs=true` + `seccomp="allowlist"` +
  conservative rlimit caps for Kubernetes-style deployments.
- Optional `--non-interactive` + `--preset <dev|prod>` flags so the
  same wizard powers scripted installers (Ansible, Terraform).

**Acceptance**:
- `smiths-net init --out /tmp/c.toml --non-interactive --preset prod`
  produces a file that loads via `Config::load` without errors.
- Interactive run in a terminal accepts every default with bare
  `<enter>` and emits a valid config; mistyped values re-prompt
  rather than silently accept.
- CI run under `expect(1)` drives an interactive session end-to-end
  and asserts the emitted `[plugins.sandbox]` section matches the
  chosen preset.

---

## P26 — Plugin cookbook (multi-language, multi-locale, interactive)  [size: L]

**Depends on**: MVP guardrails "WASM + sidecar + script plugin ABIs
are stable" (landed pre-v0.23.0) and the capability descriptor format
in `docs/architecture/05-ai-plugin-protocol.md`. Pairs with P24
(scripts), P22 (AI providers), and P21 (IoT) — each adds cookbook
chapters.

**Work**
- Cookbook site (`docs/cookbook/` as source, static-site output).
  Two axes of variation, orthogonal:
  - **Programming-language tracks** — three tiers, each a full
    walkthrough per plugin host:
    - **WASM** — Rust, TinyGo, AssemblyScript.
    - **Sidecar** — Python, Node.js, Go, Ruby.
    - **Script** — Rhai (default), Lua, Starlark (P24 tiers).
  - **Human-language locales** — `en` (baseline source), `ru`,
    `es`, `zh`, `ar`. Site routes `/{locale}/{track}/{recipe}/`;
    `ar` flips to RTL layout (`dir="rtl"`); `zh` loads the CJK
    font subset; every page declares `<html lang>` and
    `hreflang` sibling links so search engines pick up the right
    one per region.
- Each recipe carries: manifest snippet, full source, host-function
  walkthrough, capability-permission rationale, testing via
  `smiths-testkit`. Covers the canonical hooks (`on_sip_request`,
  `on_rtp_frame`, `on_call_end`) with one worked example per host
  function per language.
- Interactive browser-side editor (Monaco + a compile-to-wasm path
  for the Rust chapters, transpiling via `rustc_codegen_gcc`'s
  emscripten wrappers or a pre-built `smiths-net-sdk` Wasm target)
  so readers can tweak a plugin in-page and run its tests against a
  bundled in-browser `smiths-testkit` WASM build.
- CI gate: every cookbook example compiles + passes its test in the
  workspace's existing `cargo test --workspace` so stale samples
  break builds rather than leaking into production. Translation
  sanity gate: a page missing from a non-`en` locale falls back to
  the `en` source with a visible "this page isn't translated yet"
  banner rather than 404.
- Search + cross-reference: `docs/architecture/` pages link into the
  cookbook's "capability reference" appendix, and each cookbook
  recipe links back to the architectural doc that explains *why*
  the API exists. The search index is built per-locale so
  ru / zh / ar queries don't get English hits mixed in.
- Translation workflow: source strings live in the `en/` tree; a
  nightly script extracts + merges into a Crowdin / Weblate
  project (choice deferred to the slice); code-switched blocks
  (manifests, Rust snippets) stay untranslated and are shared
  across locales to avoid drift between translated prose and
  un-translatable config.

**Acceptance**:
- A new operator picks a language, copy-pastes the "hello-world"
  recipe, and gets a running plugin inside 5 minutes (timed on a
  first-time user at a release retrospective).
- All cookbook examples live in the workspace under
  `plugins/cookbook/<lang>/<recipe>/` and are built + tested on
  every CI run.
- Interactive editor renders a plugin's test output within 2 s of
  the reader hitting "Run" (measured on a stock laptop,
  Chromium / Firefox latest).
- Search over the cookbook returns ≥3 relevant hits for common
  entry terms ("dtmf", "ai", "storage", "metrics") in every
  supported locale (`en` / `ru` / `es` / `zh` / `ar`).
- Arabic pages render RTL end-to-end: prose flows right-to-left
  but fenced code blocks stay LTR so syntax highlighting and
  line numbers behave.
- Chinese pages render with the CJK font subset (no tofu boxes)
  on a clean browser profile with no system CJK fonts installed.
- A page missing from a non-`en` locale falls back to the `en`
  source with a visible "this page isn't translated yet" banner.

---

## Dependency graph

```
  MVP ──┬── P7   (DTMF)
        ├── P8   (Subscriber DB)
        ├── P9   (Dialplan/IVR)        ◀── needs P7 + P8 for a realistic demo;
        │                                    P24 supercharges it
        ├── P10  (ICE/STUN/TURN)
        ├── P11  (Video)               ──▶ enables P12
        │       └── P12 (Transcoding)
        ├── P13  (FAX T.38)
        ├── P14  (Conferencing)        ◀── stronger with P12
        ├── P15  (HA / persistent)
        │
        ├── P16  (Proxy/VPN)
        ├── P17  (HTTP/3, SIP-over-QUIC) ──▶ enables P19
        │       └── P19 (WebTransport) ◀── also uses P10
        ├── P18  (FlatBuffers)
        ├── P20  (A2A protocols)
        ├── P21  (IoT / smart home)
        ├── P22  (AI providers)          ──▶ pairs with P23 for RAG
        │                                    and with P9 + P24 for smart IVR
        ├── P23  (Pluggable storage)     ◀── underpins P8, P15, P22
        ├── P24  (Embedded DSL runtime)  ◀── extends plugin tier model;
        │                                    pairs with P9 + P22
        ├── P25  (CLI init wizard)       ◀── pure DX; no code-path deps
        └── P26  (Plugin cookbook)       ◀── pulls from every plugin-
                                             authoring surface (WASM,
                                             sidecar, script / P24)
```

## Selection guidance

If the project must pick its next post-MVP investment, do **P8 → P7 →
P10** first. That unlocks production deployments behind NAT with real
subscriber auth and DTMF-driven IVR — the smallest path to a useful
commercial offering.

**Second wave** — in parallel tracks once the first is in:

- **P23 (Pluggable storage)** — land this *early* alongside P8; it
  formalizes persistence for every later feature (CDR, presence, HA,
  RAG).
- **P22 (AI providers)** — the primary differentiator; enables
  transcription, summarization, TTS responses, smart routing. Works
  offline with local models, scales up to cloud.
- **P16 (Proxy/VPN)** — cheap, unlocks enterprise deployments.
- **P17 (HTTP/3)** — sets up the transport stack for P19 and better MCP
  performance.

**Third wave** — ecosystem and reach:

- **P20 (A2A)** — aligns the control plane with the broader AI-agent
  ecosystem; complements the MCP investment.
- **P21 (IoT)** — showcases the plugin model; accelerates adoption in
  adjacent verticals.
- **P9 (Dialplan/IVR)** becomes substantially more powerful once P22 +
  P23 are in (LLM-driven IVR with conversation memory).
- **P24 (Embedded DSL runtime)** — land this alongside or just before P9.
  It turns dialplan from "Python sidecar + YAML" into "edit a Rhai
  script live from an MCP prompt"; pairs with P22 for LLM-decided
  routing (`ai_invoke` callable from within the script).

The rest (P18 FlatBuffers, P19 WebTransport, P11–P14 media) are
additive and driven by concrete user need.

P24 (Embedded DSL) is low-risk, high-leverage, and unblocks the operator
UX story around dialplan; prefer landing it before P9 so the dialplan
reference plugin can be written as a Rhai script rather than a Python
sidecar.

**Developer-experience track** — can run in parallel with any of the
above because it touches no runtime code paths:

- **P25 (CLI init wizard)** — ship first; costs a few days and every
  new operator benefits. Ideal pre-cut ahead of the first tagged
  1.0 release.
- **P26 (Plugin cookbook)** — lands after enough of the post-MVP
  waves are in to have a rich surface to document. Pair it with
  P24's release so the "edit a Rhai script from the browser"
  recipe is ready on day one.
