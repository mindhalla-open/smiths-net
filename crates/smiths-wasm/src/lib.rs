//! WASM plugin host: `wasmtime` engine, guest ABI, host functions,
//! fuel and epoch interruption.
//!
//! Phase 3 walking skeleton: a single [`WasmEngine`] that compiles
//! modules, wires the `smiths::log` host import, and runs a named
//! export to completion under a caller-supplied fuel budget. Host
//! traps, fuel exhaustion, and guest bugs are surfaced as typed
//! [`WasmError`] variants — the engine itself is not killed. Hot
//! reload, richer host surface (`send_sip`, `send_rtp`, timers,
//! state, events), and permission checks against the plugin manifest
//! land in follow-up passes.

pub mod engine;
pub mod error;
pub mod host;

pub use engine::{DEFAULT_FUEL, WasmEngine};
pub use error::WasmError;
pub use host::HostState;
