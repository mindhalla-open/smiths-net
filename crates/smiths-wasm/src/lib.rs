//! WASM plugin host: `wasmtime` engine, guest ABI, host functions,
//! fuel, memory caps, and epoch-based wall-clock deadlines.
//!
//! A single [`WasmEngine`] compiles modules, wires the `smiths::*`
//! host imports (`log`, budgeted `state_{get,set}`, `publish_event`,
//! `timer_set`, `send_rtp` with a bounded outbound queue, `originate`
//! / `hangup`), checks every gated import against the plugin's
//! declared permissions, and runs guest exports under a fuel budget,
//! a linear-memory cap, and a wall-clock deadline. Host traps, fuel
//! exhaustion, deadline hits, resource-cap hits, and guest bugs are
//! surfaced as typed [`WasmError`] variants — the engine itself is
//! never killed. Entry points are synchronous; async callers run
//! them on a blocking pool.

pub mod engine;
pub mod error;
pub mod host;

pub use engine::{
    DEFAULT_FUEL, DEFAULT_INVOKE_TIMEOUT, DEFAULT_MEMORY_LIMIT_BYTES, EPOCH_TICK, WasmEngine,
};
pub use error::WasmError;
pub use host::{
    DEFAULT_STATE_BUDGET_BYTES, HostState, PluginState, PluginStore, RTP_QUEUE_CAPACITY, RtpQueue,
    SEND_RTP_DROPPED, StateBudgetExceeded,
};

// Re-export so downstream crates (`smiths-plugin`) don't need to pull
// `wasmtime` into their dep graph just to carry a `Module`.
pub use wasmtime::Module;
