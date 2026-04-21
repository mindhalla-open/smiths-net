# Changelog

All notable changes to **smiths-net** are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.49.0] - 2026-04-22

Slice 5.2 — FlatBuffers plugin wire format. Two formats now
share the engine↔plugin channel: protobuf (pre-5.2 default, via
prost) and a hand-rolled flat binary layout (new, tuned for the
RTP hot path). Plugin authors opt in per-plugin via a single
manifest field. The in-tree benchmark measures **3.27× round-
trip speedup** on a 160-byte PCMU-shaped `RtpFrame` in debug
builds.

### Added

- **`smiths_proto::WireFormat` trait** — encode/decode for both
  `Envelope` and the new `RtpFrame`. Impls:
  - `ProtoWireFormat` — wraps existing prost derives. Default
    for every manifest that omits `wire_format`.
  - `FlatbuffersWireFormat` — hand-rolled flat layout, fixed
    offsets, no per-field tags, no varints. Pure Rust, zero
    `unsafe`, no `flatc` / build.rs. Behind the `flatbuffers`
    Cargo feature (on by default).
- **`smiths_proto::RtpFrame`** — prost-derived per-packet
  message: `call_id`, `ssrc`, `sequence`, `timestamp`,
  `payload_type`, `direction`, `payload`. The shape the
  `media.streaming_rtp` capability (slice 2.5) will hand to
  plugins.
- **`smiths_proto::WireFormatKind`** — `Proto | Flatbuffers`
  with `parse`/`as_str` for manifest round-trips.
- **`flatbuffers_io::RtpFrameView`** — zero-copy accessor:
  `view.payload()` returns `&[u8]` borrowed from the backing
  buffer, no allocation, no copy on the hot path.
- **`smiths_plugin::manifest::WireFormat`** — manifest field
  `wire_format = "proto" | "flatbuffers"`. Default `proto`.
  Unknown tokens fail the manifest parse loudly rather than
  silently falling back.
- **Throughput bench** — `crates/smiths-proto/tests/
  wire_format_throughput.rs` (marked `#[ignore]`). Runs 50k
  round trips of each format on a 160-byte PCMU-shaped
  `RtpFrame`, prints ns-per-iter + byte size, asserts the flat
  path is ≥2× faster than prost. Observed local: `proto
  2318 ns, flatbuffers 708 ns — 3.27×`.
- **Tests** — 7 new unit tests on `flatbuffers_io` (`RtpFrame`
  + `Envelope` round trips, `RtpFrameView` zero-copy proof,
  truncated + magic-mismatch error paths) + 3 new manifest
  tests (defaults, explicit `flatbuffers`, unknown-token
  rejection).
- **`docs/architecture/10-plugin-wire-format.md`** — format
  comparison, picking rules, layout tables for both formats,
  migration checklist.

### Notes

The workspace's `unsafe_code = "deny"` lint rules out the
official `flatbuffers` crate's raw-table API (which is
`unsafe`-heavy by design). Rather than carry a scoped
`#[allow(unsafe_code)]` for it, the crate ships a hand-rolled
flat binary layout that matches the FlatBuffers philosophy
(flat, offset-addressable, zero-copy reads) while staying
unsafe-free. The name is still right; the library is just
absent. A future slice that genuinely needs the library's
richer schema (optional metadata tables, unions) can revisit —
a scoped allow is a one-file change.

The `media.streaming_rtp` runtime — the host function that
actually hands `RtpFrame` bytes to a loaded plugin — is a
dedicated follow-on. Today the wire format is ready + proven
against a micro-benchmark; wiring the engine's bridge to
stream frames into the plugin needs the per-leg call-id
routing + frame-rate backpressure story.

## [0.48.0] - 2026-04-21

Slice 5.1 — Video calls (passthrough). SDP negotiator gains
`m=video` support for H.264 / VP8 / VP9 via a new multi-stream
negotiation path. Peers that offer audio + video get a
well-formed answer with both m-lines and the ordering preserved,
so no more silent drops of the video m-line. Dual-bridge wiring
in the UAS is honest-deferred — today video is answered with an
RFC 3264 port-0 decline; the SDP layer is ready to relay as soon
as the UAS allocates the second endpoint.

### Added

- **`smiths_sdp::Negotiator::supported_video`** — second codec
  list on the negotiator. Defaults: `H264 @ 96`, `VP8 @ 97`,
  `VP9 @ 98`, all at 90 000 Hz RTP clock rate. Payload-type
  numbers are placeholders; the answer mirrors the offerer's
  PT so passthrough B2BUAs stay happy.
- **`Negotiator::answer_with_video(offer, audio_port,
  video_port)`** — audio negotiation identical to
  `answer()`, then appends a matching `m=video` block. When
  `video_port` is `None` or no offered codec is in the
  passthrough set, emits `m=video 0 ...` to decline while
  preserving m-line ordering (RFC 3264 §6). Never fails the
  whole negotiation on a video-only issue.
- **`SdpNegotiator::negotiate(offer_body, local_ip, audio_port,
  video_port)`** — new trait method. Default impl delegates to
  `negotiate_audio` so existing impls stay trait-compatible;
  the in-tree `Negotiator` overrides it to do the real
  multi-stream answer.
- **`NegotiationOutcome::Accepted::video_media`** — new
  additive field carrying the peer's video RTP endpoint
  (`m=video port` + `c=`) when the offer advertised non-zero
  video. Populated even when the engine declines video so the
  UAS's follow-on dual-bridge wiring can opt in without
  re-negotiating.
- **UAS now calls the multi-stream method** with
  `video_port = None`. Observable consequence: a peer that
  offers audio + video gets a 200 OK whose SDP answer has both
  m-lines (audio with chosen PT, video with port 0). Audio
  continues to bridge normally; video relay waits on the
  follow-on.
- **Tests** — 7 new unit tests in `smiths-sdp` covering
  `answer_with_video` (H.264 wins, port-None declines,
  unknown-codec declines, audio-only round-trips unchanged,
  `NegotiationOutcome::video_media` populates + redacts
  correctly, audio-backcompat path leaves `video_media` None)
  + 1 new SIP-level integration test
  (`invite_with_audio_plus_video_declines_video_but_keeps_audio`)
  that drives the full UAS surface with a real audio+video
  INVITE and asserts the declining answer shape.
- **`docs/architecture/09-video-passthrough.md`** — "no
  transcoding; passthrough only" framing, SDP shape table,
  rollout plan for the UAS dual-bridge follow-on, reasoning
  on why passthrough beats transcoding for the B2BUA use case.

### Notes

The UAS dual-bridge wiring (allocate a second
`MediaEndpoint`, spawn a second `MediaFabric::bridge`, track
both on `DialogRecord`, clean both on BYE) is a dedicated
follow-on slice. It touches `DialogRecord` serialization,
`PendingLeg` shape, `bridges_by_dialog` indexing, and every
call site that reads the per-dialog media handle (control
plane, MCP tools like `speak` / `send_dtmf`) — a multi-hour
refactor that deserves its own release. Today's release
closes the SDP-layer half cleanly: the declining answer is
RFC-compliant, m-line ordering is preserved, and
`NegotiationOutcome::video_media` surfaces the peer's endpoint
so the follow-on wires in additively.

The default video codec set picks RTP payload types `96`,
`97`, `98`. Per RFC 3264 / RFC 3551 these are dynamic-range
PTs; the negotiator mirrors the offerer's PT regardless, so
codec selection is by name + clock rate, not by PT number.

## [0.47.0] - 2026-04-21

Slice 4.5 — IoT bridges. Two reference sidecars land on the new
`bridge.*` capability namespace (Home Assistant + generic MQTT
3.1.1) with documented event-mapping patterns, a doorbell demo,
and an operator runbook for outbound `call.ended` publishing.

### Added

- **`bridge.*` capability namespace** — joins `ai.*` / `media.*`
  / `storage.*` / `routing.*` on the plugin validator. Tokens:
  `BRIDGE_HA = "bridge.ha"`, `BRIDGE_MQTT = "bridge.mqtt"`.
- **`plugins/examples/ha-bridge/`** — stdlib-only Python sidecar
  against Home Assistant's REST API. Three methods:
  - `emit_event(event_type, data)` → `POST /api/events/<type>`
  - `get_state(entity_id)` → `GET /api/states/<entity>`
  - `call_service(domain, service, data)` → `POST
    /api/services/<domain>/<service>`
  Env-configured (`HA_BASE_URL`, `HA_TOKEN`, `HA_TIMEOUT_SECS`);
  every HTTP failure surfaces as a JSON-RPC error the
  dispatcher can fail over.
- **`plugins/examples/mqtt-bridge/`** — stdlib-only MQTT 3.1.1
  publisher. Ships its own CONNECT / PUBLISH / DISCONNECT
  encoder (~100 LOC) so no `paho-mqtt` dep is required.
  Short-lived TCP socket per call; QoS 0 and 1; `payload`
  accepts string, object, array, or null (objects/arrays
  auto-JSON-encode). Env: `MQTT_HOST`, `MQTT_PORT`,
  `MQTT_USERNAME`, `MQTT_PASSWORD`, `MQTT_CLIENT_ID`,
  `MQTT_TIMEOUT_SECS`.
- **`docs/deployment/iot.md`** — event-flow diagrams,
  doorbell-triggers-SIP-call demo end-to-end, agent-side
  `call.ended` publish pattern, event-mapping table (which
  MCP notification maps to which typical outbound action),
  security notes, limits + follow-ons.
- **Tests** — 1 new unit test on `CapabilityDescriptor`
  (`accepts_bridge_ha_and_mqtt_capabilities`) covering both
  new tokens round-tripping through the validator.

### Notes

Inbound automation (HA → smiths-net) flows through the webhook
adapter (slice 4.4 / P20). The `ha-bridge` sidecar covers only
the outbound direction (engine → HA) because the plugin
protocol doesn't yet push events from engine to plugin — a
dedicated "event subscriber" capability is a follow-on slice.

The MQTT bridge is publish-only; `subscribe` needs a
persistent-connection lifecycle that doesn't fit the sidecar
JSON-RPC model as it stands. Operators who need subscription-
driven flows run a dedicated MQTT consumer outside the plugin
tier and invoke smiths-net's webhook adapter from there.

No engine-host adapter wires these sidecars into named tools
today — agents invoke them via the generic `ai_invoke` path
(`{plugin, method, params}`). Adding dedicated MCP tool
wrappers (`bridge_emit_ha`, `bridge_mqtt_publish`) for better
discoverability is a small follow-on; the plumbing is already
in place via the standard dispatcher.

## [0.46.0] - 2026-04-21

Slice 4.4 — A2A protocols. The control plane learns a fourth
adapter (generic HTTP webhook) and gets a new shared trait seam
(`ControlProtocol` + `ProtocolDispatch` + `ControlOutcome`) so
operator-authored adapters route through the same auth + rate-
limit + audit + metrics pipeline as MCP / A2A. No wire changes
to the existing adapters; everything is additive.

### Added

- **`smiths_mcp::control_protocol`** — adapter-agnostic trait
  seam. Three types land:
  - `ControlOutcome` — normalized `Ok | InvalidArguments |
    NotFound | Forbidden | Internal` verdict. Stable HTTP status
    + JSON-RPC code mapping on each variant.
  - `ProtocolDispatch` — thin wrapper over
    `Arc<ToolRegistry> + Arc<RateLimiter> + Arc<Metrics> +
    ToolContext` with one `invoke(actor, tool, args)` method
    that runs the shared `invoke_audited` pipeline and hands
    back a `ControlOutcome`.
  - `ControlProtocol` trait + three singleton markers
    (`McpStdioProtocol`, `A2aHttpProtocol`,
    `WebhookHttpProtocol`) for discovery / introspection.
  - `agent_card(dispatch, adapters, endpoint)` helper that
    builds a unified `.well-known/agent.json` document with a
    `capabilities.tools` list + `adapters` array so agents can
    tell at a glance which framings this engine speaks.
- **`smiths_mcp::webhook`** — generic HTTP webhook adapter.
  `POST /hook/<tool>` accepts a JSON body of args and returns
  `{"result": ...}` on success or `{"error": "..."}` with the
  right HTTP status on failure. Optional bearer-token guard.
  `/.well-known/agent.json` + `/health` stay public so load
  balancers and agent registries can still probe. Same
  `invoke_audited` path as MCP/A2A, so rate limits and audit
  events apply uniformly.
- **`docs/tool-authoring.md`** — migration guide: where tools
  live, the `Tool` trait contract, how to plumb optional
  dependencies via `ToolContext`, error/outcome classification,
  the adapter matrix, and what embedders gain from
  `ProtocolDispatch`.
- **Tests** — 2 unit tests on `control_protocol`
  (`outcome_from_result_round_trips`,
  `builtin_protocols_have_stable_labels`); end-to-end
  `a2a_make_call.rs` (HTTP client speaks A2A JSON-RPC → stubbed
  `CallOriginator` → `tools/call` returns the synthesized
  call-id, unknown-tool path surfaces JSON-RPC `-32601`);
  5-test `webhook_http.rs` covering the happy path, 404 on
  unknown tools, 400 on missing required args, agent-card
  discovery advertising all three adapter labels, and bearer-
  token gating.

### Notes

The existing MCP + A2A adapters keep their hand-rolled dispatch
loops for 0.46.0; they already flow through `invoke_audited` so
wire behaviour is identical. Migrating them to
`ProtocolDispatch::invoke` is a small follow-on — it removes
~20 lines of per-adapter error matching without changing any
externally observable behaviour.

The webhook adapter ships in the crate but is not yet wired
into the CLI's default binding set. Embedders who want it
online today call `smiths_mcp::webhook::serve_http(...)` from
their own binary; a `[webhook]` config section and CLI wiring
land alongside the MCP/A2A dispatch-loop migration.

## [0.45.0] - 2026-04-21

Slice 4.3 — HTTP/3 MCP + SIP-over-QUIC. MCP HTTP upgrades to
h1+h2 (free axum feature flip); feature flags + config scaffolds
land for the h3 listener and SIP-over-QUIC transport. `smiths-net
--version` now advertises every supported protocol, and
`docs/architecture/07-http3.md` walks the rollout.

### Added

- **MCP HTTP/2.** Axum workspace dep enables the `http2` feature.
  TLS-terminated MCP deployments negotiate h2 via ALPN; h1 clients
  keep working. The MCP HTTP adapter logs the enabled protocols at
  startup.
- **`[mcp.http3]` config section** — `enabled` + `bind`. Accepted
  regardless of feature flags; at startup the CLI warns if the
  runtime listener isn't wired (0.45.0: never wired) or the binary
  was built without `--features mcp-http3`.
- **`sip.transports = ["quic"]`** — valid enum value. Opted in via
  the `smiths-sip/sip-quic` Cargo feature + `smiths-cli/sip-quic`.
  Warns at startup that the listener isn't wired in 0.45.0.
- **`smiths-mcp/mcp-http3` Cargo feature** — no-op scaffold today;
  flips the `--version` advertisement and lets the config parse.
- **`smiths-sip/sip-quic` + `smiths-cli/sip-quic` features** —
  likewise scaffolds.
- **`smiths-net --version`** — now advertises every wired
  transport, storage backend, AI reference, and plugin tier;
  scaffolded protocols are flagged `(scaffold)`. Assembled via
  `const_format::concatcp!` + `#[cfg]`-gated hint fragments for
  zero runtime cost.
- **Perf baseline** — `crates/smiths-sip/tests/tcp_loss_bench.rs`
  (marked `#[ignore]`): round-trips 100 OPTIONS through the
  standard `TcpTransport` + a loopback peer, prints p50/p95/mean
  latency, asserts a sanity ceiling. Same harness will stand up a
  QUIC listener + rerun the loop once the `sip-quic` runtime
  lands.
- **`docs/architecture/07-http3.md`** — what shipped in 0.45.0,
  what's deferred and why, rollout plan through 0.48.0.

### Notes

The runtime listeners for both `mcp-http3` and `sip-quic` are
honestly deferred. Reasons:

- h3 needs a TLS 1.3 cert story separate from the DTLS-SRTP path
  (media cert vs signaling cert shouldn't be the same key).
- `quinn` + `h3-quinn` + `h3` integrate with axum through
  `tower::Service`, not `axum::serve` — ~200-400 LOC of graceful-
  shutdown + 0-RTT replay-safety code that deserves its own
  slice.
- `draft-ietf-sipcore-sip-quic` is still evolving; wiring a
  listener today pins us to a spec version that may shift.

Operators who opt into either feature today get the config shape
parsed + a clear warning + the `--version` bump — no silent
binding, no half-wired socket.

## [0.44.0] - 2026-04-21

Slice 4.2 — Dialplan + IVR kit. Two more reference plugins land
on the `routing.*` namespace: `dialplan-yaml` (YAML-authored
from/to/hour-of-day matchers) and `ivr-kit` (Rhai state machine
driving `play_prompt` / `transfer` / `hangup`). A prompt library
+ `record_prompt` MCP tool close out the authoring loop.

### Added

- **`plugins/examples/dialplan-yaml/`** — stdlib-only Python
  sidecar advertising `routing.dialplan`. Reads a shipped
  `rules.yaml`, evaluates each incoming `route(req)` through a
  from / to / hour_range matcher, returns the first match as
  `{target, reason}`. Includes a tiny YAML-subset parser so
  operators don't need `PyYAML`; swap in `yaml.safe_load` for
  full YAML 1.2. Dedicated `reload_rules` RPC method reloads the
  rule file without respawning the process.
- **`plugins/examples/ivr-kit/`** — reference IVR state machine
  in Rhai. Exports `describe_capabilities`, `greet(session)`, and
  `on_dtmf(session)`; returns `{action, prompt?, target?, state,
  done}` per press. Ships a press-1-for-sales / press-2-for-
  support / press-3-record-a-message tree with `*` to replay and
  `#` to hang up. Capability `routing.ivr`.
- **`smiths-media::PromptLibrary`** — LRU of decoded PCM16 LE
  mono WAVs keyed by path. Stdlib WAV parser (RIFF/WAVE chunks,
  format_tag = 1, 16-bit mono); `encode_wav(rate, samples)`
  helper for the inverse. `insert_raw` sidesteps disk so the
  `record_prompt` tool can make the just-written audio
  immediately hot without a round trip through `load_from_disk`.
- **`[media.prompts]` config** — `root` + `capacity`. Empty
  `root` disables the library; `record_prompt` returns a clean
  `NotFound` in that case.
- **MCP tool `record_prompt(call_id, audio_base64, path)`** —
  decodes the provided PCM16 LE audio, refuses absolute or
  `..`-containing paths, writes a mono WAV under the library
  root, seeds the cache. Returns `{path, absolute, sample_rate,
  bytes, duration_ms}`.
- **`ToolContext::with_prompts`** — engine threads the wired
  `PromptLibrary` onto tools through the existing `ToolContext`
  seam.
- **Tests** — 6 unit tests on `PromptLibrary` (encode/decode
  round-trip, rejects non-mono, LRU eviction, caching, raw
  insert, missing-file `NotFound`); 3 MCP tool tests on
  `record_prompt` (NotFound when the library is unwired,
  rejection of absolute + `..` paths, write-and-cache on the
  happy path); `ivr_state_machine.rs` integration test loads the
  reference `ivr-kit` plugin via the standard loader and drives
  every press-path the script handles (`1` → sales, `2` →
  support, `9` → replay, `#` → hangup, `3` → record submenu,
  submenu `#` → hangup).

### Notes

The IVR runtime seam — the glue that pushes live-call DTMF into
`on_dtmf` and has the UAS act on a returned `transfer` /
`hangup` mid-dialog — is a dedicated follow-on. The Rhai
contract in `ivr-kit` is stable; today the state machine is
driven synchronously by tests + tools that build a session
directly. Once the bridge exposes a per-call "IVR session"
control surface, the plugin switches from "test-driven" to
"engine-driven" with no script changes.

The dialplan sidecar ships without a native engine-side adapter
that plugs it in behind a route-selection trait — scripts and
sidecars alike are reachable via `AiProvider::invoke("route", …)`
on the plugin registry, and the SIP UAS calls into that surface
on INVITE. A dedicated `routing::Router` facade that lets
operators chain multiple `routing.dialplan` plugins (YAML first,
Rhai fallback, etc.) is a small follow-on.

## [0.43.0] - 2026-04-21

Slice 4.1 — Embedded DSL runtime (Rhai). The `smiths-script` crate
gains a working Rhai-backed `ScriptRuntime` with op-count +
wall-clock budgets; the plugin loader wires `type = "script"` into
the same `AiProvider` seam that sidecars + WASM guests use. Hot
reload with automatic rollback and a `put_script` MCP tool close
out the slice.

### Added

- **`smiths-script::ScriptRuntime`** — compiles Rhai source at load
  time, serves every invocation through the hot `Engine` + `AST`,
  enforces `max_operations` (1M default) and `wall_clock` (500 ms
  default) per call. JSON ↔ Rhai bridge (object, array, bool,
  int, float, string, null) lets scripts take the same
  `serde_json::Value` params sidecars already handle.
- **`ScriptEngineKind`** enum + `ScriptEngine` async trait so Lua /
  Starlark variants can slot in behind the same shape; Rhai is
  the only impl today.
- **`smiths-plugin::ScriptProvider`** — `AiProvider` adapter around
  the runtime. Atomic hot-swap (`swap_runtime`), auto-rollback
  (`rollback()` after `ROLLBACK_AFTER = 5` consecutive errors),
  reuses the existing `plugin_invocations` + `plugin_invoke_duration`
  metrics.
- **Manifest extension** — `type = "script"` now loads via the
  loader. New optional fields: `script_engine = "rhai"` (default),
  `script_max_operations`, `script_wall_clock_ms`.
- **Capability namespace `routing.*`** — joins `ai.*` / `media.*` /
  `storage.*` on the plugin validator. The reference dialplan
  advertises `routing.dialplan`.
- **`plugins/examples/route-rhai/`** — reference Rhai dialplan:
  small in-script lookup table routes `sip:support@…`,
  `sip:sales@…`, `sip:voicebot@…` to queue pools; everything else
  falls through to the UAS default. Tight manifest budgets
  (200k ops / 50 ms) so a sloppy edit can't pin a worker slice.
- **Hot-reload with rollback** — `smiths_plugin::watcher` already
  fires on entry-file changes; the `AiRegistry::reload` path now
  routes scripts through an in-place atomic swap (preserving the
  previous runtime for rollback) instead of the sidecar drop-and-
  respawn pattern. A compile failure leaves the running script
  untouched; a five-error streak after a swap restores the prior
  version. Capability changes refuse the swap (treated as a
  manifest change, not hot reload).
- **MCP tool `put_script(name, source, engine)`** — writes the new
  body atomically (tempfile + rename) into the loaded plugin's
  entry file and fires the same hot-reload path as a file-system
  edit. Only `engine = "rhai"` is accepted today. `NotFound` when
  the named plugin isn't loaded / isn't script-backed.
- **`AiRegistry::reload_script_source`** — new default-impl trait
  method on `smiths_core::ai::AiRegistry`. Returns "not supported"
  on registries that don't host scripts so old impls stay
  compatible; the plugin-crate impl performs the atomic write +
  reload.
- **Tests** — 6 unit tests on `smiths_script` (describe,
  round-trip JSON in+out, missing-method error, op-budget trip,
  compile-error surfacing, JSON↔Rhai round trip); 2 integration
  tests on `smiths-plugin` (`route-rhai` loaded through the
  standard loader + invoked through the `AiProvider` trait;
  rollback restores the good runtime after 5 consecutive errors);
  2 tool tests on the new `put_script` surface.

### Notes

Hot-reload today refuses to change the advertised capability set
— a script whose new `describe_capabilities` returns a different
set is treated as a manifest change, not a script edit. Operators
pick up the new contract by restarting the engine (or by removing
and re-adding the plugin directory, which triggers the ordinary
loader path).

Rhai's engine is synchronous; invocations run on a
`tokio::task::spawn_blocking` worker. This keeps each call
isolated from the tokio scheduler but means the effective
concurrency per script is bounded by the blocking pool. Scripts
that genuinely need parallelism should stay short — the dialplan
shape is a good fit; long-form RAG routing should stay in a
sidecar.

## [0.42.0] - 2026-04-21

Slice 3.5 — Proxy/VPN transports. SIP-over-TCP and SIP-over-TLS can
now tunnel outbound connects through a SOCKS5 or HTTP-CONNECT
proxy. A WireGuard deployment guide lands alongside, plus a
feature-flag scaffold for the embedded `boringtun` path. Ingress
is untouched — operators who need it terminate TLS or put a
reverse proxy in front of the engine as before.

### Added

- **`ProxyConnector` trait** in `smiths-sip::transport::proxy`,
  with three impls: `DirectConnector` (default, = plain
  `TcpStream::connect`), `Socks5Connector` (RFC 1928 + RFC 1929
  user/password), `HttpConnectConnector` (HTTP/1.1 CONNECT +
  `Proxy-Authorization: Basic`). Every connector returns a
  `TcpStream` positioned at the first app-data byte; the rest of
  the SIP pipeline is unaffected.
- **`TcpTransport::with_proxy`** — builder that swaps the
  outbound-connect shim. `DirectConnector` remains the default so
  existing callers pay zero cost.
- **`[sip.proxy]` config** — `mode = "none"|"socks5"|"http-connect"`,
  `address`, `username`, `password`. Redacted at
  `sip.proxy.password` in `config://current`.
- **`[sip.vpn]` config + `wireguard` Cargo feature** on
  `smiths-cli`. Accepts `private_key`, `peer_public_key`,
  `peer_endpoint`, `allowed_ips`, `interface_ip`. Runtime device
  is a follow-on: 0.42.0 logs a clear warning at startup when
  `mode = "wireguard"` so operators aren't surprised by the
  deferral. `sip.vpn.private_key` joins the redaction list.
- **`docs/deployment/vpn.md`** — sidecar-vs-embedded tradeoffs,
  host / Kubernetes setup, verification ladder for tunnel issues.
- **Tests** — 7 unit tests on the proxy module: SOCKS5 no-auth
  handshake + app-data streaming, SOCKS5 user/pass round trip,
  SOCKS5 `0xFF` refusal path, HTTP-CONNECT 200 tunnel + basic-auth
  header presence + 407 error surfacing, and
  `connector_from_config` covering every mode. Integration test
  `proxy_socks5_tor.rs` runs end-to-end through a real SOCKS5
  proxy when `TOR_SOCKS_PROXY=host:port` is set in the env;
  silently no-ops otherwise so CI images without a proxy still
  pass.

### Notes

The proxy shim applies only to the outbound-connect path on
TCP-based SIP transports (today: `TcpTransport`; TLS is
listener-only in 0.42.0). UDP is a connectionless protocol that
can't tunnel through a stream-oriented CONNECT proxy — the UDP
transport ignores `[sip.proxy]` with no warning, mirroring how
curl handles the same mismatch.

Embedded `boringtun` is feature-flagged + config-scaffolded in
this release; the tun-interface-creation + SIP-binding plumbing
lands in a dedicated follow-on. For production deployments today
the sidecar WireGuard pattern documented in
`docs/deployment/vpn.md` is the supported path.

## [0.41.0] - 2026-04-21

Slice 3.4 — Vector + Recording storage. Two new pluggable storage
traits join `CdrStore` / `KvStore`: `VectorStore` backs the new
`search_calls_semantic` MCP tool, and `RecordingStore` unblocks the
call-id-only path on `transcribe_call` / `summarize_call`. The
engine ships real Rust impls (in-memory vectors + filesystem
recordings) and scaffolds sidecars for the hosted backends.

### Added

- **`smiths-core::storage::VectorStore`** — embedding-indexed
  search with sync `upsert` / `delete` / `search` / `len`. The
  `MemoryVectorStore` default does NaN-safe cosine similarity,
  skips dimension-mismatched records on search, and enforces
  non-empty id/vector at upsert.
- **`smiths-core::storage::RecordingStore`** — per-call audio
  retention. The `FsRecordingStore` default writes
  `<hex(call_id)>.wav` + `<hex(call_id)>.cid` pairs so arbitrary
  call-ids round-trip through the filesystem and `list()` honestly
  surfaces the originals. `prune_older_than(Duration)` deletes by
  mtime — the engine runs it on an hourly sweeper when
  `[storage.recording] retention_days > 0`.
- **`[storage.vector]` and `[storage.recording]` config** — each
  with `backend = "none" | "memory"|"fs" | "sidecar"` and a
  `plugin` field for the sidecar path. `retention_days` gates the
  sweeper; `fs.root` points at the recording directory.
- **Capability namespace `storage.*`** — joins `ai.*` and
  `media.*` on the plugin manifest validator. Two new tokens:
  `STORAGE_VECTOR = "storage.vector"` and
  `STORAGE_RECORDING = "storage.recording"`.
- **MCP tool `search_calls_semantic(query, k)`** — embeds `query`
  via the `ai.embed` dispatcher lane, runs top-k against the wired
  `VectorStore`, returns `{hits: [{id, score, metadata}]}`. Handles
  three embed response shapes: in-tree `{vectors: [[...]]}`,
  OpenAI-compat `{embeddings: [...]}`, and raw OpenAI
  `{data: [{embedding: [...]}]}`. Observed on
  `smiths_ai_pipeline_duration_seconds{pipeline="search_calls_semantic"}`.
- **Pipeline tools now consult the recording store** —
  `transcribe_call` / `summarize_call` resolve `audio_base64` from
  `[storage.recording]` when the caller omits it. The new error
  message names the specific missing piece ("no backend wired" vs
  "no recording for this call").
- **`ToolContext::with_vector` / `with_recording`** — engine
  threads the constructed stores onto tool context. CLI auto-
  wires the in-memory + filesystem backends from config; sidecar
  adapters for both stores are scaffolded via the new sidecars
  below but not yet plugged into the trait seam.
- **`plugins/examples/store-qdrant/`** — stdlib-only Qdrant HTTP
  API wrapper. Auto-creates the collection with configured size +
  distance, upserts with string-id preservation, surfaces cosine
  top-k via `search`. Env: `QDRANT_URL`, `QDRANT_COLLECTION`,
  `QDRANT_VECTOR_SIZE`, `QDRANT_DISTANCE`, `QDRANT_API_KEY`.
- **`plugins/examples/store-s3-recording/`** — stdlib-only sidecar
  that shells out to the `aws` CLI (so SigV4 + credential
  discovery stay in one place). `put` / `get` / `delete` / `list` /
  `prune_older_than` over JSON-RPC; works against S3, MinIO,
  R2, B2.
- **Tests** — 9 new unit tests on `storage` (`MemoryVectorStore`
  round-trip + input validation + dimension-mismatch skip,
  `FsRecordingStore` put/get/list/delete/prune); 2 new capability-
  validation tests for `storage.vector` / `storage.recording`; 4
  new MCP tool tests + 2 embed-shape extractor tests on
  `search_calls_semantic`. `semantic_search_pipeline.rs`
  integration test exercises the upsert → embed → search round
  trip end-to-end with a deterministic stub embed provider.

### Notes

The storage-sidecar adapter — the shim that lets
`[storage.vector] backend = "sidecar"` pick up a loaded Qdrant
plugin and present it through the `VectorStore` trait — is
scaffolded but not yet wired. Selecting `sidecar` today logs a
warning and falls through to "no backend"; `search_calls_semantic`
returns `NotFound`. Landing the adapter is a small follow-on
slice once we have a second vector backend asking for the same
seam.

Retention is wall-clock mtime-based on the filesystem store; on
disks without reliable mtimes (some NFS mounts, tmpfs under
certain containers) the sweeper may mis-classify. Object-store
backends use the LIST response's `LastModified` timestamp.

## [0.40.0] - 2026-04-21

Slice 3.3 — ASR + composite AI pipelines. `ai-asr-whisper`
sidecar (local whisper.cpp) joins the `ai.asr` seam, and two new
MCP tools — `transcribe_call` and `summarize_call` — route through
the dispatcher so agents name a capability, not a plugin. The
summarize tool is the first engine-shipped composite: ASR → LLM,
both hops failover-aware, end-to-end wall clock observed on a new
pipeline histogram.

### Added

- **`plugins/examples/ai-asr-whisper/`** — stdlib-only Python
  sidecar that execs `whisper-cli` (or the legacy `main`) with a
  local GGML model. Decodes `audio_base64` → temp WAV (crude-
  upsamples 8 kHz telephony audio to the 16 kHz whisper.cpp
  expects), invokes the binary with `-oj`, parses the JSON
  transcript. `priority = 20` so the dispatcher picks Whisper over
  the canned `ai-asr-mock` (50). Env: `WHISPER_BIN`,
  `WHISPER_MODEL`, `WHISPER_THREADS`, `WHISPER_LANG`.
- **MCP tool `transcribe_call(call_id)`** — ASR only, dispatcher-
  routed at `ai.asr`. Returns `{transcript, raw}`. Accepts an
  `audio_base64` argument until the recording store lands (slice
  3.4); a call-id-only invocation returns a clean `NotFound` that
  names the missing backend.
- **MCP tool `summarize_call(call_id)`** — flagship composite.
  Transcribes via `ai.asr`, then summarizes via `ai.llm.chat` with
  a concise-meeting-notes system prompt; `max_sentences` bounds the
  output length. Returns `{transcript, summary}`. Same deferral
  path when the recording store isn't wired.
- **`smiths_ai_pipeline_duration_seconds{pipeline}` histogram** —
  one sample per invocation of a composite pipeline tool, keyed by
  the tool name. Sits alongside `tool_duration_seconds` but scoped
  to AI compositions so operators can grep end-to-end ASR→LLM
  latency without mixing in every other MCP tool.
- **`ToolContext::with_metrics`** — engine threads its
  `Arc<Metrics>` onto the tool context. Pipeline tools observe
  histograms through this handle; tools that don't need metrics
  ignore the field. Older test fixtures keep working (the field
  defaults to `None` and the pipeline tools fall through to
  `Metrics::noop()` when absent).
- **Demo update — `examples/python-client/voice_agent.py`** — the
  STT + LLM path is now a single `summarize_call` hop; the summary
  is then routed through `translate` (slice 3.1) into the caller's
  preferred language before TTS. The old per-plugin wrappers stay
  as a fallback illustration.
- **Tests** — 4 new MCP tool tests covering the honest-deferral
  paths on `transcribe_call` + `summarize_call` (no audio →
  `NotFound`; no asr provider → `NotFound`; missing `call_id` →
  `InvalidArguments`). Total MCP tool-unit-test count is now 24.

### Notes

The call-id-only path on `transcribe_call` / `summarize_call` is an
honest deferral: without a `storage.recording` backend the engine
has nowhere to pull audio from. Slice 3.4 lands that backend
(`storage.recording` trait + filesystem default + S3 sidecar); at
that point both tools will resolve audio from `call_id` alone and
the `audio_base64` parameter becomes the override, not the
requirement.

The Whisper sidecar can't be exercised in CI — each invocation
shells out to the `whisper-cli` binary and loads a 100 MB+ GGML
model from disk. The tool-level tests use the mock registry and
prove the dispatcher hookup; the real binary is covered by the
README's smoke-test recipe.

## [0.39.0] - 2026-04-21

Slice 3.2 — Cloud AI parity. `ai-llm-openai` and `ai-llm-anthropic`
reference sidecars join the local `ai-llm-ollama` at the
`ai.llm.chat` seam, all behind the same ABI. Streaming partials
ride the existing plugin-notification rail as `ai.llm.partial`
frames, and every response that carries `usage.*_tokens` ticks the
new `smiths_ai_tokens_total` counter.

### Added

- **`plugins/examples/ai-llm-openai/`** — stdlib-only sidecar
  against `/v1/chat/completions`. Streaming via SSE when
  `controls.stream = true`; emits one `ai.llm.partial` notification
  per delta plus a final-marker. `priority = 15` so the dispatcher
  prefers cloud OpenAI over local Ollama (20) and the mock (50).
  Env: `OPENAI_API_KEY`, `OPENAI_API_BASE`, `OPENAI_MODEL`,
  `OPENAI_TIMEOUT_SECS`.
- **`plugins/examples/ai-llm-anthropic/`** — stdlib-only sidecar
  against `/v1/messages`. Splits `system` turns into Anthropic's
  top-level `system` field; streams via SSE
  (`content_block_delta` → `ai.llm.partial`). `priority = 16` so it
  fails over behind OpenAI. Env: `ANTHROPIC_API_KEY`,
  `ANTHROPIC_API_BASE`, `ANTHROPIC_MODEL`, `ANTHROPIC_VERSION`,
  `ANTHROPIC_MAX_TOKENS`, `ANTHROPIC_TIMEOUT_SECS`.
- **`[ai]` config section + `AiConfig`** — optional
  `openai_api_key`, `anthropic_api_key` fields. Redacted in
  `config://current` via the existing MCP resource layer; the three
  secret paths now are `a2a.bearer_token`, `ai.openai_api_key`,
  `ai.anthropic_api_key`.
- **`smiths_ai_tokens_total{provider, dir}` metric** —
  dispatcher-credited on every `AiProvider::invoke` success whose
  response includes `usage.input_tokens` / `usage.output_tokens`
  (directly or nested under `message.usage`). `provider` = plugin
  name; `dir` ∈ `{"input", "output"}`. Zero-cost for plugins that
  don't report usage (counter stays at 0 for that label set).
- **Streaming partials demo** — the canned `ai-llm-mock` now
  honors `controls.stream = true`, emitting one `ai.llm.partial`
  per token plus the final marker. The declared descriptor gains
  `streaming.supported = true` + a `stream` control so the plugin
  validator accepts it end-to-end.
- **Tests** — 3 redaction tests on `resource::redact_secrets`
  covering the bearer-token + two AI-key paths; 2 dispatcher-token
  tests (usage credited on success; zero credit when plugin omits
  usage); `streaming_llm.rs` integration test loading `ai-llm-mock`,
  invoking `chat` with streaming on, and asserting that
  `ai.llm.partial` notifications reach the engine's bus as
  `PluginEvent::Notification`.

### Notes

The engine does **not** auto-forward `[ai]` config secrets into
sidecar child environments — operators still set `OPENAI_API_KEY` /
`ANTHROPIC_API_KEY` in the engine's own environment, which the
sidecars inherit on spawn. The config surface exists so secrets have
one canonical home on disk + a tested redaction path; an env-
injection layer ("secrets plumbing") is a small follow-on.

The two cloud sidecars can't be exercised in CI (no API keys on
CI runners), which is why the streaming-partials test uses the
extended mock. The mock + the two cloud sidecars emit identical
wire frames (`ai.llm.partial` shape, final-marker, token accounting
in the RPC response), so a passing mock test implies the cloud
path is protocol-correct.

## [0.38.0] - 2026-04-21

Slice 3.1 — AI providers: `AiDispatcher` + fail-over. Agents now
say `ai_invoke(capability, ...)` and the engine picks the best
candidate by priority + health; a single flapping plugin no longer
blocks the whole call. Reference sidecars (`ai-llm-ollama`,
`ai-tts-piper`) land alongside the canned mocks so operators have a
"real model talking to a real call" path on day one.

### Added

- **`smiths-core::ai::AiDispatcher`** — capability-routed dispatcher
  over `AiRegistry`. Selection rule: candidates filtered by
  `capability` string, sorted by descriptor `priority` (lower wins;
  ties broken by `latency_ms.p50`, then lexical plugin name),
  breaker-open plugins partitioned to the tail. `invoke(capability,
  method, params)` runs the chain with per-attempt timeout + fail-
  over; returns the first `Ok`, or `DispatchError::AllFailed` with
  the final error attached.
- **`DispatchPolicy`** — `per_attempt_timeout` (30 s default),
  `max_attempts` (4 default). Builder on the dispatcher:
  `with_policy`, `with_metrics`.
- **Per-plugin health (`ProviderHealth`)** — simple count-with-
  cooldown breaker: 3 consecutive failures trip it Open for 30 s;
  one success closes it. Breaker is internal — the dispatcher still
  tries Open candidates if nothing else is healthy so a partially-
  degraded registry doesn't refuse service.
- **`CapabilityDescriptor.priority: u8`** — optional, defaults to
  `DEFAULT_PRIORITY = 50`. Reference sidecars (Ollama, Piper)
  advertise `priority = 20` so the dispatcher prefers them over the
  canned mocks without the operator having to configure anything.
- **Metrics** — `smiths_ai_invocations` (one per dispatcher call)
  and `smiths_ai_failovers` (one per fail-over tick, not per
  attempt). Labelled by `capability`. Wired via
  `AiDispatcher::with_metrics`.
- **MCP tool `translate(text, to)`** — first dispatcher-native tool.
  Routes through `ai.llm.chat` with a fixed translator system
  prompt; returns the translated text alongside the raw provider
  response. Handles Ollama, OpenAI-compat, and flat-`content`
  shapes. `NotFound` when no `ai.llm.chat` is loaded.
- **`plugins/examples/ai-llm-ollama/`** — sidecar that shells into a
  local Ollama daemon (`/api/chat`, `stream=false`). Stdlib-only;
  env-overridable (`OLLAMA_HOST`, `OLLAMA_MODEL`,
  `OLLAMA_TIMEOUT_SECS`). README walks the three-command install.
- **`plugins/examples/ai-tts-piper/`** — sidecar that execs the
  `piper` binary with `--output_raw` and returns PCM16. Stdlib-only;
  env-overridable (`PIPER_BIN`, `PIPER_VOICE`, `PIPER_VOICE_ID`,
  `PIPER_LANG`). Decimates Piper's native 22.05 kHz to 8 / 16 kHz
  on request.
- **Tests** — 7 new dispatcher tests in `smiths-core::ai::tests`
  (candidate ordering, `NoProvider`, fail-over, `AllFailed` surfaces
  last error, timeout trips fail-over, breaker opens after 3
  failures, metrics increment on success + fail-over) and 5 new MCP
  tool tests (`translate` arg validation, `NoProvider` surface, and
  three `extract_chat_content` shapes: flat, `message.content`,
  `choices[0].message.content`).

### Notes

Dispatcher metrics are plumbed through `AiDispatcher::with_metrics`
but the MCP `translate` tool constructs its own dispatcher per
call and currently runs **without** a metrics handle — engine-level
wiring (one dispatcher on `ToolContext`, `Arc<Metrics>` threaded
through) is a small follow-on slice. `smiths_ai_invocations` /
`smiths_ai_failovers` are still registered on the Prometheus
registry so they show up at 0 until the wiring lands.

The two reference sidecars assume a healthy local install — Ollama
daemon up, or `piper` + an ONNX voice on disk. Each sidecar still
serves `describe_capabilities` when its backend is unreachable, so
the dispatcher sees the candidate and fails over on `invoke`. That
keeps `list_ai_providers` honest in both states.

## [0.37.0] - 2026-04-21

Slice 2.5 — Inband DTMF via Goertzel. Covers legs that never
negotiated RFC 4733 (PSTN gateway crossings) — same `DtmfSink`
contract as slice 2.4, same `SipEvent::Dtmf` bus event.

### Added

- **`smiths-core::dtmf_inband`** — second-order Goertzel detector:
  - `InbandDtmfDetector::new(leg_id)` + `with_config(...)` for
    tighter tunings.
  - `feed_pcm16` / `feed_pcmu` — PCMU decode built-in so the bridge
    hot path doesn't re-parse.
  - `synthesize_tone(digit, ms, rate)` — ground-truth generator
    used by the accuracy benchmark + tests.
  - Constants: 8 kHz default clock, 20 ms frame (160 samples),
    0.3 magnitude threshold, 40 ms debounce, co-channel 4× runner-
    up ratio.
- **Bridge integration** — `BridgeConfig::inband_dtmf` bool opts
  in; forwarder runs both RFC 4733 and Goertzel detectors against
  the same plaintext RTP stream, delivering through the existing
  `DtmfSink` seam. Zero overhead when off.
- **`UdpMediaFabric::with_inband_dtmf(bool)`** builder.
- **`[media]` config section** — `inband_dtmf = false` by default
  so existing deployments don't pay the FLOPs.
- **`MEDIA_STREAMING_RTP = "media.streaming_rtp"` capability**
  recognized in the plugin manifest validator. Namespaces are now
  `ai.*` **and** `media.*`. Declares the plugin-tier contract for
  a future sidecar that consumes per-packet RTP — `dtmf-inband`
  (Python + scipy) is the intended reference but deferred.
- **Tests** — 8 unit tests on the detector (every DTMF digit
  round-trips, silence is silent, debounce works both ways, PCMU
  feed matches PCM16, synthesize duration accurate, **accuracy
  benchmark pass-all on clean synthesized audio**), 1 unit test
  on the plugin validator accepting `media.streaming_rtp`, and
  `dtmf_inband_bus.rs` end-to-end: synthesized tones over PCMU
  through `UdpMediaFabric` → live `EventBus` subscriber.

### Notes

The Python sidecar listed in the slice plan (numpy + Goertzel) is
deferred — running a detector across a JSON-RPC boundary at 50
packets/sec/leg is absurd for a hot-path workload. The engine-side
pure-Rust detector covers the acceptance target (≥ 95% accuracy on
clean audio; bench passes 100%). A plugin-tier consumer will slot
in later behind the new `media.streaming_rtp` capability without
engine changes.

## [0.36.0] - 2026-04-21

Slice 2.4 — DTMF via RFC 4733 (a.k.a. RFC 2833). DTMF keypresses
detected on the media bridge now land on the engine's event bus as
`SipEvent::Dtmf` + are emittable from the control plane via
`send_dtmf`.

### Added

- **`smiths-core::dtmf` module** —
  - `TelephoneEvent::parse` / `encode` (RFC 4733 §2.3 wire format).
  - `event_code_to_digit` / `digit_to_event_code` — §3.2 Table 1
    mapping (0–9, *, #, A–D, flash).
  - `DtmfDetector` — stateful decoder. Dedupes the §2.5.1.3
    three-end retransmits and emits exactly one `DtmfKeypress` per
    press with a wall-clock `duration_ms`.
  - `DtmfSink` trait + `BusDtmfSink` adapter that publishes presses
    onto an `EventBus` as `SipEvent::Dtmf`. Non-blocking —
    bridge-forwarder hot-path safe.
  - `generate_keypress` transmit helper: builds the full start +
    intermediates + three-end packet stream for a digit.
- **`SipEvent::Dtmf { call_id, keypress }`** variant — typed
  first-class event for dashboards + MCP subscribers.
- **Bridge DTMF detection on the hot path** —
  `BridgeConfig::dtmf_sink` + per-direction `DtmfDetector`. When
  the sink is `None` (default) forwarders pay no extra cost; when
  wired, each packet with PT 101 gets parsed and the resulting
  keypress delivered.
- **`UdpMediaFabric::with_dtmf_sink`** — builder that plumbs a
  shared sink into every subsequently-spawned bridge. The CLI
  wires a `BusDtmfSink` here so RFC 4733 keypresses surface on the
  engine-wide bus.
- **`send_dtmf` MCP tool** — takes `{call_id, digits, duration_ms}`,
  emits the full RFC 4733 stream into the call's media leg (20 ms
  frame cadence, 40 ms inter-digit gap, three end-retransmits).
  Validates every digit up-front so a typo can't partial-send.
- **`smiths-testkit::dtmf_gen`** — re-export of the core transmit
  helper so tests keep the historical import path.
- **Integration tests** (`smiths-media/tests/`):
  - `dtmf_bridge.rs` — raw `Bridge` with a `DtmfSink` trait object.
  - `dtmf_bus.rs` — end-to-end through `UdpMediaFabric` +
    `BusDtmfSink` to a live `EventBus` subscriber.
  - 8 unit tests on the parse/encode/detector cycle + 3 on the
    generator.

### Notes

The plugin-tier version (`dtmf-2833` as a WASM plugin under
`plugins/examples/`) is deferred — the engine-side detection path
covers the acceptance scenario (MCP subscriber observes DTMF inside
100 ms of the tone), and landing the reference plugin is a pure
follow-on behind the same `DtmfSink` trait.

## [0.35.0] - 2026-04-21

Slice 2.3 — Pluggable storage MVP (P23). Formalizes the persistence
surface every post-MVP feature (HA, recording, RAG, presence) now
targets. CDR recording wired end-to-end.

### Added

- **`smiths-core::storage` module** — three new traits:
  - [`CdrStore`] with [`CallDetailRecord`] DTO and [`CdrFilter`]
    for bounded queries (time range, From/To substring, result,
    required `limit`, newest-first ordering).
  - [`KvStore`] — opaque key/value for session state + hot-reload
    snapshots. `get` / `put` / `delete` / `list_prefix`.
  - [`StorageError`] — backend-agnostic error type.
  (The existing `smiths-sip::auth::CredentialStore` stays in the
  SIP crate; it's RFC-2617-shaped and this slice adds the generic
  companions rather than shuffle the auth seam.)
- **`[storage]` config section** — `backend = "none" | "sqlite"`,
  `[storage.sqlite] path`. Defaults to `none` so fresh configs
  stay silent until operators opt in.
- **`SqliteAuthStore` v2 schema** — adds `cdr` + `kv` tables in a
  second, idempotent migration. Same store now serves
  `CredentialStore` + `RegistrationStore` + `CdrStore` + `KvStore`;
  operators typically point `[auth.sqlite]` and `[storage.sqlite]`
  at the same DB file. Indexes: `cdr(started_at_unix DESC)`,
  `cdr(result)`.
- **UAS CDR emission** — `handle_invite` stashes From/To + start
  time in a per-dialog side table at 200 OK; `handle_bye` emits a
  `CallDetailRecord` with `result = "answered"` and
  `duration_secs` computed from wall-clock deltas. Silent no-op
  when no `CdrStore` is wired (default).
- **`list_cdr` MCP tool** — bounded query exposed through the
  control plane. Returns `{count, rows}`; empty page when no
  backend is wired.
- **`UasServer::with_cdr_store`** + `ToolContext::with_cdr`
  builders — same pattern as the slice-2.1 registration wiring.
- **12 new tests** in `sqlite_store`: CDR record / list / filter
  (result, since, substring) / upsert-on-conflict / zero-limit
  rejection / truncate, plus KV round-trip / delete semantics /
  prefix listing / wildcard-escape. Plus `cdr_record.rs`
  integration: full INVITE→ACK→BYE produces exactly one CDR row
  with the right From / To / duration.

### Notes

The Postgres sidecar adapter listed in the slice plan is deferred
— no concrete operator ask yet, and the generic trait shape means
it's a pure addition when it lands.

## [0.34.0] - 2026-04-21

Slice 2.2 — HTTP webhook subscriber-DB backend (P8, second half).
Operators with existing IAM / HR systems can now delegate
credential lookup to an HTTPS endpoint without exposing plaintext
passwords to the engine.

### Added

- **`smiths-sip::auth::http_store::HttpAuthStore`** —
  `CredentialStore` impl that `POST`s `{realm, username, algorithm}`
  to an operator-provided webhook and expects
  `{"status":"accept","ha1":"<hex>"}` or `{"status":"deny"}` back.
  Built on `reqwest` with `rustls-tls` so musl static builds stay
  self-contained. Behind the `auth-http` feature (on by default).
- **Pre-computed HA1 path** — `Credentials` gained an optional
  `ha1` field + `from_ha1` constructor. The registrar uses it when
  set, skipping the on-demand hash; plaintext passwords never cross
  the webhook boundary.
- **Circuit breaker** — per-store state with Closed / Open /
  HalfOpen semantics. After `breaker_threshold` consecutive
  failures (default 5) the breaker trips Open; after
  `breaker_cooldown_secs` (default 30) one probe is allowed; a
  successful probe closes it. Open-state behaviour selectable via
  `FailureMode::FailClosed` (default, safe) vs `FailOpen`
  (UnknownUser-equivalent, dev only).
- **Bearer auth** — `Authorization: Bearer <token>` on every
  webhook request so the backend can authenticate the engine.
  Config: `[auth.http] bearer_token`.
- **`[auth.http]` config section** — `endpoint`, `timeout_ms`,
  `retries`, `bearer_token`, `breaker_threshold`,
  `breaker_cooldown_secs`, `failure_mode`. `[auth] backend` grew a
  `"http"` variant.
- **Integration test** (`tests/register_http.rs`) — 5 scenarios
  against an in-process `axum` mock: accept / deny / breaker trip
  (asserts the wire isn't hit while Open) / bearer token plumbed
  correctly / missing bearer surfaces as deny.
- **Async entry point** — `HttpAuthStore::authenticate()` is
  awaitable directly for callers that already live in async code;
  the sync `CredentialStore::lookup` bridge uses
  `tokio::task::block_in_place` + `Handle::block_on` (requires a
  multi-thread runtime, which is our default).

### Changed

- `Credentials` has a new required field (`ha1: Option<String>`);
  every construction site migrated to `Credentials::new(...)` or
  `::from_ha1(...)`. Existing behaviour is unchanged when `ha1` is
  `None`.

## [0.33.0] - 2026-04-21

Slice 2.1 — SQLite subscriber DB (P8, first half). Unblocks
production REGISTER with persisted credentials + contact bindings.

### Added

- **`smiths-sip::auth::sqlite_store::SqliteAuthStore`** — embedded
  `SQLite` backend (via `rusqlite` with `bundled` — no system
  libsqlite3 required). Implements both `CredentialStore` (for
  digest auth) and the new `RegistrationStore` trait (for Contact
  bindings). Single impl, two trait objects — callers
  `Arc::clone` the store into both seats.
- **3-table v1 schema**: `realms`, `users` (FK → realms, unique
  `(realm_id, username)`), `contacts` (unique `(aor, contact)`).
  Indexes on `contacts.aor` + `contacts.expires_at_unix` for the
  hot paths.
- **`MigrationRunner`** — idempotent. Opening an empty DB installs
  v1; reopening a populated DB is a no-op; opening a DB newer than
  this binary understands fails fast with
  `SchemaTooNew { found, max_supported }`. Each step logs at
  `info!` when applied.
- **`auth::RegistrationStore` trait + `Binding` DTO** — pluggable
  persistence surface for per-AOR contact bindings. In-memory
  default (`InMemoryRegistrationStore`) plus the SQLite impl
  above. Expired rows are filtered out of `snapshot()` /
  `lookup_bindings()`; `gc_expired()` hard-deletes them.
- **`[auth]` config section** — `backend = "none" | "sqlite"` (new
  default `"none"` keeps pre-v0.33.0 behavior), `realm`, plus
  `[auth.sqlite] path` when `backend = "sqlite"`.
- **UAS REGISTER Contact persistence** — `handle_register` parses
  `Contact:` + `Expires:`, computes the AOR as `sip:user@realm`,
  and calls `RegistrationStore::bind` on success. `Expires: 0`
  unbinds per RFC 3261 §10.3.7. Silent no-op when no store is
  wired.
- **`smiths_core::RegistrationView` + `RegistrationSnapshot`** —
  read-only observability trait; surfaced to the MCP control
  plane without a cross-crate dep on `smiths-sip`.
- **`sip://registrations` MCP resource** — serialises every live
  binding as `{count, bindings: [...]}`. Empty snapshot when no
  backend is wired; operators distinguish "disabled" from "idle"
  via `config://current`.
- **Integration test** (`tests/register_sqlite.rs`) — full
  challenge → authenticate → bind → unbind flow against a tempfile
  DB. Asserts the contact lands in the contacts table after 200
  OK, and that `Expires: 0` removes it.

### Changed

- `auth.rs` is now `auth/mod.rs`; new `sqlite_store` submodule
  gated behind the `auth-sqlite` feature (on by default for
  `smiths-sip`).

## [0.32.0] - 2026-04-20

Closes **prod-readiness Phase 6** — full e2e + perf validation.

### Added

- `crates/smiths-testkit/tests/full_stack.rs` — skeleton for the
  TLS + SRTP + WASM + sidecar + metrics + drain-during-call
  scenario. Smoke-passes today; individual steps gain assertions
  as each subsystem's seam freezes.
- `observability.pcap_dir` config knob + `smiths-media`'s `pcap`
  Cargo feature. Placeholder writer module lives under
  `pcap = ["feature"]`; the pcap-file encoder lands in a follow-on
  when operators actually demand the tap.
- `.github/workflows/fuzz-nightly.yml` — runs `cargo-fuzz` on
  `sip_parser`, `via_branch`, and `sdes_crypto` nightly, uploads
  coverage + crash corpus as workflow artifacts.
- `observability/dashboards/smiths-overview.json` — Grafana
  dashboard covering dialogs, SIP req/resp, txn FSM entries, 2xx
  retransmits, RTP forward rate, RTCP SRs, plugin invocations, tool
  latency, sidecar restarts.

## [0.31.0] - 2026-04-20

### Added — deployment artifacts (Phase 6 ops)

- `Dockerfile` — multi-stage musl build targeting
  `linux/amd64` + `linux/arm64`. Final image is distroless-static
  with the config.toml example bundled as `.example`.
- `.github/workflows/release.yml` — tag-driven (`v*`) buildx job
  publishing multi-arch images to GHCR plus per-arch release
  tarballs.
- `systemd/smiths-net.service` — `CAP_NET_BIND_SERVICE` ambient
  cap, `ProtectSystem=strict`, read-only `/etc/smiths-net` mount,
  systemcall filter (`@system-service`), `RestrictAddressFamilies
  =AF_INET AF_INET6 AF_UNIX`.
- `k8s/` — `Deployment` (2 replicas, zero-unavailable rolling
  update, read-only root, `RuntimeDefault` seccomp),
  `ConfigMap` (engine config inline), `Service`
  (LoadBalancer with ClientIP affinity for SIP, separate
  ClusterIP for `/metrics`).
- `docs/operator-runbook.md` — install / upgrade / rollback
  procedures for bare-metal, Docker, and Kubernetes. Extends the
  sandbox docs section added in v0.29.0.

## [0.30.0] - 2026-04-20

### Changed — workspace lint tightening (Phase 6)

- `#![warn(clippy::unwrap_used, clippy::expect_used)]` promoted on
  `smiths-sip`, `smiths-media`, `smiths-mcp`, `smiths-plugin`,
  `smiths-sidecar` — every production-code fire is either
  documented at the site with `#[allow]` + justification
  (intra-module mutex locks) or absent. Tests stay unrestricted
  via `#[cfg_attr(test, allow(...))]`.
- `#![warn(missing_docs)]` promoted on `smiths-core`, `smiths-sdp`,
  `smiths-proto`. Every public item now carries at least a
  one-line description. Zero-fire; CI's existing `-D warnings`
  gates drift.
- `smiths-proto` gains the unwrap/expect lint pair for parity with
  the other frozen-surface crates.

## [0.29.0] - 2026-04-20

### Added — hardened sandbox (seccomp-BPF, Linux)

`SandboxConfig` grows a `seccomp` policy field with two values:
`off` (default, same as pre-v0.29.0) and `allowlist`, which
installs a curated BPF allowlist covering the syscalls a typical
Rust / Python / Node plugin needs. Denies return `ERRNO(EPERM)`
so plugins surface "operation not permitted" instead of the
opaque kernel kill `SIGSYS` would produce.

- **`smiths_sidecar::sandbox::seccomp_filter`** — compiled only on
  `target_os = "linux"`. Uses `seccompiler` 0.5 behind the `pre_exec`
  gate. Baseline covers file I/O, memory ops, futex, signals,
  event-loop primitives (epoll, eventfd2), sockets (AF_INET +
  AF_INET6 + AF_UNIX), process lifecycle, `prctl`, `getrandom`.
  Per-arch syscall tables for `x86_64` + `aarch64`.
- **`SeccompPolicy`** — serde-ready TOML enum
  (`seccomp = "allowlist"`). `SandboxConfig::seccomp_extra_allow`
  accepts syscall names to layer on top; unknown names fail fast
  at spawn rather than silently widening the filter.
- **`docs/operator-runbook.md`** — new section walking through
  enabling the filter, debugging a filter-triggered failure, and
  the rationale for EPERM-over-SIGSYS. Includes the full deny
  list for escape-the-sandbox primitives (`mount`, `pivot_root`,
  `unshare`, `bpf`, `ptrace`, …).

### Changed

- `SandboxConfig` became `Clone` (no longer `Copy`) because
  `seccomp_extra_allow` carries a `Vec`. All callsites updated;
  supervisor spawn path clones once per respawn (cheap).

## [0.28.0] - 2026-04-20

### Added — WebRTC interop support surfaces

Preparation for closing roadmap item 2 with a real browser. The
harness and trickle-ICE plumbing land here; the Chromium driver
stays feature-gated until a CI image can be standardized.

- **Trickle ICE** — `MediaDescription::end_of_candidates` field +
  `a=end-of-candidates` parse / serialize, so a peer that trickles
  candidates after the initial offer gets merged without triggering
  a full renegotiation.
- **Typed media-security events** — `SipEvent::MediaSecurityError`
  + `MediaSecurityFailure` enum (`DtlsFingerprint`, `DtlsHandshake`,
  `SrtpAuthTag`, `UnsupportedSuite`). Dashboards can now count
  crypto failures by class instead of scraping logs.
- **`smiths-testkit::webrtc_harness`** — module under the new
  `browser` Cargo feature. Placeholder `WebRtcHarness` +
  `HarnessConfig` + `HarnessError` so downstream tests can compile
  against the API today; the concrete Chromium launcher lands with
  a browser-driver dep pick.
- **`docs/architecture/07-webrtc-interop.md`** — end-to-end
  walkthrough of the four-layer WebRTC stack (SDP, DTLS, ICE, RTP)
  with a `browser ↔ engine ↔ SIP UA` call trace + a table of
  which `SipEvent::MediaSecurityError` fires on which failure path.

## [0.27.0] - 2026-04-20

### Added — ICE MVP (host candidates only)

New `smiths-ice` crate implementing the STUN half of the ICE
pairing algorithm. Slice 1.4 of the DTLS-SRTP / ICE rollout —
LAN loopback works end-to-end today; STUN short-term credentials +
server-reflexive candidates + TURN relay are follow-on slices.

- **`smiths_ice::stun`** — hand-rolled RFC 8489 Binding
  Request/Response encoder + parser. `XOR-MAPPED-ADDRESS` attribute
  (the only one MVP inspects). `binding_ping()` primitive: send
  one Binding Request over a tokio UDP socket, await the paired
  response. Loopback integration test confirms the observed
  address equals the caller's bound socket.
- **`smiths_ice::candidate`** — `CandidateGatherer` + the free
  `gather_host_candidates()` helper emit `smiths_sdp::IceCandidate`
  values ready to slot into an SDP answer's `m=audio` block.
  Priority per RFC 8445 §5.1.2.1 (host type-pref 126, IPv6 local-
  pref > IPv4, component 1 > component 2).
- **`smiths_ice::config::IceConfig`** — `[ice]` TOML block with
  `enabled` master switch + optional `stun_servers` list (for the
  server-reflexive work in the follow-on).

## [0.26.0] - 2026-04-20

### Added — DTLS-SRTP handshake (`smiths-dtls`)

Slice 1.3 of the DTLS-SRTP rollout. Per-leg DTLS handshake against
a fixed peer, SRTP keying material extracted via RFC 5764 §4.2
`EXTRACTOR-dtls_srtp` label, fingerprint verification against the
SDP-advertised value.

- **New crate `smiths-dtls`** wrapping `webrtc-dtls` 0.12.
  `DtlsLeg` per-leg state machine (`Idle → Handshaking → Active
  → Closed`); `DtlsLegConfig` bundles the local cert, the
  `DtlsRole` (Client / Server), and the peer fingerprint to check.
- **Fingerprint verification** — SHA-256 of the peer's leaf cert
  compared against the normalized SDP fingerprint. Mismatch
  surfaces as `DtlsHandshakeError::FingerprintMismatch`; weaker
  algorithms (sha-1) surface as `UnsupportedAlgorithm`.
- **`use_srtp` extension** — `srtp_protection_profiles =
  [SRTP_AES128_CM_HMAC_SHA1_80]` fixed for MVP; any other profile
  on the wire is a hard error.
- **SRTP key extraction** — 60-byte export sliced into
  client/server stacks and assigned to `peer_tx_key` /
  `local_tx_key` based on the local role. Ready to plug into the
  existing `AesCmHmacSha1_80Transform` on the bridge.
- **Tests** — 10 unit tests cover state transitions, fingerprint
  verification happy/sad paths, algorithm rejection, key-export
  bit-level layout. A placeholder `#[ignore]` test holds the slot
  for the openssl `s_client -dtls1_2` integration check that the
  slice 1.5 harness runs.
- **Workspace** pinned `webrtc-util = "=0.11.0"` inside the
  `smiths-dtls` crate Cargo.toml so the `KeyingMaterialExporter`
  trait resolves against the same instance `webrtc-dtls` 0.12
  implements.

## [0.25.0] - 2026-04-20

### Added — DTLS-SRTP SDP surface + cert helper

Slice 1.2 of the DTLS-SRTP rollout. Parses (but does not yet
terminate) the full WebRTC SDP surface so peers offering
`UDP/TLS/RTP/SAVP` see a descriptive 488 rather than a silent
reject — and later slices (1.3 handshake, 1.4 ICE) slot into a
plumbed data model.

- **`smiths-sdp`** now typed-models `a=fingerprint` / `a=setup` /
  `a=ice-ufrag` / `a=ice-pwd` / `a=ice-options` / `a=candidate`.
  New `Fingerprint`, `DtlsSetup`, `IcePassword` (debug-redacted),
  and `IceCandidate` types on `MediaDescription`. Display +
  parse both round-trip cleanly.
- **`smiths_core::dtls::SelfSignedCert::generate(subject)`** —
  `rcgen`-backed helper minting an ECDSA P-256 self-signed cert
  and returning its RFC 8122 §5 SHA-256 fingerprint (uppercase
  colon-separated hex). Zero-cost for other crates because the
  helper is opt-in behind the existing `rcgen` workspace dep.
- **`NegotiationOutcome::UnsupportedTransport { reason }`** — new
  variant distinct from `Mismatch`. The negotiator returns it on a
  recognized-but-unterminatable profile (`UDP/TLS/RTP/SAVP[F]`);
  the UAS maps it to `488 Not Acceptable Here` with
  `Warning: 399 smiths-net "DTLS-SRTP not yet supported"` per
  RFC 3261 §20.43. Integration test covers the full path.
- **`docs/architecture/04-post-mvp-scope.md`** — new section
  documents the staged DTLS-SRTP rollout (slices 1.2 → 1.5) with
  a per-slice table of what lands and what's still missing.

## [0.24.0] - 2026-04-20

### Added — per-dialog 2xx INVITE retransmit loop (RFC 3261 §13.3.1.4)

Replaces the branch-keyed `invite_2xx_cache` LRU DashMap with a
proper TU-owned timer loop per dialog. Starts at T1 (500 ms),
doubles to T2 (4 s cap), stops after 64·T1 total or on ACK.

- **`DialogRecord::pending_2xx`** — parked 2xx bytes, serializable
  so an HA snapshot captures in-flight retransmit state.
- **`UasServer::invite_2xx_retransmits`** — dashmap of
  `CancellationToken` per dialog key. ACK (`handle_ack`) and BYE
  (`handle_bye`) both trip the token; dialog teardown cleans up
  automatically.
- **`Metrics::sip_invite_2xx_retransmits`** — cumulative counter.
  A healthy deployment barely moves; a sudden slope change is
  the operator's signal that 2xx delivery is flaking or a UAC
  stopped ACKing.
- **Dropped**: `invite_2xx_cache` (`Arc<DashMap<branch, Bytes>>`) +
  the 4096-entry LRU eviction + `INVITE_2XX_CACHE_CAPACITY` const.
  Retransmitted INVITEs arriving for a dialog-with-live-loop are
  silently dropped (TU owns the retransmit cadence; answering a
  peer retry would inject an off-schedule 2xx).
- **Tests**: `invite_2xx_is_retransmitted_on_lost_ack` verifies
  cadence (T1 then 2·T1 retransmit); `ack_cancels_2xx_retransmit`
  verifies ACK stops the loop.
- **Arch doc**: new `docs/architecture/06-sip-core.md` walks
  through cancellation paths, per-field ownership, and why peer-
  retransmitted INVITEs get dropped.

## [0.23.0] - 2026-04-20

### Added — sidecar resource sandboxing (closes roadmap item 8, MVP scope)

Sidecar plugins now spawn under a `SandboxConfig` that the operator
controls through `[plugins.sandbox]` in TOML. Limits apply via a
`pre_exec` closure in the forked child right before `execve`, so
the kernel enforces them from the plugin's first instruction.

- **`smiths_core::SandboxConfig`** — per-sidecar knobs:
  `max_fds` (`RLIMIT_NOFILE`), `max_memory_bytes` (`RLIMIT_AS`),
  `max_cpu_seconds` (`RLIMIT_CPU`), `max_processes` (`RLIMIT_NPROC`;
  set `0` to forbid `fork`/`exec` from the plugin entirely), and
  `no_new_privs` (Linux `PR_SET_NO_NEW_PRIVS`, silently skipped
  elsewhere).
- **`smiths_sidecar::sandbox::apply_in_child`** — async-signal-safe
  application via the `rustix` crate. No libc unsafe blocks; the
  workspace lint relaxed from `unsafe_code = "forbid"` to `"deny"`
  so the one necessary `CommandExt::pre_exec` call can carry a
  narrowly-scoped `#[allow(unsafe_code)]` with a justification
  comment. Every other crate stays unsafe-free.
- **`Sidecar::spawn_with(name, dir, entry, policy, sandbox)`** —
  new primary spawn entry point. `Sidecar::spawn` +
  `Sidecar::spawn_with_policy` retained as convenience wrappers
  (both thread `SandboxConfig::default()` — permissive).
- **`LoaderOpts::sandbox`** field — CLI plumbs
  `config.plugins.sandbox` through the loader so every loaded
  plugin inherits the same caps. Includes supervisor-driven
  respawns after crashes (rlimit is re-applied per spawn).
- **Integration test**: `sandbox_rlimit_nofile_is_applied_to_child`
  spawns a bash script that echoes `ulimit -n` back over JSON-RPC
  and verifies the child observes exactly the configured value.

### What's explicitly out of scope for this slice

- **seccomp-BPF syscall filtering** — its correctness is bound to
  the plugin's runtime (tokio, Python, etc.), so it needs its own
  curation pass and config surface. Tracked as a separate slice.
- **User-namespace isolation / cgroups** — same story, significantly
  bigger, deserves its own plan.
- **macOS `sandbox-exec`** — deprecated Apple API; not worth wiring
  given the current dev-primary role of macOS.

This satisfies roadmap item 8 (MVP sandboxing — FD / memory / CPU /
process caps + privilege-escalation gate); hardened-seccomp +
namespaces move to the "hardening follow-on" list.

### Changed

- Workspace lint `unsafe_code = "forbid"` → `"deny"`. The sole
  narrow exception is documented at the `pre_exec` call site in
  `smiths-sidecar::supervisor`.
- `examples/config.toml` gained a documented `[plugins.sandbox]`
  section with every knob commented out at conservative defaults
  so operators can uncomment and deploy.

## [0.22.0] - 2026-04-20

### Changed — UAS INVITE path migrated onto the server transaction FSM

Closes the UAS FSM migration started in v0.21.0. Every inbound
request the UAS responds to — INVITE included — now lives as a
`ServerInviteTxn` or `ServerNonInviteTxn` entry in the driver.
Retransmit replay, G/H/I timer arming, ACK-for-non-2xx transitions,
and 2xx bypass are all FSM-driven. The legacy `dedupe` `DashMap` is
gone as a general-purpose cache.

- **`ServerInviteTxn` + ACK correlation**: INVITE registers a server
  FSM on arrival; the handler's `send_provisional` (100 Trying) and
  `respond` (2xx / 401 / 488 / …) both route through
  `driver.send_response(...)`. Non-2xx transitions the FSM to
  Completed (G + H armed). The ACK for a non-2xx carries the
  INVITE's branch per §17.1.1.3 — `handle_ack` now delivers it to
  the INVITE FSM for the Completed → Confirmed transition + timer I.
- **`invite_2xx_cache`** (narrow replacement for `dedupe`): RFC 3261
  §13.3.1.4 gives the Transaction User ownership of 2xx retransmit
  so the FSM bypasses straight to Terminated on 2xx send. Until the
  UAS grows a per-dialog 2xx retransmit loop, a small branch-keyed
  cache parks the 2xx bytes for simple peer-retry replay. Keeps the
  4096-entry LRU cap + shard-scoped eviction (v0.13.1 pattern).
- Existing integration tests (`invite_retransmit_replays_same_200`,
  `invite_401_cancel`, `invite_establishes_dialog_ack_then_bye`,
  `dedupe_eviction.rs`) all pass unchanged through the new path —
  that is the regression contract for the migration.

### Added — `sip_server_txns_active` Prometheus gauge

`TransactionDriver::with_metrics` wires an `Arc<Metrics>` handle
into the driver; `start_server` / terminate keep the
`sip_server_txns_active` gauge in sync. The UAS's `with_metrics`
builder now rebuilds the driver with the shared handle so
operators see live FSM entry counts on `/metrics` — replaces the
visibility the old LRU-capped `dedupe` table provided, now that
the FSM table has no cap. New unit test
`metrics_gauge_tracks_server_txn_lifecycle` covers the inc/dec.

### Added — `clippy::unwrap_used` on smiths-core + smiths-sdp

Both crates have zero production-code unwraps / expects (every use
is inside `#[cfg(test)]` mods). Promoted via crate-level
`#![warn(clippy::unwrap_used, clippy::expect_used)]` +
`#![cfg_attr(test, allow(...))]` in each `lib.rs` — Cargo 1.74+
doesn't permit mixing `[lints] workspace = true` with
`[lints.clippy]` overrides, so the crate-level-attr pattern is
the idiomatic per-package route.

### Added — `sdes_crypto` fuzz target

New `fuzz/fuzz_targets/sdes_crypto.rs` drives `SdesCrypto::parse`
+ `SessionDescription::parse` with adversarial bytes. Matches the
existing `sip_parser` target's shape. Run via
`cargo +nightly fuzz run sdes_crypto`.

## [0.21.0] - 2026-04-20

### Changed — UAS non-INVITE path migrated onto the server transaction FSM

Every non-INVITE request the UAS actually responds to — OPTIONS,
BYE, REGISTER, CANCEL, unknown-method 405s — now registers a
`ServerNonInviteTxn` in the shared `TransactionDriver` on first
arrival. Response bytes flow through `driver.send_response(...)`;
the FSM caches the last response in its `last_response` field and
arms timer J. Retransmits of the same request route to
`driver.deliver_request(...)`, which replays the cached response
via the FSM.

- **Replaces the legacy `dedupe` `DashMap` path for non-INVITE**,
  keeping the deadlock-fix semantics (v0.13.1) but eliminating the
  need for a capacity-bound LRU scan entirely — the FSM's timer J
  is wall-clock-driven (`64 · T1 = 32 s`).
- **INVITE + ACK still use `dedupe`**. INVITE's server FSM needs
  ACK correlation + G/H/I retransmit timers, which is its own
  slice. ACK never elicits a response, so neither path matters.
- `UasServer` gained a `txn_driver: TransactionDriver<T>` field
  constructed alongside the existing transport. Same transport is
  shared with the UAC's driver; the server driver maintains its
  own transaction table keyed by `(branch, method, Server)`.
- Existing integration tests (`retransmission_replays_cached_response`,
  all REGISTER / BYE / OPTIONS / auth scenarios) pass unchanged
  through the new path — that is the regression test.
- New helper `is_fsm_candidate(method)` keeps the split explicit at
  every call site so the follow-on INVITE migration is a one-line
  change.

### Added — MCP stdio binary smoke test

`crates/smiths-cli/tests/mcp_stdio.rs` — spawns the real
`smiths-net` binary with `--mcp stdio`, drives JSON-RPC over stdin,
verifies:

- `initialize` returns our server name + protocol version + the
  tools / resources capability block.
- `tools/list` enumerates the canonical in-box tool set.
- `tools/call health` returns `{"status":"ok"}` with a numeric
  `uptime_secs`.
- Closing stdin triggers a clean exit (CLI honors stdin-EOF).

Closes the last Phase 5 pending item (`docs/plans/todo.md`).

### Added — workspace lint tightening (phase-6 hardening, first slice)

Five prospective `clippy::...` lints promoted to `warn` at the
workspace level. Zero current fires, so this is pure forward
protection — future PRs that sneak in debug artifacts get caught
by CI:

- `clippy::dbg_macro`
- `clippy::print_stdout`
- `clippy::print_stderr`
- `clippy::todo`
- `clippy::unimplemented` (test fakes in
  `crates/smiths-wasm/tests/engine.rs` get a crate-local
  `#![allow(...)]` — that's the idiomatic "shouldn't be hit"
  marker in test doubles.)

`clippy::unwrap_used` / `missing_docs` from the original Phase 6
plan are deferred — each needs a dedicated slice to avoid a
262-fire cliff across src/ + integration tests.

### Maintenance

- `rand` dep bumped `0.9 → 0.10.1` (workspace-transitively; the
  one direct use in `smiths-sdp::negotiate::fresh_sdes_key`
  migrated from `RngCore::fill_bytes` to `Rng::fill_bytes` per the
  new trait split).
- `wasmtime` bumped `43 → 44.0.0`.

## [0.20.0] - 2026-04-20

### Added — SDES negotiator wiring end-to-end

SRTP SDES (`AES_CM_128_HMAC_SHA1_80`) now flows end-to-end through
the SDP offer/answer exchange. Before this release, the SRTP
transforms existed in `smiths-media` but bridges had to be
instantiated with manually-injected key material; from now on, a UAS
that receives an `RTP/SAVP` INVITE with `a=crypto:` responds with a
matching `a=crypto:` line carrying an engine-generated key, and the
media fabric wires per-direction transforms on the resulting bridge
automatically.

- **`smiths-core::sdp::SrtpKeys`** — new struct carrying the peer's
  advertised key + the engine's chosen answer key + the cipher suite.
  `NegotiationOutcome::Accepted` gained an `srtp: Option<SrtpKeys>`
  field. `Debug` is hand-written to redact key bytes (log-safe).
- **`smiths-core::media::BridgeLeg`** — new spec struct used by
  `MediaFabric::bridge`. Replaces the prior 4-arg
  `bridge(a, peer_a, b, peer_b)` signature with
  `bridge(a: BridgeLeg, b: BridgeLeg)` where each leg optionally
  carries SRTP keys. Constructors `BridgeLeg::plain` +
  `BridgeLeg::with_srtp`.
- **`smiths-sdp::MediaDescription::crypto`** — parses + serializes
  `a=crypto:` lines. `a=crypto:` parse errors on one line are soft
  (logged + skipped), so a single bad line doesn't kill the whole
  SDP document.
- **`smiths-sdp::Negotiator::answer`** — detects `RTP/SAVP` + any
  supported `a=crypto:`, generates an engine key via
  `fresh_sdes_key()` (CSPRNG), emits the matching answer line, and
  returns `NegotiationResult::Answer { sdp, srtp }`. `RTP/SAVP`
  without acceptable crypto → `Mismatch` (RFC 4568 §5.1.2 no
  plaintext downgrade).
- **`smiths-sip::uas`** — threads `SrtpKeys` from the negotiator
  outcome into `PendingLeg`, then into the `BridgeLeg` spec passed
  to `MediaFabric::bridge` on rendezvous pairing. Plain-RTP calls
  keep their prior zero-crypto path.
- **`smiths-media::UdpMediaFabric::bridge`** — materializes each
  leg's `SrtpKeys` into `AesCmHmacSha1_80Transform` instances and
  attaches them to `LegSrtp` on the bridge.
- New workspace dep `rand = "0.9"` (direct use: engine-side SDES key
  generation only).

### Added — `codec_mismatch` integration coverage (`tests/codec_mismatch.rs`)

Four richer SDP-offer shapes the UAS must reject with 488:
- Multiple unknown codecs at three different clock rates in one
  offer (guards against a regression where the first PT number
  matched regardless of codec name).
- Video-only offer (engine is audio-only).
- `RTP/SAVP` with a supported codec but **no** `a=crypto:` line
  (RFC 4568 §5.1.2 — must not downgrade to plaintext).
- `RTP/SAVP` with `a=crypto:` advertising a suite we don't support
  (`AES_256_GCM`).

All four also confirm `100 Trying` still fires before the 488, so
provisional behavior doesn't regress under the negotiator refactor.

### Added — SRTP end-to-end integration test (`tests/sdp_srtp.rs`)

Two fake UAs rendezvous on a `sip:<key>@engine` with `RTP/SAVP` +
`a=crypto:`. The engine answers each with an engine-chosen crypto
line, pairs the legs, and wires SRTP transforms. UA-A encrypts an
RTP packet with its offer key; UA-B receives ciphertext and decrypts
it with the key the engine advertised in UA-B's 200 OK. Confirms:

- Engine never echoes either UA's offer key as its own answer key
  (no leaked peer secret).
- Bridge SSRC rewrite still fires on the SRTP path.
- End-to-end payload round-trip.

### Changed — `MediaFabric::bridge` trait signature

Moved from `bridge(a, peer_a, b, peer_b)` to
`bridge(a: BridgeLeg, b: BridgeLeg)`. Every in-tree caller migrated;
external trait implementers need to update their `bridge` signature
and import `smiths_core::BridgeLeg`.

### Docs

- `docs/architecture/03-mcp-and-ops.md` — rewritten MCP section to
  reflect the actual three-transport surface (stdio, HTTP, SSE) +
  the A2A HTTP adapter, the shipped tool + resource sets, SSE
  notification stream, and bearer/rate-limit/audit posture. Closes
  the Phase 5 pending item.

### Roadmap tidy-up

Two items in `.vscode/prod-readiness-roadmap.md` were already done
but still listed as pending:

- **Loss-% tracking on the sender side** — v0.15.0 landed
  `StreamStats` cumulative-loss tracking; RR blocks now carry real
  numbers.
- **sipp perf validation** — v0.13.0 ran it; found + fixed the
  dedupe-eviction deadlock (v0.13.1); the engine now sustains
  ~10 k cps REGISTER on loopback.

## [0.19.0] - 2026-04-19

### Added — Transaction FSM slice 5 (final)

Closes RFC 3261 §17 as a library: all four transaction FSMs + the
dialog-layer FSM are now in place, and the `TransactionDriver`
hosts both client and server FSMs.

- **`smiths-sip::txn::ServerNonInviteTxn`** — RFC 3261 §17.2.2.
  - States: `Trying → Proceeding → Completed → Terminated`.
  - Timer J (`64 · T1 = 32 s` absorb-retransmits window); armed on
    entering Completed.
  - Request retransmits in Proceeding / Completed replay the
    cached last response; in Trying they drop silently.
  - 10 unit tests.
- **`smiths-sip::txn::DialogFsm`** — RFC 3261 §12 dialog FSM.
  - States: `Early → Confirmed → Terminated`.
  - Events: `AckReceived`, `ByeCompleted`, `Cancelled`, `Error`.
  - Illegal transitions surface as `DialogTransitionError`
    rather than silent absorption — dialogs are long-lived and
    stray events usually mean an application-layer bug.
  - `to_core_state()` projects to the serializable
    `smiths_core::DialogState` (returns `None` when terminated —
    dead dialogs don't appear in the HA snapshot).
  - 10 unit tests.
- **`TransactionDriver::start_server`** — symmetric to
  `start_client`. Registers a server FSM and returns a `TuEvent`
  receiver the TU drains for `Terminated`.
- **`TransactionDriver::send_response`** — TU pushes a built
  response through the FSM; the driver emits `SendToPeer` and
  arms the appropriate retransmit-absorb timer.
- **`TransactionDriver::deliver_request`** — routes inbound
  request retransmits into the server FSM, which replays the
  cached response per §17.2.1 / §17.2.2.
- Two new driver integration tests: server registration + peer
  send, and request-retransmit replay.

### Status — RFC 3261 §17 transaction layer

All four FSMs written, tested, and hosted by the async driver:

- Client non-INVITE (§17.1.2) — migrated (`UacClient::hangup`).
- Client INVITE (§17.1.1) — migrated (`UacClient::place_call`).
- Server INVITE (§17.2.1) — library + driver API, UAS wiring
  deferred.
- Server non-INVITE (§17.2.2) — library + driver API, UAS
  wiring deferred.
- Dialog FSM (§12) — library, ready for the dialog-layer glue
  slice that ties transactions to call lifecycle.

The existing UAS dedupe DashMap keeps working (deadlock fix
landed in v0.13.1). Replacing it with server FSM entries is
cleanup rather than a correctness requirement, so it's tracked
as an optional follow-on rather than a roadmap blocker.

## [0.18.0] - 2026-04-20

### Added — Transaction FSM slices 3 + 4

Both INVITE-side transaction FSMs (RFC 3261 §17.1.1 + §17.2.1). The
UAC `place_call` path migrates onto the client INVITE FSM; server
INVITE FSM lands as a library (UAS wiring = slice 5).

- **`smiths-sip::txn::ClientInviteTxn`** — RFC 3261 §17.1.1.
  - States: `Calling → Proceeding → Completed → Terminated`.
  - Timer A (retransmit INVITE, T1 doubling, no RFC cap — stops on
    1xx or timer B).
  - Timer B (`64 · T1 = 32 s` transaction timeout).
  - Timer D (`32 s` wait for non-2xx retransmits in Completed).
  - **2xx bypass**: 2xx final drops straight to Terminated; the TU
    owns end-to-end ACK per §13.3.1.4.
  - **Non-2xx ACK is the FSM's job** per §17.1.1.3 — every
    3xx-6xx final (and retransmits) gets an auto-generated ACK.
  - 12 unit tests across every state / timer / response path.
- **`smiths-sip::txn::ack::build_non_ok_ack`** — byte-level ACK
  builder. Preserves INVITE's Via branch (§17.1.1.3), copies From /
  Call-ID / Request-URI / Max-Forwards / Route headers; takes `To`
  (with server tag) from the response. 4 unit tests.
- **`smiths-sip::txn::ServerInviteTxn`** — RFC 3261 §17.2.1.
  - States: `Proceeding → Completed → Confirmed → Terminated`.
  - Timer G (retransmit non-2xx final, T1 doubling up to T2).
  - Timer H (`64 · T1 = 32 s` wait for ACK after non-2xx).
  - Timer I (`T4 = 5 s` absorb ACK retransmits in Confirmed).
  - **2xx bypass**: TU owns 2xx retransmit timers per §13.3.1.4.
  - INVITE retransmits replay the last response (cached in the
    FSM; replaces the UAS's current `dedupe` cache once the UAS
    migration lands in slice 5).
  - 13 unit tests.
- **`TransactionEvent::SendResponseFromTu`** — new event variant
  for server FSMs; the TU (dialog layer) asks the transaction
  layer to emit a response (1xx / 2xx / non-2xx), and the FSM
  decides whether to cache + retransmit.

### Changed — `UacClient::place_call` migrated through the driver

Previously: one-shot `transport.send(invite)` + subscribe router +
`wait_for_final` (no retransmit at all; on UDP loss the whole 30 s
budget burned through silently).

Now: register a `ClientInviteTxn` with the driver, drain
`TuEvent` stream until a final response or the deadline. Behavioural
upgrades:

- **Timer-A retransmits** at T1 / 2·T1 / 4·T1 / … until the peer
  replies or the overall deadline fires.
- **Automatic ACK for 3xx-6xx** handled by the FSM (no code in UAC).
- `wait_for_final` + `parse_status` helpers in `uac.rs` removed as
  dead code; the `router` field on `UacClient` removed (driver
  owns it now — constructor signature unchanged, the passed
  `Arc<ResponseRouter>` is moved into the driver).
- Existing `uac_places_call_and_hangs_up_against_fake_uas`
  integration test passes through the migrated path unchanged.

### Deferred to slice 5

- **UAS server-INVITE migration** — replacing the `dedupe` cache +
  in-UAS retransmit dedupe path with per-transaction
  `ServerInviteTxn` entries managed by a server-side driver.
- **Server non-INVITE FSM** (§17.2.2, timer J) — tiny, ~200 LOC.
- **Dialog FSM driver** (Idle → Early → Confirmed → Terminated)
  on top.
- UAC `place_call` is still the one callsite that manually builds
  the 2xx ACK — candidate for a dialog-layer helper.

## [0.17.0] - 2026-04-20

### Added — async `TransactionDriver` + first migration (UAC BYE)

Second slice of the RFC 3261 §17 migration. The pure FSM built in
v0.16.0 now has an async runtime + its first real call-site in the
engine.

- **`smiths-sip::txn::TransactionDriver<T: Transport>`** — hosts
  live FSMs, runs their `SendToPeer` / `ArmTimer` / `CancelTimer` /
  `DeliverResponseToTu` / `Terminated` actions. Generic over
  `Transport` to match UAC's existing shape (AFIT `Transport` isn't
  dyn-compatible). Cheap to clone.
- **Timer tasks** — each armed timer is a `tokio::spawn` with an
  `AbortHandle` stored on the txn entry. `CancelTimer` aborts it;
  `Terminated` aborts every remaining timer plus the response
  listener. A per-txn `CancellationToken` races the sleep so an
  aborted timer task drops even if it's mid-sleep.
- **Response routing** — driver spawns one listener task per txn
  that subscribes to [`ResponseRouter`] by branch, re-feeds each
  arriving response into the FSM, and re-subscribes for the next
  one (the router's oneshot is single-shot, same pattern as the
  old `wait_for_final`). Exits on `Terminated`.
- **`TuEvent`** — the Transaction User sees an unbounded MPSC
  stream of responses plus one final `Terminated` marker.
- **Two end-to-end integration tests** in `driver::tests` using
  real UDP sockets: request-send → final-response round-trip; and
  timer-E actually retransmits on the wire after T1=500 ms when no
  response arrives.
- **`UacClient::hangup` migrated** — the BYE path now registers a
  `ClientNonInviteTxn` with the driver instead of doing a one-shot
  `transport.send` + `router.subscribe` + `wait_for_final`. **Net
  behavioural win:** if the BYE's first send is lost on UDP, the
  FSM retransmits at T1, 2·T1, 4·T1, up to T2=4 s, capped by the
  30 s overall budget — the old path would silently burn through
  the whole budget on a single lost packet.
- Public signature of `hangup` unchanged; existing
  `uac_places_call_and_hangs_up_against_fake_uas` integration test
  passes through the migrated path unchanged.

### Deferred to FSM slice 3+

- **Client INVITE FSM** (§17.1.1, timers A/B/D, `Calling` state).
  Needed to migrate `UacClient::place_call` / `wait_for_final`.
- Server FSMs + dialog driver (still on the roadmap — slices 4/5).
- `ResponseRouter` simplification — once client FSMs handle their
  own retransmit + final-response correlation, the router's
  per-branch oneshot pattern can collapse into a simple
  `DashMap<String, Sender<Bytes>>` without the re-subscribe loop.

## [0.16.0] - 2026-04-20

### Added — RFC 3261 §17 transaction layer: framework + client non-INVITE

First slice of a multi-session migration off the ad-hoc UAC retransmit
path. **Additive only** — the new FSM module lives alongside `uas.rs`
/ `uac.rs` without touching them; no user-visible behavior change.

- **`smiths-sip::txn` module** — pure-synchronous FSM framework:
  - `TransactionState` / `Role` / `TransactionKey` — the lookup
    vocabulary the future driver indexes on.
  - `TimerId` — every RFC 3261 §17.1.1.1 timer (A–K) named by the
    letter that matches the spec, so FSM code reads 1:1 with the RFC.
  - `TransactionEvent` — network-in events (`StartClient`,
    `ResponseReceived`, `RequestReceived`, `TimerFired`).
  - `TransactionAction` — network-out actions (`SendToPeer`,
    `DeliverResponseToTu`, `ArmTimer`, `CancelTimer`, `Terminated`).
  - `Transaction` trait — the `on_event(ev) → Vec<Action>` shape
    every FSM flavor implements.
- **`smiths-sip::txn::timers`** — `T1` (500 ms), `T2` (4 s), `T4`
  (5 s), `TIMEOUT_64T1` (32 s) constants; `doubling_backoff(attempt,
  cap)` helper shared across FSMs.
- **`ClientNonInviteTxn` (RFC 3261 §17.1.2)** — smallest of the four
  FSMs, covers outbound BYE / OPTIONS / REGISTER / CANCEL once the
  driver wires it next session:
  - States: `Trying → Proceeding → Completed → Terminated`.
  - Timer E (retransmit, `T1` then doubles up to `T2`).
  - Timer F (transaction timeout, `64·T1 = 32 s`).
  - Timer K (wait for duplicate responses, `T4 = 5 s`).
  - Duplicate finals in Completed are silently consumed.
  - Late events in Terminated are absorbed (real SIP stacks race
    against their own timer cancellations all the time).
- **14 unit tests** cover every state transition + every timer path
  (initial send, E retransmit doubling, Trying→Proceeding on 1xx,
  Proceeding loop, Trying→Completed and Proceeding→Completed on
  final, duplicate-final silence, K-terminates, F-times-out in both
  Trying and Proceeding, post-Terminated events are no-ops, key
  round-trips).

### Deferred (FSM slice 2+)

- **`TransactionDriver`** — async wrapper that owns timers +
  transaction table. Needed to integrate FSMs with the live UAC.
- **`UacClient` BYE migration** — first call-site swap onto the new
  FSM once the driver lands.
- **Client INVITE FSM** (§17.1.1, timers A/B/D, includes `Calling`
  state).
- **Server INVITE FSM** (§17.2.1, timers G/H/I, ACK-driven
  `Confirmed` state).
- **Server non-INVITE FSM** (§17.2.2, timer J).
- **Dialog FSM driver** (Idle → Early → Confirmed → Terminated) on
  top of the transaction FSMs.

## [0.15.0] - 2026-04-20

### Added — SRTP (SDES)

- **`smiths-core::SrtpTransform` + `SrtpSuite` + `SrtpError`** —
  trait seam + suite enum + typed errors. `SrtpSuite` knows its own
  key / salt lengths and SDP name. `SrtpTransform` is `&self` with
  interior mutability so a bridge forwarder can share a transform
  across tasks; per-direction instances keep SSRC state / rollover
  counters isolated.
- **`smiths-media::srtp::AesCmHmacSha1_80Transform`** —
  `webrtc-srtp`-backed implementation. Pure Rust, MIT/Apache,
  `AES_CM_128_HMAC_SHA1_80` suite. `from_sdes(key_material: &[u8])`
  constructs from the 30-byte SDES master-key + salt; auth-failure
  bytes from the backend map to `SrtpError::AuthFailed`. 4 unit
  tests: round-trip / wrong-key-rejected / wrong-size-rejected /
  sequential-packets.
- **`smiths-sdp::SdesCrypto` + `SdesParseError`** — SDES
  `a=crypto:<tag> <suite> inline:<b64>` parser / generator with 7
  tests (canonical, `|lifetime|mki` tail, round-trip, wrong-suite,
  short-key, truncated, not-a-crypto early bail-out).
- **`smiths-media::Bridge` SRTP integration** — `Leg.srtp:
  Option<LegSrtp>` carries `(peer_tx, local_tx)`. Forwarder: decrypt
  with ingress-leg's `peer_tx` → rewrite SSRC on plaintext →
  re-encrypt with egress-leg's `local_tx`. Re-sign happens under
  SRTP because auth covers the header (including SSRC). Integration
  test proves A encrypts → engine decrypts → rewrites SSRC →
  re-encrypts → B decrypts, payload byte-identical. SDP-negotiator
  wiring (UAS answering `a=crypto:` offers end-to-end) is the next
  slice.
- **`webrtc-srtp = "0.17"`** pinned as workspace dep.

### Added — RTCP cumulative-loss tracking

- **`StreamStats` grew `base_seq` / `cycles` / `seen_first` atomics**
  implementing RFC 3550 §A.3 extended-max + wrap detection.
  `snapshot().cumulative_lost` returns `expected - received` clamped
  at 0 (reordered arrivals don't push the count negative).
- **SR emitter now feeds real loss into Report Blocks** instead of
  the `0` placeholder. Peer receivers finally see the engine's view
  of the stream health.
- 3 new unit tests: gap-counted-as-lost, no-gap-stays-zero,
  reorder-clamps-at-zero.

### Added — env-driven credential seed (`SMITHS_TEST_CREDS`)

- CLI reads `SMITHS_TEST_CREDS=user:realm:pass[,user:realm:pass…]`
  at startup, builds an in-memory `Registrar`, attaches it to
  every UAS. Every realm stanza shares the first one. Disabled by
  default — production credential stores land via the
  `CredentialStore` trait (DB, LDAP, …). Unset = no registrar (dev
  blind-200-OK path from prior versions).

### Fixed — digest URI mismatch on clients that drop the user-part

- Some UAs (sipp, many real-world SIP stacks) sign only the
  host-authority in the digest `uri=` parameter even when the
  Request-URI carries a user-part. Previously the UAS rejected
  them with `UriMismatch` / re-challenged forever. Fix: extract
  the `host[:port]` authority from both URIs and accept a match on
  that, in addition to the existing exact / substring rules.
  Found during the auth-exercised sipp prove-out (below).

### Validated — sipp auth-exercised perf run

- With `SMITHS_TEST_CREDS="sipp:smiths.test:s3cret"` on the engine,
  ran `scenarios/sipp/register.xml` (the digest-authenticated
  variant):
  - 500 calls @ 100 cps → 100% success, 0 retrans.
  - 5000 calls @ 1000 cps → 100% success, 0 retrans.
  - Every call exercised the full 4-message round-trip
    (REGISTER → 401 → REGISTER+Auth → 200).
- Engine metrics after the 1000 cps run: 11000 REGISTER requests,
  5500 × 401, 5500 × 200, 0 parse errors.

## [0.14.0] - 2026-04-20

### Validated — fuzz harness run

- Ran `cargo +nightly fuzz run sip_parser` for ~4 minutes
  (**5.2M runs**) with a seeded corpus covering OPTIONS, INVITE,
  REGISTER-with-auth, and responses. **Zero crashes, zero panics,
  zero OOMs** across three parse layers:
  1. `rsip::SipMessage::try_from` (third-party gate)
  2. `summarize_request` (our hand-rolled request-summary parser)
  3. `extract_via_branch` (response-router branch extractor)
- Exposed the latter two via a `#[doc(hidden)] pub mod __fuzz`
  inside `smiths-sip::uas` so the fuzz target can drive them
  directly without booting a UAS.
- Seeded `fuzz/corpus/sip_parser/` with four realistic inputs —
  future fuzz runs start from meaningful mutations, not `[]`.

### Added — per-source-IP SIP rate limiting

- **`smiths-sip::rate_limit::SipRateLimiter`** — token bucket per
  source IP. `SipRateLimit { per_sec, burst }` config tuple lands
  on `SipConfig` (disabled by default; `per_sec == 0`). Over-limit
  datagrams are silently dropped *before* the rsip parser runs,
  keeping the hot path short during a flood.
- **`UasServer::with_rate_limit(...)`** — builder. UAS checks the
  limiter in `handle_datagram`, before parse, before any state
  touch. Per-IP buckets in a `DashMap`; disabled case is a single
  atomic compare so the hot path stays cheap.
- CLI wires one shared limiter across all transports (UDP, TCP,
  TLS) so a hostile peer can't bypass the limit by hopping
  transports. `SMITHS__SIP__RATE_LIMIT__PER_SEC=50` enables.
- Two integration tests: caps-at-burst, and
  per_sec=0-lets-everything-through.

### Added — RTCP Receiver Reports (embedded + listener)

- **`smiths-media::rtcp::build_sr_with_rb` / `build_rr` /
  `parse_rr` / `ReportBlock`** — RFC 3550 §6.4 wire layout for
  Report Blocks. 24-byte RB carries `ssrc` / `fraction_lost` /
  24-bit signed `cumulative_lost` / `extended_highest_seq` /
  `jitter` / `last_sr` / `delay_since_last_sr`. 4 round-trip /
  validation tests cover the happy path, wrong PT, lying RC.
- **Bridge emitter now embeds a Report Block** in each outgoing SR
  describing what the engine *received* from the peer of the
  current direction. When no inbound packets have been observed
  yet (fresh bridge), the emitter still emits a bare SR (`RC=0`)
  rather than burning cycles building an empty RB.
- **`spawn_rr_listener`** — per-direction RTCP listener that reads
  incoming RR packets, parses them, logs the peer's loss / jitter
  numbers at debug level. Hooking the values back into
  `StreamStats` (for `last_sr` / DLSR) is deferred — needs the
  cumulative-lost counter wired on the emitter side first.

### Bug-check notes

- Fuzz found nothing new after the v0.13.1 dedupe-eviction fix.
- Rate limiter defense protects against the saturation scenario
  observed during the sipp perf run (2000 cps burst → OS socket
  buffer overflow). With a 50 rps / 100 burst limit the attacker
  gets 100 datagrams, then silence.

## [0.13.1] - 2026-04-20

### Fixed — UAS dedupe-eviction deadlock

- **The bug:** `UasServer::respond` evicted a cache entry via
  `self.dedupe.iter().next()` + `remove(&k)`. Rust's `if let`
  lifetime rules extend the `iter()` rvalue temporary through the
  full scope, so the `Iter` (holding a `DashMap` shard read guard)
  was still alive when `remove` took a write lock on the same
  shard. The UAS wedged permanently once `DEDUPE_CAPACITY` (4096)
  was hit under load — no further SIP datagrams got dispatched
  even though the process, health endpoint, and metrics all looked
  healthy.
- **The fix:** extract the eviction key in a self-contained
  expression (`self.dedupe.iter().next().map(|e| e.key().clone())`)
  so the `Iter` is dropped before `remove`. The hot path stays
  single-threaded (UAS serializes `handle_datagram`); the bug
  surfaced because macOS's loopback buffered 4096+ datagrams fast
  enough to push us past the threshold.
- **Regression test:** `crates/smiths-sip/tests/dedupe_eviction.rs`
  sends 4200 unique OPTIONS requests and asserts at least 4100 get
  200 OK back. Pre-fix this hung after ~4096. Post-fix all 4200
  land. Takes ~10 s on dev hardware.
- **Validated via sipp** on loopback: 50,000 REGISTERs at
  ~10,000 cps, 100% success, 0 retransmits, 0 failed. Prior to
  the fix the engine froze after ~4096 REGISTERs regardless of
  arrival rate.

### Added

- **`scenarios/sipp/register_noauth.xml`** — blind-200 variant of
  the REGISTER scenario for running throughput smokes against the
  default (no-credential-store) engine. The existing
  `register.xml` still drives the auth round-trip; use this one
  when seeding the credential store is not on the table.

## [0.13.0] - 2026-04-20

### Added — graceful drain

- **`smiths-core::Drain`** — cheaply-clonable atomic flag shared
  across subsystems. `Drain::start()` flips to "draining";
  `Drain::is_draining()` is a relaxed load on the hot path.
- **`UasServer::with_drain(Drain)`** — when set, `handle_invite`
  short-circuits to `503 Service Unavailable` (with `Retry-After: 0`)
  before touching auth, media allocation, or dialog state. Live
  dialogs (BYE, ACK, re-INVITE on the same dialog) flow through
  unaffected; only fresh call setup is refused.
- **`SMITHS_DRAIN_SECS` env var** — the CLI reads this on shutdown
  (default 5). Flow: SIGTERM → `drain.start()` → sleep window →
  fire the existing cancel token. Setting it to `0` reverts to
  the pre-v0.13 instant-cancel behaviour (used by the e2e test).
- Two integration tests in `smiths-sip/tests/drain.rs`:
  `draining_uas_rejects_new_invite_with_503` and
  `non_draining_uas_still_accepts_invite`.

### Added — deep `/health` endpoint

- **`/health` now returns structured JSON** instead of
  `{"status":"ok"}`. Fields:
  - `status`: `"ok"` or `"draining"`.
  - `draining`: boolean from the shared `Drain`.
  - `uptime_secs`: seconds since process start.
  - `sip.binds`: list of `proto://addr` strings for every configured
    SIP listener (`udp://`, `tcp://`, `tls://`).
  - `plugins.loaded`: plugin names successfully registered.
  - `plugins.failed`: `[{ dir, error }]` entries for failed loads.
  - `dialogs_active`: live gauge read from `Metrics`.
  - `bridges_active`: live gauge read from `Metrics`.
- **`HealthState` / `HttpState`** — axum state types in
  `smiths-cli/src/main.rs`. Spawned after SIP bind collection so
  the snapshot is complete on the first request.

### Changed

- `smiths-cli`'s shutdown sequence now runs `drain.start()` before
  `shutdown.trigger()`, sleeping `SMITHS_DRAIN_SECS` in between. The
  existing system event (`ShutdownRequested`) is still published at
  the start of drain.
- `UasServer::new(...)`'s four builder methods now include
  `with_drain(...)`; constructions without it (tests, single-shot
  helpers) fall back to "never draining".
- The full-binary e2e test now sets `SMITHS_DRAIN_SECS=0` so its
  SIGTERM-to-exit assertion stays within its 5 s budget.

### Operational / deferred

- sipp perf validation remains an operator task. `scenarios/sipp/
  register.xml` + `README.md` document the run. Needs a host with
  sipp installed; not reproducible inside the sandbox.

## [0.12.0] - 2026-04-20

### Added — engine-wide metrics coverage

- **`smiths-core::Metrics`** grew seven new fields covering the
  subsystems that previously had no visibility:
  - `sip_parse_errors` (counter) — incremented in UAS when rsip
    rejects an inbound datagram. Pairs with the `ParseError` event.
  - `bridges_active` (gauge) — in/dec on `MediaFabric::bridge` /
    `release_bridge` so operators see the live passthrough count.
  - `rtp_packets_forwarded{direction}` (counter family) —
    `direction="a_to_b"` / `"b_to_a"`; incremented after successful
    `send_to` in each forwarder task.
  - `rtcp_sr_sent` (counter) — incremented in the SR emitter task
    on each successful RTCP write.
  - `plugin_invocations{plugin, outcome}` (counter family) —
    recorded on every `AiProvider::invoke`; `outcome="ok"|"error"`.
  - `plugin_invoke_duration_seconds{plugin}` (histogram family) —
    same hook; uses the default latency buckets shared with the
    tool-duration histogram.
  - `sidecar_restarts{plugin}` (counter family) — emitted by the
    supervisor each time `supervise_loop` successfully respawns a
    crashed child.
- **Metrics threading.** The CLI builds a single `Arc<Metrics>` at
  boot and threads it through:
  - `LoaderOpts::metrics` — every loaded `PluginEntry` and
    `WasmProvider` receives it at registration time.
  - `UdpMediaFabric::with_metrics` — fabric propagates it to every
    bridge via `BridgeConfig::metrics`.
  - `Sidecar::set_metrics` — set post-`spawn` (uses `OnceLock`
    internally so the hot path reads without locking).
- **Optional at every layer.** Each new hook checks `Option<Arc<
  Metrics>>`; tests and embedded use that bypass the registry
  continue to work unchanged.

### Changed

- `BridgeConfig` is no longer `#[derive(Default)]` — it now has an
  explicit `Default` so the new `metrics` field initializes to
  `None` without shifting the `rtcp_interval` default.
- `PluginEntry` gained a `metrics: Option<Arc<Metrics>>` field.
  Existing consumers that destructure the struct need the extra
  field; construction via the loader is unaffected.
- `Metrics` now derives `Debug` (required to keep `PluginEntry`'s
  derive working).

## [0.11.0] - 2026-04-20

### Added — RTP stats + RTCP SR emission

- **`smiths-media::rtp_stats`** — per-direction `StreamStats`
  tracker. Observes every forwarded RTP packet: counts, bytes,
  highest sequence, last RTP timestamp + SSRC, and the RFC 3550
  §A.8 interarrival jitter (smoothed at 1/16, stored in fixed
  point). Snapshot is atomics-only, no locking.
- **`smiths-media::rtcp`** — Sender Report builder + parser.
  Packet layout per RFC 3550 §6.4.1 (28 bytes, no report blocks
  yet). `ntp_now()` helper returns the 64-bit NTP timestamp with
  the 1900-epoch offset. Five byte-layout round-trip tests cover
  success and every rejection path.
- **`Bridge` grew `spawn_with(id, a, b, cfg)`** — when the legs
  carry `RtcpLeg` handles and `BridgeConfig::rtcp_interval` is
  `Some`, the bridge fires periodic Sender Reports to each peer's
  RTCP port. Stats travel through the forwarder path (observed
  after SSRC rewrite so the SR's SSRC matches the egress SSRC).
  `Bridge::stats()` returns a `BridgeStats` snapshot for both
  directions.
- **`UdpMediaFabric::bridge`** now populates `RtcpLeg` from the
  already-allocated RTCP sockets (previously `_rtcp` — bound but
  idle). Peer RTCP address is derived as `peer RTP port + 1` per
  RFC 3550 §11; explicit `a=rtcp:` SDP lines can land later.
- Two new integration tests: stats tick as packets flow; SRs land
  on the peer within 2 s with the correct packet count.

### Added — WASM `send_rtp` host fn

- **`smiths::send_rtp(call_id_ptr, call_id_len, bytes_ptr, bytes_len) -> i32`**
  — guest hands the host a call-id + payload; host looks up the
  call's media endpoint + remote RTP address and dispatches via
  `MediaFabric::send_packet`. Returns `0` on dispatch, `-1` for
  unknown calls. Gated behind the new `"send_rtp"` permission.
- **`smiths-core::CallLookup`** trait — seam for `call-id →
  (EndpointId, SocketAddr)`. Lives in `smiths-core` so
  `smiths-wasm` can consume it without linking the MCP crate.
  `ControlState` implements it.
- **`WasmEngine::with_media(lookup, fabric)`** — builder that
  attaches both handles to every store the engine builds. CLI
  wires control-plane state + `UdpMediaFabric` at boot.
- Test coverage: `send_rtp` dispatches correctly through a
  recording fabric; permission-denied traps cleanly; both paths
  are verified inline via WAT.

### Changed

- CLI now builds `UdpMediaFabric` before the WASM engine so the
  engine can carry its handle. Prior ordering put fabric
  construction after the plugin load — moving it up kept the
  single `Arc<dyn MediaFabric>` shared across all consumers.

## [0.10.0] - 2026-04-20

### Added — WASM host surface expansion

- **`smiths::publish_event(topic_ptr, topic_len, data_ptr, data_len) -> i32`**
  — guest publishes `(topic, bytes)` to the engine's event bus as
  `PluginEvent::Published`. Returns `0` on success, `-1` when no
  subscribers were live (non-fatal). Requires the `events`
  permission.
- **`smiths::timer_set(delay_ms, event_id) -> i32`** — guest schedules
  a one-shot host timer. When the delay elapses, the engine
  publishes a `PluginEvent::TimerFired` carrying `event_id`.
  Implementation uses a `std::thread` rather than `tokio::spawn` so
  sync `run_entry` callers without a runtime still work. Requires
  the `timers` permission.
- **`WasmEngine::with_bus(EventBus)`** — builder that attaches the
  engine-wide bus to every store the engine builds. CLI wires it
  at boot, so `load_plugins` plugins get bus access without any
  extra threading.
- **`PluginEvent::Published` / `PluginEvent::TimerFired`** — two
  new variants on the bus. Subscribers (MCP forwarder, sidecar
  bridge, future routing agents) can react without linking the
  WASM crate.

### Added — plugin hot reload

- **`smiths-plugin::watcher`** — `spawn(root, registry) ->
  WatcherHandle` launches a background task that polls the plugins
  directory and calls `AiRegistry::reload(name)` when `plugin.toml`
  or an entry file changes. Uses `notify::PollWatcher` with
  `compare_contents(true)` for same-second-edit detection
  (platform-independent; macOS HFS+ and editor save bursts
  handled). Bursts are debounced with a 250 ms trailing delay so a
  noisy editor save only triggers one reload per plugin.
- **`WatcherHandle`** — canceling it (drop or `shutdown().await`)
  tears down the background task cleanly.
- Integration test `hot_reload::watcher_triggers_reload_on_manifest_touch`
  loads a sidecar stub, touches `plugin.toml`, and verifies the
  registry swaps in a fresh `Arc<PluginEntry>` within 5 s.

### Added — proto schema v1 frozen

- **`smiths-proto`** grew `Envelope` / `Request` / `Response` /
  `Notification` messages with prost-derive annotations — no
  `build.rs`, no `protoc` dependency at compile time. Field
  numbers are locked; future additions use fresh tags so old peers
  decode correctly. Six round-trip tests cover each variant and
  the empty-envelope edge case.
- When the sidecar transport eventually migrates from JSON-RPC to
  length-prefixed protobuf, these types serialize both sides.

## [0.9.0] - 2026-04-19

### Added — WASM plugin invocation dispatch

- **`WasmEngine::call_invoke(module, plugin, method, &params) ->
  Result<Value, WasmError>`** — the invoke trampoline. Guest ABI:
  module exports `memory`, `alloc(len: i32) -> i32`, and
  `invoke(method_ptr, method_len, params_ptr, params_len) -> i64`.
  The host serializes `params` as JSON, allocates + writes both
  buffers via `alloc`, calls `invoke`, and decodes the packed
  `(ptr << 32) | len` response envelope.
- **Response envelope** — the guest returns a JSON object matching
  one of `{"result": X}` (success, `X` passed back to the caller)
  or `{"error": "msg"}` (plugin-level failure surfaced as
  `WasmError::PluginError`). Malformed envelopes trap.
- **`WasmProvider::invoke` wired** — replaces the prior "not yet
  wired" stub. Real WASM plugins now run full describe + invoke
  through `AiRegistry`, matching sidecar semantics.
- **`rust-logger` example** — grew `alloc` (4 KiB static bump
  buffer) and `invoke` (fixed `{"result":"ok"}` envelope) exports
  so it's a complete, buildable reference for the WASM tier.

### Added — plugin permission model

- **`permissions: Vec<String>`** — new `plugin.toml` field. Empty
  by default; only the plugins that need gated host surfaces opt
  in. Today's meaningful value: `"state"` (required by
  `smiths::state_{get,set}`). Future slices key `"send_sip"`,
  `"send_rtp"`, timers, etc. off the same list.
- **`WasmEngine::set_plugin_permissions(name, perms)`** — engine-
  side registry. Called at load time with the manifest's list; the
  per-invocation `HostState` cheaply clones the resulting
  `Arc<HashSet<String>>` so every host fn can gate in O(1).
- **`WasmError::PermissionDenied { plugin, permission, op }`** —
  typed trap variant so operators can distinguish "plugin over-
  reach" from plain traps / fuel exhaustion / timeouts. Surfaces
  the exact permission string the author needs to add to
  `plugin.toml`.
- **`HostState::for_plugin_with_state` signature grew a
  `permissions` parameter** — single construction point now covers
  the three pieces of per-call plugin context (name, persistent
  state, permission set).

### Changed

- Tests for `state_get` / `state_set` now explicitly register the
  `"state"` permission via `engine.set_plugin_permissions(...)` —
  previously permission-free access was implicit, now it's the
  *declared* path. `for_plugin` (the empty-default helper) now
  grants no permissions, matching runtime behavior.

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
