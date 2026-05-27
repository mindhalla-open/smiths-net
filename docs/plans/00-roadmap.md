# Implementation Roadmap

Seven phases, each ending in a demonstrably working increment. Phases are
ordered so every previous deliverable remains useful; no phase throws work
away.

| Phase | Title                       | Deliverable (demoable)                             |
|-------|-----------------------------|----------------------------------------------------|
| 0     | [Foundation](phase-0-foundation.md)         | Workspace, CI, empty binary that boots, `/health` ok |
| 1     | [SIP core](phase-1-sip-core.md)             | Responds to `OPTIONS` / `REGISTER` with digest auth from `pjsua` |
| 2     | [Media passthrough](phase-2-media.md)       | Two UAs make a G.711 call through the engine (B2BUA) |
| 3     | [WASM plugins](phase-3-wasm-plugins.md)     | Rust + TinyGo plugins tap SIP and RTP hooks          |
| 4     | [Sidecar plugins](phase-4-sidecar-plugins.md) | Python sidecar reacts to call state changes        |
| 5     | [MCP server](phase-5-mcp.md)                | Claude Code drives `make_call` via stdio MCP         |
| 6     | [Hardening & ops](phase-6-hardening.md)     | TLS SIP, Prometheus metrics, Docker image, e2e tests |

## Phase gates

Each phase defines explicit **acceptance criteria**. A phase is "done" only
when every criterion is demoable and covered by at least one test in
`smiths-testkit`.

## Risk register (top 5)

| Risk | Phase | Mitigation |
|------|-------|------------|
| SIP parser edge cases (fragmentation, 0-len headers) | 1 | Start with `rsip` rather than hand-rolled parser; write fuzz tests early |
| RTP timing drift in B2BUA passthrough | 2 | Use `webrtc-rtp` session, measure jitter per leg, ship a jitter metric from day one |
| WASM host-function ABI churn | 3 | Freeze proto schema before hook work; add ABI version check |
| Sidecar backpressure on slow plugins | 4 | Bounded outbox + drop-oldest for non-critical hooks; hard timeout on sync hooks |
| MCP schema drift between spec versions | 5 | Pin MCP spec version; integration test against Claude Code CLI |

## Working style

- No phase lasts more than ~2 weeks of focused work; if it grows, split it.
- Each phase lands on `main` via a single merge (feature branches
  `phase-<n>-<slug>`).
- Each phase ends with a tagged checkpoint (`v0.<n>.0`).
- Docs in `docs/architecture/` are the source of truth; if a phase diverges
  from them, update the doc in the same PR.
- No scope creep: features for later phases are tracked as TODOs in
  `docs/plans/backlog.md` (created lazily, not part of this plan).

## Cross-cutting work tracked separately

These are not in the phase plan because they're needed throughout:

- **Fuzzing** — SIP parser fuzzed from phase 1; RTP parser from phase 2.
- **Benchmarks** — `criterion` benches added incrementally: parser,
  dispatcher, WASM invocation.
- **Docs** — update `docs/architecture/*` when decisions change.
- **Post-MVP guardrails** — during v1 PR review, check each new module
  against `docs/architecture/04-post-mvp-scope.md §Summary matrix`. The
  guardrails (trait boundaries, `Serialize` on FSM, m-line list, etc.)
  are mandatory in MVP even though the features they enable are deferred.

## Post-MVP (v2+)

Work deferred from `openswitch.md §7`. Not scheduled; ordered by expected
ROI. Each item has an integration sketch in
`docs/architecture/04-post-mvp-scope.md`. Concrete, phase-shaped entries
live in `docs/plans/post-mvp.md`.

| Bucket | Items |
|--------|-------|
| Telephony features     | DTMF relay, FAX (T.38), Video |
| Control plane          | Dialplan / IVR, Subscriber DB, A2A protocols, Embedded DSL runtime (Rhai/Lua/Starlark) for live-editable policy |
| Media plane            | ICE / STUN / TURN, Transcoding, Conferencing / mixing, WebTransport |
| Transport & wire       | Proxy / VPN, HTTP/2 + HTTP/3 (SIP-over-QUIC, MCP), FlatBuffers |
| AI & data              | AI providers (local Ollama/Whisper/Piper + cloud OpenAI/Anthropic/…), pluggable storage (SQL, KV, document, vector, time-series) |
| Ecosystem integrations | IoT / smart-home bridges (MQTT, CoAP, Matter, Home Assistant) |
| Resilience             | HA / clustering, Distributed sessions, Persistent storage |
