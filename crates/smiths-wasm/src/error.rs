//! Error surface for [`crate::WasmEngine`].
//!
//! As of wasmtime 43 the runtime exposes its own `wasmtime::Error`
//! type (not a re-export of `anyhow::Error`), so our variants carry
//! that directly. Each variant preserves the underlying cause so
//! operators can see exactly where compilation, linking, or execution
//! went wrong.

use thiserror::Error;

/// Errors raised when building or running a WASM plugin.
#[derive(Debug, Error)]
pub enum WasmError {
    /// `wasmtime::Engine::new` failed (almost always a config bug).
    #[error("wasm engine init: {0}")]
    Engine(#[source] wasmtime::Error),
    /// Compilation of the guest module's bytes failed (not valid WASM,
    /// unsupported features, etc.).
    #[error("wasm module compile: {0}")]
    Compile(#[source] wasmtime::Error),
    /// `Linker` failed to resolve imports — the guest asked for a
    /// symbol we did not provide.
    #[error("wasm link: {0}")]
    Link(#[source] wasmtime::Error),
    /// Module did not export the expected entry point.
    #[error("wasm instance missing export `{0}`")]
    MissingExport(String),
    /// The guest hit `unreachable`, a host-function failure, or some
    /// other trap — the instance is dead but the engine survives.
    #[error("wasm trap: {0}")]
    Trap(#[source] wasmtime::Error),
    /// Caller-supplied fuel budget was consumed before the guest
    /// finished. Distinct from [`Self::Trap`] so operators can rate-
    /// limit noisy plugins without also panicking on their bugs.
    #[error("wasm fuel exhausted after {fuel} units")]
    FuelExhausted {
        /// Fuel budget that was set for this invocation.
        fuel: u64,
    },
    /// Guest ran past the wall-clock deadline set by
    /// [`crate::WasmEngine::run_with_deadline`] and tripped the
    /// epoch interruption.
    #[error("wasm deadline exceeded after {millis} ms")]
    Timeout {
        /// Deadline configured for the invocation.
        millis: u64,
    },
    /// `Store::set_fuel` / fuel setup failed.
    #[error("wasm fuel configuration: {0}")]
    Fuel(#[source] wasmtime::Error),
}
