# Phase 4 — Sidecar Plugins

**Goal**: run plugins as supervised child processes in any language. Same
hook ABI as WASM, but over length-prefixed protobuf on stdio. Ship a Python
AI example.

## Deliverables

1. `smiths-sidecar` crate with:
   - Child-process supervisor (spawn, restart policy, health ping).
   - Length-prefixed protobuf stdio framing (frame format in
     `architecture/02-plugin-system.md §Sidecar wire protocol`).
   - Host-call router (plugin → core → reply).
2. `smiths-plugin::Dispatcher` integrated to route hook calls to WASM or
   sidecar based on manifest `type`.
3. Example plugin `plugins/examples/py-ai/`:
   - Python 3.11+ script using `protobuf` library.
   - Subscribes to `on_call_state_change`.
   - Calls a mocked "AI" stub (just text echo) and logs via `host::log`.
4. Optional gRPC-over-UDS transport behind `sidecar-grpc` feature.

## Step-by-step tasks

1. **Supervisor** (`smiths-sidecar::supervisor`)
   - `spawn()`: fork child, attach stdin/stdout, set working dir,
     forward stderr to engine logger with `plugin=<name>` tag.
   - Watchdog: send `Ping` every 5 s; mark unhealthy if no `Pong` in 15 s;
     restart per policy.
   - Backoff: exponential 100 ms → 30 s.
2. **Stdio framing** (`smiths-sidecar::codec`)
   - Encoder: `tokio_util::codec::LengthDelimitedCodec` with big-endian
     u32 length.
   - Decoder: same; max frame 4 MB (configurable).
3. **Host-call router**
   - Sidecar sends `HostCall{seq, op, args}`; core executes; reply with
     `HostReturn{seq, result}`.
   - Per-sidecar outbound queue (bounded 1024); drop-oldest for
     non-critical frames.
4. **Dispatcher integration**
   - `smiths-plugin::Plugin` enum: `Wasm(WasmPlugin)`, `Sidecar(SidecarPlugin)`.
   - `dispatch(hook, payload)` picks the right backend.
   - `on_rtp_frame` routed to sidecar only when `allow_rtp_tap = true`;
     logs a warning at load time.
5. **Python example** (`plugins/examples/py-ai`)
   - `plugin.toml` with `type="sidecar"`, `entry="./main.py"`.
   - `main.py` reads frames from stdin, decodes with generated
     `plugin_pb2`, dispatches to Python handlers, writes replies.
   - `requirements.txt` pinned; a small `Makefile` to build the proto.
6. **Optional gRPC transport** (feature-gated)
   - `tonic` server in-process, UDS socket, one service
     `PluginRuntime` with bidi streaming.
   - Same proto message set.
7. **Tests**
   - `sidecar_lifecycle.rs` — spawn, health-ping, kill, restart.
   - `sidecar_hook_dispatch.rs` — Python echo plugin responds to a SIP
     event within budget.
   - `sidecar_crash_isolation.rs` — Python script exits mid-call; call
     continues; sidecar restarts; next call gets new plugin.
   - `sidecar_backpressure.rs` — slow plugin; core drops non-critical
     frames without blocking the call.

## Acceptance criteria

- [ ] `py-ai` loads on startup and logs `"call confirmed"` on every
  established call.
- [ ] Killing the Python child mid-call does not affect the call; the
  engine restarts the child within 5 s per `restart_policy="on-failure"`.
- [ ] Sidecar-call p99 latency on `on_call_state_change` ≤ 10 ms on a dev
  laptop.
- [ ] `sidecar-grpc` feature builds and passes the same test suite.
- [ ] ABI (proto schema) unchanged since phase 3 — no breakage to WASM
  examples.

## Out of scope

- NATS transport (tracked in backlog).
- Shared-memory ring for sidecar RTP tap (tracked in backlog, v2).
- Resource limits (cgroups / rlimit) — documented, not enforced in v1.

## Risks & notes

- Python's `asyncio` + stdin framing needs care; provide a reference loop
  in `main.py`.
- Do not let a slow sidecar stall sync hooks: timeouts are mandatory.
- Document the exact proto versions used by the example so users can pin.
