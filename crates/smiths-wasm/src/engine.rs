//! Wasmtime engine wrapper with per-call fuel + epoch interruption.
//!
//! `WasmEngine` is cheap to clone (it holds an `Arc`-backed
//! `wasmtime::Engine`). The usual flow is:
//!
//! ```ignore
//! let engine  = WasmEngine::new()?;
//! let module  = engine.load(wasm_bytes)?;
//! engine.run_entry(&module, "on_call", 1_000_000, "rust-logger")?;
//! ```
//!
//! `run_entry` instantiates a fresh [`wasmtime::Store`] per call
//! (per-call isolation), wires the `smiths::*` host imports, and
//! invokes the named exported function (arity `() -> ()` for now).

use wasmtime::{Config, Engine, Linker, Module, Store, Trap};

use crate::error::WasmError;
use crate::host::{HostState, register};

/// Default per-call fuel budget. Guest is a small event handler, not
/// a compute workload — a million units is generous.
pub const DEFAULT_FUEL: u64 = 1_000_000;

/// Wasmtime engine configured for smiths plugins.
#[derive(Clone)]
pub struct WasmEngine {
    engine: Engine,
}

impl WasmEngine {
    /// Build a new engine with fuel metering on. Epoch interruption
    /// is left off in the walking skeleton — the background epoch
    /// ticker belongs with the per-call deadline work that lands
    /// alongside the richer host surface.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.consume_fuel(true).wasm_multi_memory(false);
        let engine = Engine::new(&config).map_err(WasmError::Engine)?;
        Ok(Self { engine })
    }

    /// Access the underlying wasmtime engine (for epoch ticking, etc.).
    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile a WASM module from its binary (or textual, thanks to
    /// wasmtime's auto-detection) representation.
    pub fn load(&self, bytes: &[u8]) -> Result<Module, WasmError> {
        Module::new(&self.engine, bytes).map_err(WasmError::Compile)
    }

    /// Instantiate `module`, run the exported `entry` function to
    /// completion under `fuel` units, and drop the instance. Guest
    /// traps, fuel exhaustion, and host-fn failures are reported as
    /// typed [`WasmError`] variants — the host process is never
    /// killed.
    pub fn run_entry(
        &self,
        module: &Module,
        entry: &str,
        fuel: u64,
        plugin: &str,
    ) -> Result<(), WasmError> {
        let mut store = Store::new(&self.engine, HostState::for_plugin(plugin));
        store.set_fuel(fuel).map_err(WasmError::Fuel)?;

        let mut linker = Linker::new(&self.engine);
        register(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, module)
            .map_err(WasmError::Link)?;
        let entry_fn = instance
            .get_typed_func::<(), ()>(&mut store, entry)
            .map_err(|_| WasmError::MissingExport(entry.to_owned()))?;

        match entry_fn.call(&mut store, ()) {
            Ok(()) => Ok(()),
            Err(err) => Err(classify_trap(err, fuel)),
        }
    }
}

/// Map a `wasmtime::Error` into our `WasmError` vocabulary. Fuel
/// exhaustion gets its own variant because operators want to
/// distinguish "hostile/noisy plugin" from "plugin bug".
fn classify_trap(err: wasmtime::Error, fuel: u64) -> WasmError {
    if let Some(trap) = err.downcast_ref::<Trap>()
        && *trap == Trap::OutOfFuel
    {
        return WasmError::FuelExhausted { fuel };
    }
    WasmError::Trap(err)
}
