//! Wasmtime engine wrapper with per-call fuel + epoch interruption.
//!
//! `WasmEngine` is cheap to clone (it holds an `Arc`-backed
//! `wasmtime::Engine`). The usual flow is:
//!
//! ```ignore
//! let engine  = WasmEngine::new()?;
//! let module  = engine.load(wasm_bytes)?;
//! engine.run_entry(&module, "on_call", 1_000_000, "rust-logger")?;
//! // With a wall-clock deadline:
//! engine.run_with_deadline(&module, "on_call", 1_000_000, "rust-logger",
//!                          std::time::Duration::from_millis(50))?;
//! ```
//!
//! `run_entry` instantiates a fresh [`wasmtime::Store`] per call
//! (per-call isolation), wires the `smiths::*` host imports, and
//! invokes the named exported function (arity `() -> ()` for now).
//! `run_with_deadline` additionally arms a one-shot timer thread
//! that calls [`Engine::increment_epoch`] on expiry so an infinite-
//! loop guest traps cleanly.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use dashmap::DashMap;
use wasmtime::{Config, Engine, Linker, Module, Store, Trap};

use crate::error::WasmError;
use crate::host::{HostState, PluginState, register};

/// Default per-call fuel budget. Guest is a small event handler, not
/// a compute workload — a million units is generous.
pub const DEFAULT_FUEL: u64 = 1_000_000;

/// Wasmtime engine configured for smiths plugins.
///
/// Cheap to clone — state is `Arc`-backed internally. Persistent
/// plugin KV state lives on the engine so it survives the ephemeral
/// [`Store`] we build per `run_entry` call.
#[derive(Clone, Debug)]
pub struct WasmEngine {
    engine: Engine,
    /// Per-plugin `Arc<DashMap>` state. Created on first access via
    /// [`Self::plugin_state`].
    states: Arc<DashMap<String, PluginState>>,
}

impl WasmEngine {
    /// Build a new engine with fuel metering + epoch interruption on.
    /// Per-call deadlines are armed by [`Self::run_with_deadline`];
    /// [`Self::run_entry`] stays deadline-free for computations the
    /// caller doesn't need to bound.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config
            .consume_fuel(true)
            .epoch_interruption(true)
            .wasm_multi_memory(false);
        let engine = Engine::new(&config).map_err(WasmError::Engine)?;
        Ok(Self {
            engine,
            states: Arc::new(DashMap::new()),
        })
    }

    /// Fetch (or lazily create) the persistent KV store for `plugin`.
    /// Called by `run_internal` but also exposed so tests can inspect
    /// state between invocations.
    #[must_use]
    pub fn plugin_state(&self, plugin: &str) -> PluginState {
        self.states
            .entry(plugin.to_owned())
            .or_insert_with(|| Arc::new(DashMap::new()))
            .clone()
    }

    /// Access the underlying wasmtime engine.
    #[must_use]
    pub const fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile a WASM module from its binary (or textual, thanks to
    /// wasmtime's auto-detection) representation.
    pub fn load(&self, bytes: &[u8]) -> Result<Module, WasmError> {
        Module::new(&self.engine, bytes).map_err(WasmError::Compile)
    }

    /// Run `entry` with a fuel budget and no wall-clock deadline.
    pub fn run_entry(
        &self,
        module: &Module,
        entry: &str,
        fuel: u64,
        plugin: &str,
    ) -> Result<(), WasmError> {
        self.run_internal(module, entry, fuel, plugin, None)
    }

    /// Invoke a guest `describe() -> i64` export and read the
    /// returned capability bytes out of the guest's exported
    /// `memory`. The returned `i64` is packed as `(ptr << 32) | len`.
    /// Used by the WASM plugin tier to discover advertised
    /// capabilities at load time.
    pub fn call_describe(&self, module: &Module, plugin: &str) -> Result<Vec<u8>, WasmError> {
        let state = self.plugin_state(plugin);
        let mut store = Store::new(
            &self.engine,
            HostState::for_plugin_with_state(plugin, state),
        );
        store.set_fuel(DEFAULT_FUEL).map_err(WasmError::Fuel)?;
        store.set_epoch_deadline(u64::MAX);

        let mut linker = Linker::new(&self.engine);
        register(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, module)
            .map_err(WasmError::Link)?;
        let describe = instance
            .get_typed_func::<(), i64>(&mut store, "describe")
            .map_err(|_| WasmError::MissingExport("describe".into()))?;
        let packed = describe
            .call(&mut store, ())
            .map_err(|e| classify_trap(e, DEFAULT_FUEL, None))?;
        #[allow(clippy::cast_sign_loss)] // packed is a guest-provided u64 we control
        let ptr = (packed >> 32) as u32 as usize;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let len = (packed & 0xFFFF_FFFF) as u32 as usize;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Trap(wasmtime::Error::msg("guest must export `memory`")))?;
        let data = memory.data(&store);
        let end = ptr
            .checked_add(len)
            .ok_or_else(|| WasmError::Trap(wasmtime::Error::msg("describe: ptr/len overflow")))?;
        if end > data.len() {
            return Err(WasmError::Trap(wasmtime::Error::msg(format!(
                "describe: range {ptr}..{end} outside guest memory ({} bytes)",
                data.len()
            ))));
        }
        Ok(data[ptr..end].to_vec())
    }

    /// Run `entry` with a fuel budget **and** a wall-clock deadline.
    /// When `deadline` elapses the engine's epoch is incremented
    /// once; the guest's store had its epoch deadline set to 1, so
    /// the next WASM instruction traps with [`WasmError::Timeout`].
    /// The timer thread is cancelled as soon as the guest returns.
    pub fn run_with_deadline(
        &self,
        module: &Module,
        entry: &str,
        fuel: u64,
        plugin: &str,
        deadline: Duration,
    ) -> Result<(), WasmError> {
        self.run_internal(module, entry, fuel, plugin, Some(deadline))
    }

    fn run_internal(
        &self,
        module: &Module,
        entry: &str,
        fuel: u64,
        plugin: &str,
        deadline: Option<Duration>,
    ) -> Result<(), WasmError> {
        let state = self.plugin_state(plugin);
        let mut store = Store::new(
            &self.engine,
            HostState::for_plugin_with_state(plugin, state),
        );
        store.set_fuel(fuel).map_err(WasmError::Fuel)?;
        // Configure the store's epoch deadline. When a deadline is
        // supplied we arm a background thread to increment the
        // engine's epoch once; setting the store deadline to 1 means
        // that single increment traps the guest. Without a deadline
        // we seed `u64::MAX` so the guest never spontaneously traps.
        if deadline.is_some() {
            store.set_epoch_deadline(1);
        } else {
            store.set_epoch_deadline(u64::MAX);
        }

        // Arm + stash the timer *before* we enter the guest — a
        // malicious guest could otherwise spin before we got here.
        let timer = deadline.map(|d| spawn_deadline_timer(self.engine.clone(), d));

        let mut linker = Linker::new(&self.engine);
        register(&mut linker)?;

        let run_result = (|| -> Result<(), WasmError> {
            let instance = linker
                .instantiate(&mut store, module)
                .map_err(WasmError::Link)?;
            let entry_fn = instance
                .get_typed_func::<(), ()>(&mut store, entry)
                .map_err(|_| WasmError::MissingExport(entry.to_owned()))?;
            entry_fn
                .call(&mut store, ())
                .map_err(|err| classify_trap(err, fuel, deadline))
        })();

        if let Some(t) = timer {
            // Signal the timer thread to exit if it hasn't fired yet.
            t.cancel();
        }
        run_result
    }
}

/// Cancellable one-shot timer that increments `engine`'s epoch after
/// `deadline`. The timer runs on a dedicated std thread so blocking
/// `run_entry` callers don't need a tokio runtime.
struct DeadlineTimer {
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DeadlineTimer {
    fn cancel(self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

fn spawn_deadline_timer(engine: Engine, deadline: Duration) -> DeadlineTimer {
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_for_task = std::sync::Arc::clone(&cancel);
    thread::spawn(move || {
        // Chunk the sleep so cancellation responds within 10 ms even
        // for long deadlines — matters when the guest returns quickly.
        let chunk = Duration::from_millis(10);
        let start = std::time::Instant::now();
        loop {
            if cancel_for_task.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
                break;
            };
            if remaining.is_zero() {
                break;
            }
            thread::sleep(chunk.min(remaining));
        }
        if !cancel_for_task.load(std::sync::atomic::Ordering::Relaxed) {
            engine.increment_epoch();
        }
    });
    DeadlineTimer { cancel }
}

/// Map a `wasmtime::Error` into our `WasmError` vocabulary. Fuel
/// exhaustion and epoch timeout get their own variants because
/// operators want to distinguish "hostile/noisy plugin" from "plugin
/// bug" and from "plugin was too slow".
fn classify_trap(err: wasmtime::Error, fuel: u64, deadline: Option<Duration>) -> WasmError {
    if let Some(trap) = err.downcast_ref::<Trap>() {
        if *trap == Trap::OutOfFuel {
            return WasmError::FuelExhausted { fuel };
        }
        if *trap == Trap::Interrupt
            && let Some(d) = deadline
        {
            return WasmError::Timeout {
                millis: u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
            };
        }
    }
    WasmError::Trap(err)
}
