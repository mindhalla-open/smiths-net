# Crate Layout

Single Cargo workspace. Each crate is focused, independently testable, and
publishable on its own.

## Directory layout

```
smiths-net/
├── Cargo.toml                 # [workspace] only
├── crates/
│   ├── smiths-core/           # runtime, event bus, config, shutdown, Call FSM
│   ├── smiths-proto/          # protobuf types shared by core/WASM/sidecars
│   ├── smiths-sip/            # SIP parser, transport, transactions, dialogs, auth
│   ├── smiths-sdp/            # SDP parse/generate, offer/answer negotiation
│   ├── smiths-media/          # RTP/RTCP session, jitter buffer, media router
│   ├── smiths-plugin/         # manifest, registry, dispatcher, hook trait
│   ├── smiths-wasm/           # wasmtime host, ABI bindings, host functions
│   ├── smiths-script/         # embedded DSL runtime (Rhai/Lua/Starlark), host-fn bridge
│   ├── smiths-sidecar/        # subprocess supervisor, IPC framing
│   ├── smiths-mcp/            # MCP server (stdio + HTTP/SSE)
│   ├── smiths-cli/            # main binary: CLI, config loading, wiring
│   └── smiths-testkit/        # integration helpers: fake UAC/UAS, pcap cmp
├── plugins/
│   ├── examples/
│   │   ├── rust-logger/       # WASM plugin in Rust
│   │   ├── tinygo-hdr/        # WASM plugin in TinyGo
│   │   └── py-ai/             # sidecar plugin in Python
│   └── README.md
├── proto/                     # .proto sources; compiled by smiths-proto build.rs
└── docs/
    ├── openswitch.md          # original spec
    ├── architecture/
    └── plans/
```

## Dependency graph (no cycles)

```
cli ─┬─ core
     ├─ sip ──── core
     ├─ sdp ──── core               (impls core::sdp::SdpNegotiator)
     ├─ media ── core               (impls core::media::MediaFabric)
     ├─ plugin ─┬─ core             (impls core::ai::{AiProvider, AiRegistry})
     │          ├─ wasm ──── core + proto
     │          ├─ script ── core
     │          └─ sidecar ─ core
     └─ mcp ──── core               (consumes core::ai trait seam)
```

**Layering invariant.** Every non-root crate depends on `smiths-core`
(and `smiths-proto` where wire types are needed), with **one documented
exception**: `smiths-plugin` owns its host tiers and directly depends
on `smiths-wasm`, `smiths-script`, and `smiths-sidecar`. No other
crate reaches sideways into a sibling.

The dependency-inversion seam makes this work: shared concerns that
touch multiple subsystems (event bus, config, media fabric, SDP
negotiation, call state, AI-plugin descriptors + registry) live as
traits + pure data types in `smiths-core`, and host crates implement
them. `smiths-cli` wires concrete implementations at startup.

Concretely:

- `smiths-sip` depends on `smiths-core` only — it takes
  `Arc<dyn MediaFabric>` and `Arc<dyn SdpNegotiator>` at construction,
  so it never links `smiths-sdp` or `smiths-media` and never touches
  a socket beyond its own signaling transport.
- `smiths-sdp` depends on `smiths-core` to impl `SdpNegotiator` and
  re-export the `NegotiationOutcome` enum, but it has no coupling in
  the other direction.
- `smiths-media` depends on `smiths-core` to impl `MediaFabric` and
  return `EndpointId` / `BridgeId` tokens; it knows nothing about SIP
  or SDP.
- `smiths-mcp` depends on `smiths-core` only — it consumes
  `Arc<dyn AiRegistry>` and calls plugins through `AiProvider::invoke`,
  so it never links `smiths-plugin` and stays a pure tool/adapter layer.
- `smiths-plugin` is the umbrella for the three host tiers. It
  implements `core::ai::{AiProvider, AiRegistry}` over its sidecar
  (today) / WASM (Phase 3) / script (Phase 3) backends and re-exports
  each host crate as a sub-namespace (`plugin::sidecar`, `plugin::wasm`,
  `plugin::script`).
- Integration tests and the CLI are **allowed** to reach across
  siblings (they wire concrete implementations) — that is the one
  place the layering "flattens" on purpose.

## Crate responsibilities

| Crate            | Responsibility                                                            | Key external deps                        |
|------------------|---------------------------------------------------------------------------|------------------------------------------|
| `smiths-core`    | tokio runtime, typed event bus, config loader (+ `BindSpec`), graceful shutdown, Call FSM state (`DialogRecord`, `Serialize`), media/sdp **trait seams** (`MediaFabric`, `SdpNegotiator`, `EndpointId`, `BridgeId`, `NegotiationOutcome`), AI-plugin **trait seams** (`CapabilityDescriptor`, `validate_controls`, `AiProvider`, `AiRegistry`), timer wheel | `tokio`, `async-trait`, `serde`, `serde_json`, `tracing`, `toml`, `figment` |
| `smiths-proto`   | `.proto` → Rust types; build-time codegen; schema versioning              | `prost`, `prost-build`                   |
| `smiths-sip`     | RFC 3261 parser/serializer, UDP/TCP/TLS transport, transaction + dialog FSMs, digest auth. Consumes `MediaFabric` + `SdpNegotiator` trait objects; **no** direct deps on `smiths-sdp` / `smiths-media` | `rsip` or custom `nom`, `rustls`, `tokio-rustls` |
| `smiths-sdp`     | SDP parse/generate, codec negotiation (PCMU, PCMA, Opus). Provides `Negotiator: SdpNegotiator` impl consumed through `smiths-core` | `webrtc-sdp` or custom                   |
| `smiths-media`   | RTP/RTCP sockets, SSRC mgmt, passthrough router, optional jitter buffer. Provides `UdpMediaFabric: MediaFabric` impl; owns all media sockets behind opaque tokens | `webrtc-rtp`, `webrtc-rtcp`, `dashmap`   |
| `smiths-plugin`  | Manifest loader, plugin registry, dispatcher, priority ordering, hot-reload. Umbrella for the three host tiers (re-exports `smiths-sidecar` / `smiths-wasm` / `smiths-script`). Implements `core::ai::{AiProvider, AiRegistry}` | `serde`, `dashmap`                     |
| `smiths-wasm`    | wasmtime engine, guest ABI, host functions, fuel/epoch interruption       | `wasmtime`, `wasmtime-wasi`              |
| `smiths-script`  | Embedded DSL runtime for script-tier plugins (Rhai default; Lua/Starlark via feature); host-function bridge shared with `smiths-wasm`; op-count + wall-clock budgets; live file reload | `rhai`, optional `mlua` / `starlark-rust`, `notify` |
| `smiths-sidecar` | Child process supervisor, JSON-RPC 2.0 IPC over stdio (newline-delimited). gRPC/UDS option under a feature flag. JSON was chosen over protobuf so Python/Node plugin authors don't need `protoc` — switching wire formats later is an impl swap, not a protocol change | `tokio`, `serde_json`, `tonic` (optional) |
| `smiths-mcp`     | Control plane: adapter-agnostic `Tool` / `Resource` traits plus MCP (stdio) and A2A (HTTP/JSON-RPC) adapters. Consumes `Arc<dyn AiRegistry>` from `smiths-core::ai`; does **not** depend on `smiths-plugin` | `axum`, `tower`                         |
| `smiths-cli`     | `main.rs`, `clap` CLI, config loading, signal handling, wiring            | `clap`, `tracing-subscriber`             |
| `smiths-testkit` | Test helpers: fake UAC/UAS, pcap capture/compare, call assertions         | `tokio-test`                             |

## Workspace `Cargo.toml` (sketch)

```toml
[workspace]
resolver = "3"
members = ["crates/*", "plugins/examples/rust-logger"]

[workspace.package]
edition      = "2024"
rust-version = "1.95"
license      = "Apache-2.0"

[workspace.dependencies]
tokio        = { version = "1", features = ["full"] }
tracing      = "0.1"
serde        = { version = "1", features = ["derive"] }
prost        = "0.13"
wasmtime     = "26"
# ... etc, pinned here so crates share versions
```

## Public API boundaries

- `smiths-core::bus` — only mechanism for cross-crate communication.
- `smiths-core::config` — all config types re-exported from here.
- `smiths-core::ai` — single source of truth for the AI-plugin contract:
  `CapabilityDescriptor`, `validate_controls`, and the `AiProvider` /
  `AiRegistry` trait seams. `smiths-plugin` implements them;
  `smiths-mcp` consumes them.
- `smiths-proto` — the *only* module that defines wire types. Other crates
  re-export where convenient but do not define overlapping types.

## Feature flags

| Feature          | Where         | Effect                                                       |
|------------------|---------------|--------------------------------------------------------------|
| `tls`            | `smiths-sip`  | Include `rustls` + TLS transport. On by default.             |
| `mcp-http`       | `smiths-mcp`  | Enable HTTP/SSE transport (adds `axum`).                     |
| `sidecar-grpc`   | `smiths-sidecar` | Enable gRPC transport (adds `tonic`). Off by default.     |
| `script-rhai`    | `smiths-script` | Default embedded DSL engine (Rhai). On by default.         |
| `script-lua`     | `smiths-script` | Lua 5.4 via `mlua`. Off by default; mutually exclusive with other `script-*`. |
| `script-starlark`| `smiths-script` | Starlark via `starlark-rust`. Off by default; mutually exclusive. |
| `pcap`           | `smiths-cli`  | Enable pcap capture tool. Off by default.                    |
| `examples-rust`  | workspace     | Build example Rust WASM plugin.                              |

Goal: a minimal build with `--no-default-features` produces a <12 MB binary
with UDP-only SIP and stdio MCP.

## Test layout

- Unit tests in each crate under `src/`.
- Integration tests in `crates/<crate>/tests/`.
- End-to-end tests in `crates/smiths-testkit/tests/` — spin the binary,
  drive it with a fake UAC, assert pcap and event log.
- Plugin tests in `plugins/examples/*/tests/` using `smiths-testkit`.
