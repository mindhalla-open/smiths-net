# rust-logger — WASM plugin skeleton

Minimal example that calls the engine's `smiths::log` host import. A
working smoke test for the `smiths-wasm` walking skeleton — when
richer host surface lands (`send_sip`, `send_rtp`, timers, state),
those imports get added alongside.

## Build

Needs the `wasm32-unknown-unknown` target:

```sh
rustup target add wasm32-unknown-unknown
cargo build -p rust-logger --target wasm32-unknown-unknown --release
```

The output `.wasm` lands at:

```
target/wasm32-unknown-unknown/release/rust_logger.wasm
```

## Run against the engine

No loader integration yet — the WASM manifest tier lands after the
walking skeleton. For now, drive it directly through the
`WasmEngine::run_entry` API:

```rust
use smiths_wasm::WasmEngine;

let bytes  = std::fs::read("target/wasm32-unknown-unknown/release/rust_logger.wasm")?;
let engine = WasmEngine::new()?;
let module = engine.load(&bytes)?;
engine.run_entry(&module, "run", 1_000_000, "rust-logger")?;
// tracing::info! picks up the greeting via the host's log sink.
```

## Exports

| symbol | arity       | notes                                                  |
|--------|-------------|--------------------------------------------------------|
| `run`  | `() -> ()`  | Calls `smiths::log("rust-logger: hook fired")`.        |

## Imports

| import              | signature                                         |
|---------------------|---------------------------------------------------|
| `smiths.log`        | `(ptr: i32, len: i32)` — host reads UTF-8 slice.  |

## Not in scope for the walking skeleton

- `send_sip` / `send_rtp` / timers / state / events — future host fns.
- WASM plugin manifests and loader integration (today's loader is
  sidecar-only).
- `tinygo-hdr` example — parallel structure, needs a Go + `tinygo`
  toolchain; lands when the rest of Phase 3 does.
