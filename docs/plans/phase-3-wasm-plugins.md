# Phase 3 — WASM Plugins

**Goal**: loadable WASM plugins with the full hook set, the host-function
surface from `architecture/02-plugin-system.md`, and two working examples
(Rust and TinyGo).

## Deliverables

1. `smiths-proto` compiles protobuf to Rust + ships generated files for
   Rust/Go/Python into `proto/gen/`.
2. `smiths-plugin` crate with:
   - Manifest loader (`plugin.toml`).
   - Registry (per-plugin state, permissions).
   - Dispatcher invoking hooks by priority with per-hook timeout.
3. `smiths-wasm` crate with:
   - `wasmtime::Engine` shared; per-call `Store` with fuel + epoch
     interruption.
   - Host functions exposed to guests per
     `architecture/02-plugin-system.md §Host functions`.
   - Guest ABI as protobuf bytes via linear-memory pointer + length.
4. Example plugins under `plugins/examples/`:
   - `rust-logger` — logs every SIP method and call state.
   - `tinygo-hdr` — adds a custom `X-Engine` header on outgoing requests.
5. MCP-less developer CLI: `smiths-cli plugin load <path>` for manual
   testing (deleted/replaced by MCP in phase 5).

## Step-by-step tasks

1. **Proto schema freeze v1** (`smiths-proto`)
   - `SipMessage`, `SdpSession`, `RtpFrame`, `CallMeta`, `HookCall`,
     `HookReturn`, `HostCall`, `HostReturn`.
   - Generate Rust via `prost-build`.
   - Generate Go + Python into `proto/gen/` via `protoc`.
2. **Plugin crate**
   - `Manifest` struct, deserialized from `plugin.toml`.
   - `Registry` keyed by `plugin_id = hash(name + version)`.
   - `Dispatcher` with:
     - Per-hook priority ordering (stable sort by manifest `priority`).
     - Per-hook budget (`1ms` WASM, `20ms` sidecar).
     - Failure isolation: trap/timeout → result discarded, metric++.
3. **WASM host**
   - `wasmtime::Engine` configured with `epoch_interruption` and
     `consume_fuel`.
   - `Store` created per call for `on_rtp_frame` / `on_sip_request`;
     reused within a call to amortize JIT.
   - Linear-memory I/O: guest exposes `alloc(size) → ptr`; host writes
     protobuf bytes at `ptr`; calls export `on_<hook>(ptr, len) → ptr2,
     len2`; reads result, frees via `dealloc`.
4. **Host function bindings**
   - Each host func is a `wasmtime::Func` checking permissions before
     dispatch.
   - All host funcs go through `smiths-core::bus` or a dedicated syscall
     router; none touch transports directly.
5. **RTP hook path**
   - Fast path: if no plugin subscribes to `on_rtp_frame`, router forwards
     with zero copy.
   - Slow path: serialize the frame to proto, invoke plugin, deserialize
     result, forward.
6. **Example plugins**
   - `rust-logger`: `cdylib` + `wasm32-unknown-unknown` target; uses a
     tiny helper crate `plugins/sdk-rust` with the proto types and
     `host::log`, `host::send_sip` imports.
   - `tinygo-hdr`: TinyGo build, imports the proto, mutates Via header.
7. **Hot reload**
   - File watcher on `plugins.dir`; on manifest change, load v2 alongside
     v1, migrate new calls to v2, drain v1.
8. **Tests**
   - `hook_dispatch_order.rs` — two plugins with different priorities.
   - `wasm_trap_isolation.rs` — plugin that panics; call proceeds.
   - `rtp_mutation.rs` — plugin drops every 3rd frame; verify counts.
   - Example plugins each ship a small integration test.

## Acceptance criteria

- [ ] `rust-logger` loads from `plugins/examples/rust-logger/` at startup
  and logs every `INVITE`/`BYE`.
- [ ] `tinygo-hdr` adds `X-Engine: smiths-net` to outgoing requests.
- [ ] A plugin that calls `panic!()` does not crash the engine; call
  completes; `smiths_plugin_errors_total{kind="trap"}` increments.
- [ ] A plugin that spins forever is interrupted within the configured
  fuel/epoch budget; call completes.
- [ ] `on_rtp_frame` hook at 50 pps imposes < 100 µs p50 overhead per
  frame on a dev laptop.
- [ ] Hot reload of `rust-logger` during an active call drops zero
  packets.

## Out of scope

- Sidecar plugins (phase 4).
- MCP tools for plugin management (phase 5).
- Plugin signing/verification.

## Risks & notes

- `wasmtime` upgrades sometimes change epoch API — pin the version in
  workspace deps.
- Proto schema v1 *must* be stable before writing examples; breaking it
  forces rebuild of all guests.
- Keep the Rust guest SDK (`plugins/sdk-rust`) tiny — do not ship a
  framework; it's a proto re-export + import stubs.
