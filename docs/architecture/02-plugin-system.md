# Plugin System

Two tiers (in-process and out-of-process), one ABI. The in-process tier
has two variants — compiled WASM and interpreted DSL scripts — that share
the same sandbox, host-function surface, and hook set. Plugin authors pick
based on latency needs, language choice, and whether they want a build
step. The core treats all three uniformly through the dispatcher.

## Tier A1 — WASM (in-process, compiled, sandboxed)

**Use when**: hot-path hooks, per-RTP-frame processing, header mutation on
the critical path, deterministic low latency, strict sandboxing.

- Runtime: `wasmtime`.
- Isolation: no host filesystem, no sockets, no clock beyond `host_now_ms`.
- Limits: 64 MB memory cap, CPU fuel per call (configurable), epoch
  interruption for wall-clock deadlines.
- Languages: Rust, TinyGo, C/C++, AssemblyScript, Zig. (Python/Node in WASM
  are heavy — prefer sidecar for those.)
- Packaging: `<name>.wasm` + `plugin.toml` in `plugins/<name>/`.

## Tier A2 — Embedded DSL scripts (in-process, interpreted, sandboxed)

**Use when**: dialplan and routing rules, per-request policy, header / URI
rewrite snippets, A/B experiments, live operator hotfixes — logic that is
painful as WASM (toolchain + recompile per edit) and fragile as a sidecar
(runtime drift, dependency hell).

- Runtime: `rhai` is the default; `mlua` (Lua 5.4) and `starlark-rust` are
  selectable via compile-time features. Exactly one is linked in a given
  build.
- Isolation: identical guarantees to WASM — no filesystem, no sockets, no
  blocking calls. Host functions gated by the same `permissions` list on
  the same Rust closures; the script engine is just another frontend.
- Limits: per-call op-count budget (default 100_000 operations) plus a
  wall-clock deadline (default 500 µs for hot-path hooks, 5 ms for
  control-path). Exceeding either yields `Err(Budget)` and the hook's
  output is discarded (fail-closed).
- Hot reload: editing a `.rhai` / `.lua` / `.star` file on disk (or pushing
  one via MCP) swaps it live — no rebuild, no restart. Old script drains
  in-flight calls; new script picks up the next one.
- Not for: `on_rtp_frame`. Scripts are an order of magnitude slower than
  WASM; per-frame hooks stay on Tier A1.
- Packaging: `<name>.{rhai,lua,star}` + `plugin.toml` in `plugins/<name>/`.

Mental model: **a script is a WASM plugin you didn't have to cross-compile.**
It trades raw speed for edit-latency and author accessibility — ideal for
the control-plane layer where rules change often and must be readable by
non-Rust operators (and by LLMs via MCP).

## Tier B — Sidecar (out-of-process, any language)

**Use when**: control-plane hooks, AI inference, heavy native libraries,
need for unrestricted I/O.

- Runtime: a child process supervised by `smiths-sidecar`.
- Transport:
  - **default**: length-prefixed Protobuf over stdio.
  - **opt-in**: gRPC over Unix domain socket (feature `sidecar-grpc`).
  - **future**: NATS subject pair for multi-node plugins.
- Languages: anything with a protobuf library — Python, Node, Go, Java, C#,
  Ruby, Rust, ...
- Packaging: any executable + `plugin.toml` in `plugins/<name>/`.

## Unified hook set

Identical semantics across tiers. Payload schema is the same protobuf type.

| Hook                    | Direction   | Supported tiers             | Sync/async characteristics |
|-------------------------|-------------|-----------------------------|----------------------------|
| `on_init`               | core → plug | wasm, script, sidecar       | sync; plugin may return fatal error and refuse to start |
| `on_sip_request`        | core → plug | wasm, script, sidecar       | sync; WASM budget 1 ms, script budget 500 µs, sidecar budget 20 ms; on timeout output is discarded |
| `on_sip_response`       | core → plug | wasm, script, sidecar       | sync; same budgets |
| `on_sdp_offer`          | core → plug | wasm, script, sidecar       | sync |
| `on_sdp_answer`         | core → plug | wasm, script, sidecar       | sync |
| `on_rtp_frame`          | core → plug | **wasm** (sidecar opt-in)   | sync; per-frame; budget ~200 µs. **Script tier not supported** — too slow. |
| `on_call_state_change`  | core → plug | wasm, script, sidecar       | async, fire-and-forget |
| `on_timer`              | core → plug | wasm, script, sidecar       | async |
| `on_shutdown`           | core → plug | wasm, script, sidecar       | async; drain deadline 5 s |

Sidecars opting into `on_rtp_frame` get an explicit warning at load time.
v1 ships stdio framing for this; v2 may add a shared-memory ring. The
script tier cannot opt into `on_rtp_frame` at all — the dispatcher rejects
such manifests at load time.

## Manifest (`plugin.toml`)

```toml
name        = "ai-transcriber"
version     = "0.2.0"
type        = "sidecar"                # "wasm" | "script" | "sidecar"
entry       = "./bin/transcriber"      # .wasm | .rhai/.lua/.star | executable
hooks       = ["on_call_state_change", "on_rtp_frame"]
priority    = 50                        # 0..=100, lower runs first

# sidecar-only
allow_rtp_tap = true
restart_policy = "on-failure"           # "always" | "on-failure" | "never"
transport      = "stdio"                # "stdio" | "grpc-uds"

# script-only
script_engine  = "rhai"                 # "rhai" | "lua" | "starlark"

# declared permissions; core rejects host calls outside this set
permissions = ["send_sip", "set_timer", "store_plugin_state"]

[resources]
memory_mb         = 128
cpu_fuel_per_call = 10_000_000          # wasm only
ops_per_call      = 100_000             # script only (op-count budget)
wall_clock_us     = 500                 # script only (wall-clock deadline)
```

## Host-function surface (syscalls)

Identical in WASM imports and sidecar RPCs:

- `log(level, msg)`
- `send_sip(msg)` — inject a SIP request/response
- `send_rtp(leg, frame)` — inject an RTP frame on a media leg
- `set_timer(id, ms) → handle`
- `cancel_timer(handle)`
- `get_call_meta(call_id, key) → value`
- `set_call_meta(call_id, key, value)`
- `plugin_state_get(key)` / `plugin_state_put(key, value)` (per-plugin scope)
- `emit_event(topic, payload)` — cross-plugin bus
- `subscribe_event(topic)` — set in manifest, delivered via `on_event`

Each call passes through a permission check against the manifest. Denied
calls return `Err(Permission)` and emit a warning.

## Lifecycle

```
discover (scan plugins dir)
  → validate manifest + signature
  → spawn/instantiate
  → on_init
  → subscribe to declared hooks
  → (serve events)
  → unload: stop accepting new → drain in-flight → on_shutdown → kill/drop
```

**Hot reload**: the new version instantiates in parallel. Old instance
continues serving in-flight calls. New calls route to new version. Old
drains then unloads. No dropped calls.

**Crash policy**:
- WASM trap → plugin marked faulted for that call only; next call gets a
  fresh store. After 5 consecutive traps the plugin is disabled and an
  alert is emitted.
- Script error or budget exhaustion → same per-call fault semantics as a
  WASM trap. After 5 consecutive errors the script is disabled; the
  previous revision (if any) is kept as the hot-reload rollback target.
- Sidecar crash → supervisor follows `restart_policy` with exponential
  backoff (100 ms → 30 s).

## Sidecar wire protocol (v1, stdio)

Length-prefixed Protobuf frames:

```
  4 bytes  big-endian uint32 length
  N bytes  Protobuf-encoded Frame
```

```proto
// smiths-proto/proto/plugin.proto
message Frame {
  uint64 seq = 1;
  oneof kind {
    HookCall   call    = 10;   // core → plugin, needs reply with matching seq
    HookReturn ret     = 11;   // plugin → core
    HostCall   host    = 12;   // plugin → core (syscall), needs reply
    HostReturn host_r  = 13;   // core → plugin
    LogLine    log     = 20;   // plugin → core, fire-and-forget
    Ping       ping    = 30;   // bidirectional health
    Pong       pong    = 31;
  }
}
```

Backpressure: stdio pipe naturally provides it. Core maintains a per-plugin
outbox of bounded size; overflow drops oldest non-critical frames and
increments a metric.

## Versioning

- ABI version is baked into `plugin.toml` (`abi = "1.0"`) and checked at load.
- Proto schema evolves only additively within a major ABI.
- Breaking changes bump major; loader supports the previous major during a
  deprecation window.

## Security posture

- **WASM**: no host capabilities by default; each capability must be
  explicitly declared in `permissions`.
- **Script**: identical capability model to WASM — the script engine is
  instantiated with an empty host-function table; only the closures
  matching declared `permissions` are registered. A script loaded with
  `permissions = []` is pure computation over its argument payload and
  can do nothing else.
- **Sidecar**: runs as the engine user by default. Config can pin a
  dedicated uid/gid, apply seccomp/AppArmor profile, or launch in a
  namespace (`unshare`). Documented but not enforced by the engine in v1.
- **Signing** (v2): plugins may be cosign-signed; the loader refuses
  unsigned plugins when `plugins.require_signature = true`.
