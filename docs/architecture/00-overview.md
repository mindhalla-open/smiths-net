# Architecture Overview

Project: **smiths-net** — lightweight, pluggable SIP engine with an AI-first control plane.

Based on the spec in `docs/openswitch.md`. Read that first.

## Goals

- **Lightweight**: single static binary, target < 20 MB, < 64 MB RSS at idle.
- **SIP + RTP**: RFC 3261 signaling, basic RTP/RTCP media per spec §1–2.
- **Pluggable in any language**: two-tier plugin system (WASM hot-path,
  sidecar control-path) with one unified hook ABI.
- **AI-first**: embedded Model Context Protocol (MCP) server so LLM agents
  can drive calls and plugins through well-typed tools and resources.
- **Scalable**: stateless core; state lives in call FSMs and plugin-owned
  stores; horizontal scaling via external SIP load balancer.
- **Maintainable**: small focused crates, schema-first IPC, no cyclic deps.

## Scope boundary: MVP vs post-MVP

Per `openswitch.md §7`, the following are **out of MVP**:

- Video calls, conferencing, mixing, real-time transcoding
- Complex dialplan / IVR, subscriber databases, HA / clustering
- ICE / STUN / TURN and full NAT traversal
- FAX (T.38), DTMF relay (RFC 2833 / inband)
- Distributed sessions, state replication, persistent storage

These are **not** hard non-goals — they are deferred. The architecture must
not actively preclude them. `04-post-mvp-scope.md` sketches the integration
path for each. Several (IVR / dialplan, subscriber lookup, DTMF detection)
land as **plugins** under the existing two-tier ABI — no core changes. The
rest are documented core extensions for post-v1 phases; see
`docs/plans/post-mvp.md`.

## Top-level component diagram

```
                  ┌───────────────────────────────────────┐
                  │               smiths-cli              │
                  │  (config · wiring · graceful shutdown)│
                  └──────────────┬────────────────────────┘
                                 │
       ┌─────────────┬───────────┼───────────┬───────────────┐
       ▼             ▼           ▼           ▼               ▼
  ┌─────────┐  ┌─────────┐  ┌─────────┐ ┌──────────┐   ┌───────────┐
  │smiths-  │  │smiths-  │  │smiths-  │ │smiths-   │   │smiths-mcp │
  │sip      │  │sdp      │  │media    │ │plugin    │   │(tools/res)│
  │(sig)    │  │         │  │(rtp)    │ │dispatcher│   │           │
  └────┬────┘  └────┬────┘  └────┬────┘ └────┬─────┘   └─────┬─────┘
       │            │            │           │               │
       └────────────┴────────────┴───────────┴───────────────┘
                           smiths-core
                  (event bus · FSM · tokio runtime)
                                 │
            ┌────────────────────┴────────────────────┐
            ▼                                         ▼
   ┌────────────────┐                        ┌─────────────────┐
   │  smiths-wasm   │                        │ smiths-sidecar  │
   │  (wasmtime)    │                        │ (subproc + IPC) │
   └────────────────┘                        └─────────────────┘
```

## Key design decisions

1. **Rust + `tokio` multi-thread runtime.** Proven for telecom, single-binary
   deploy, ecosystem for RTP/SIP/TLS.
2. **Event bus as the spine.** All subsystems publish/subscribe typed events
   (`SipEvent`, `MediaEvent`, `ControlEvent`, `PluginEvent`). Modules do not
   call each other directly outside the bus. Plugins become first-class
   without special cases.
3. **Two-tier plugins, one ABI.**
   - **WASM (wasmtime)** — for hot-path hooks (`on_rtp_frame`, inline SIP
     mutation) where latency and sandboxing matter. Any WASM-targeting
     language.
   - **Sidecar (child process + IPC)** — for control-path and AI hooks where
     you want true "any language" (Python, Node, Go, Java).
   - Same hook names, same payload schema (protobuf), same host-function
     surface.
4. **MCP as the AI front door.** LLMs never poke internals; they call typed
   tools and read typed resources. Audit and rate-limit live at this boundary.
5. **Schema-first IPC.** `smiths-proto` owns all wire types. WASM guests,
   sidecars, and MCP clients all consume the same schema.
6. **Fail-closed plugins.** Plugin trap or timeout never crashes the engine;
   the call continues with plugin output discarded and a metric incremented.

## Data flow: inbound call with plugins

1. `smiths-sip` receives `INVITE` over UDP/TCP/TLS; parses; creates server
   transaction.
2. Emits `SipEvent::InviteReceived{call_id, headers, sdp}`.
3. `smiths-plugin` dispatcher runs `on_sip_request` hooks in priority order
   (WASM + sidecar). Each may mutate headers/body or veto.
4. On accept, `smiths-core` creates a Call FSM task (`Idle → Early`).
5. `smiths-sdp` negotiates codecs; `smiths-media` allocates an RTP session
   from the configured port range.
6. Engine sends `200 OK` with SDP answer; state moves to `Confirmed`.
7. `smiths-media` pumps RTP. Each frame can be tapped by WASM
   `on_rtp_frame`. Sidecars do **not** receive per-frame events by default
   (latency gate); they can opt in via manifest.
8. On `BYE` or timeout, FSM enters `Terminated`, resources freed,
   `on_call_state_change` fired on all plugins.

## Runtime / threading model

- Single multi-thread `tokio` runtime with `N = num_cpus` workers by default.
- One task per bound SIP socket reads the transport.
- One FSM task per call, lifetime = dialog lifetime.
- WASM invocations run on `tokio::task::spawn_blocking`; each call owns its
  `wasmtime::Store` with fuel metering and epoch-based interruption.
- Each sidecar plugin is supervised by one task managing its child process
  and IPC framing.

## Where this diverges from `openswitch.md`

The spec is followed. This overview adds:

- Explicit **two-tier** plugin model (spec only specified WASM).
- Event bus as an explicit architectural invariant.
- Protobuf as the single wire schema across WASM + sidecar + MCP.

Everything else (hooks, MCP tools, non-goals, stack) stays as specified.
