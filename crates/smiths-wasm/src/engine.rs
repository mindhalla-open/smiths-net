//! Wasmtime engine wrapper with per-call fuel, memory caps, and
//! epoch-based wall-clock deadlines.
//!
//! `WasmEngine` is cheap to clone (it holds an `Arc`-backed
//! `wasmtime::Engine`). The usual flow is:
//!
//! ```ignore
//! let engine = WasmEngine::new?
//!.with_memory_limit(32 * 1024 * 1024)
//!.with_invoke_timeout(std::time::Duration::from_secs(2));
//! let module  = engine.load(wasm_bytes)?;
//! engine.run_entry(&module, "on_call", 1_000_000, "rust-logger")?;
//! // With a per-call wall-clock deadline override:
//! engine.run_with_deadline(&module, "on_call", 1_000_000, "rust-logger",
//!                          std::time::Duration::from_millis(50))?;
//! ```
//!
//! Every entry point instantiates a fresh [`wasmtime::Store`] per
//! call (per-call isolation), wires the `smiths::*` host imports, and
//! invokes the named export. Each store gets:
//!
//! - a fuel budget (deterministic instruction cap);
//! - a linear-memory cap enforced through [`HostState`]'s
//!   `ResourceLimiter` — growth past it traps with
//!   [`WasmError::MemoryLimit`];
//! - an epoch deadline relative to the engine's current epoch. One
//!   background ticker thread per engine bumps the epoch every
//!   [`EPOCH_TICK`]; a guest that runs past its own deadline traps
//!   with [`WasmError::Timeout`]. Because each store's deadline is
//!   relative to the shared counter, concurrent guests with
//!   different deadlines never interrupt each other.
//!
//! Entry points are synchronous and block the calling thread for
//! the duration of the guest call. Async callers (the plugin
//! provider) run them on `tokio::task::spawn_blocking`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use dashmap::DashMap;
use serde_json::Value;
use smiths_core::call::CallOriginator;
use smiths_core::media::MediaFabric;
use smiths_core::{CallLookup, EventBus};
use wasmtime::{Config, Engine, Linker, Memory, Module, Store, Trap};

use crate::error::WasmError;
use crate::host::{
    DEFAULT_STATE_BUDGET_BYTES, HostState, PluginPermissions, PluginState, PluginStore,
    RTP_QUEUE_CAPACITY, RtpQueue, register,
};

/// Default per-call fuel budget. Guest is a small event handler, not
/// a compute workload — a million units is generous.
pub const DEFAULT_FUEL: u64 = 1_000_000;

/// Default cap on each guest linear memory (64 MiB).
pub const DEFAULT_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// Default wall-clock budget for one guest invocation.
pub const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Period of the engine's epoch ticker thread. Deadlines are rounded
/// up to a whole number of ticks, so this is also their resolution.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

/// Wasmtime engine configured for smiths plugins.
///
/// Cheap to clone — shared state is `Arc`-backed. Persistent plugin
/// KV state and the per-plugin declared permission set both live on
/// the engine so they survive the ephemeral [`Store`] built per
/// invocation. The resource knobs (`memory_limit_bytes`,
/// `invoke_timeout`, `fuel`, `state_budget_bytes`) are plain values
/// copied by `clone`, so a clone can be tuned independently.
#[derive(Clone)]
pub struct WasmEngine {
    engine: Engine,
    /// Per-plugin persistent KV stores. Created on first access via
    /// [`Self::plugin_state`].
    states: Arc<DashMap<String, PluginState>>,
    /// Per-plugin declared permissions. Populated via
    /// [`Self::set_plugin_permissions`]; a miss returns an empty set,
    /// so an unregistered plugin can only use `log`.
    permissions: Arc<DashMap<String, PluginPermissions>>,
    /// Engine-wide event bus forwarded to guests via
    /// `smiths::publish_event` and `smiths::timer_set`. Attached via
    /// [`Self::with_bus`]; `None` on a bare-test engine means those
    /// host fns trap with a descriptive message when called.
    bus: Option<EventBus>,
    /// Call-id → (endpoint, remote) lookup for `send_rtp`. Attached
    /// via [`Self::with_media`]; `None` disables `send_rtp`.
    call_lookup: Option<Arc<dyn CallLookup>>,
    /// Bounded outbound RTP queue draining into the media fabric.
    /// Attached via [`Self::with_media`]; `None` disables `send_rtp`.
    rtp_queue: Option<Arc<RtpQueue>>,
    /// Late-bound call originator for `originate` / `hangup`. The UAC
    /// is built *after* plugins load, so the engine holds a write-once
    /// slot every cloned `WasmEngine` shares; the CLI fills it via
    /// [`Self::originator_slot`] once SIP is up. Empty until then —
    /// guests calling `originate` trap descriptively.
    originator: Arc<OnceLock<Arc<dyn CallOriginator>>>,
    /// Linear-memory cap applied to every store.
    memory_limit_bytes: usize,
    /// Wall-clock budget for `call_invoke` / `call_describe` /
    /// `run_entry`.
    invoke_timeout: Duration,
    /// Fuel budget for `call_invoke` / `call_describe`.
    fuel: u64,
    /// Byte budget handed to each newly created plugin KV store.
    state_budget_bytes: usize,
    /// Keeps the epoch ticker thread alive; the thread exits when the
    /// last engine clone drops. Only its `Drop` matters.
    _ticker: Arc<EpochTicker>,
}

impl std::fmt::Debug for WasmEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `engine` is a wasmtime::Engine (no user-visible shape);
        // list the runtime-visible fields with their cardinality/
        // presence so logs stay human-readable without dumping the
        // whole wasmtime state.
        let _ = &self.engine;
        f.debug_struct("WasmEngine")
            .field("states", &self.states.len())
            .field("permissions", &self.permissions.len())
            .field("bus", &self.bus.is_some())
            .field("call_lookup", &self.call_lookup.is_some())
            .field("rtp_queue", &self.rtp_queue.is_some())
            .field("originator", &self.originator.get().is_some())
            .field("memory_limit_bytes", &self.memory_limit_bytes)
            .field("invoke_timeout", &self.invoke_timeout)
            .field("fuel", &self.fuel)
            .field("state_budget_bytes", &self.state_budget_bytes)
            .finish_non_exhaustive()
    }
}

impl WasmEngine {
    /// Build a new engine with fuel metering + epoch interruption on
    /// and the default resource caps ([`DEFAULT_MEMORY_LIMIT_BYTES`],
    /// [`DEFAULT_INVOKE_TIMEOUT`], [`DEFAULT_FUEL`],
    /// [`DEFAULT_STATE_BUDGET_BYTES`]). Starts the epoch ticker
    /// thread that drives wall-clock deadlines.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config
            .consume_fuel(true)
            .epoch_interruption(true)
            .wasm_multi_memory(false);
        let engine = Engine::new(&config).map_err(WasmError::Engine)?;
        let ticker = EpochTicker::spawn(engine.clone())?;
        Ok(Self {
            engine,
            states: Arc::new(DashMap::new()),
            permissions: Arc::new(DashMap::new()),
            bus: None,
            call_lookup: None,
            rtp_queue: None,
            originator: Arc::new(OnceLock::new()),
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            invoke_timeout: DEFAULT_INVOKE_TIMEOUT,
            fuel: DEFAULT_FUEL,
            state_budget_bytes: DEFAULT_STATE_BUDGET_BYTES,
            _ticker: Arc::new(ticker),
        })
    }

    /// Attach an event bus. Every store the engine builds from now on
    /// gets this bus on its [`HostState`] so `publish_event` and
    /// `timer_set` can fan out guest-originated events back into the
    /// engine. Builder-style so engine construction stays one-liner
    /// for callers that don't use those host fns.
    #[must_use]
    pub fn with_bus(mut self, bus: EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Attach the call-lookup + media-fabric pair needed by
    /// `smiths::send_rtp`. Without this, guests that call `send_rtp`
    /// trap. Packets flow through a bounded [`RtpQueue`] of
    /// [`RTP_QUEUE_CAPACITY`] packets; see [`Self::rtp_dropped`].
    #[must_use]
    pub fn with_media(
        mut self,
        call_lookup: Arc<dyn CallLookup>,
        media_fabric: Arc<dyn MediaFabric>,
    ) -> Self {
        self.call_lookup = Some(call_lookup);
        self.rtp_queue = Some(RtpQueue::new(media_fabric, RTP_QUEUE_CAPACITY));
        self
    }

    /// Cap each guest linear memory at `bytes`. Growth past the cap
    /// traps with [`WasmError::MemoryLimit`].
    #[must_use]
    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit_bytes = bytes;
        self
    }

    /// Wall-clock budget for every invocation that doesn't pass its
    /// own deadline ([`Self::call_invoke`], [`Self::call_describe`],
    /// [`Self::run_entry`]).
    #[must_use]
    pub fn with_invoke_timeout(mut self, timeout: Duration) -> Self {
        self.invoke_timeout = timeout;
        self
    }

    /// Fuel budget for [`Self::call_invoke`] / [`Self::call_describe`].
    #[must_use]
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    /// Byte budget for each plugin's persistent KV store. Applies to
    /// stores created after this call; a plugin whose store already
    /// exists keeps the budget it was created with.
    #[must_use]
    pub fn with_state_budget(mut self, bytes: usize) -> Self {
        self.state_budget_bytes = bytes;
        self
    }

    /// Linear-memory cap in bytes.
    #[must_use]
    pub fn memory_limit_bytes(&self) -> usize {
        self.memory_limit_bytes
    }

    /// Default per-invocation wall-clock budget.
    #[must_use]
    pub fn invoke_timeout(&self) -> Duration {
        self.invoke_timeout
    }

    /// Fuel budget used by `call_invoke` / `call_describe`.
    #[must_use]
    pub fn fuel(&self) -> u64 {
        self.fuel
    }

    /// Byte budget handed to newly created plugin KV stores.
    #[must_use]
    pub fn state_budget_bytes(&self) -> usize {
        self.state_budget_bytes
    }

    /// Outbound RTP packets dropped because the queue was full. `0`
    /// when no media is attached.
    #[must_use]
    pub fn rtp_dropped(&self) -> u64 {
        self.rtp_queue.as_ref().map_or(0, |q| q.dropped())
    }

    /// Hand back a clone of the late-bound originator slot so a caller
    /// (the CLI) can fill it in once the UAC exists. Every cloned
    /// `WasmEngine` — including the ones inside loaded `WasmProvider`s
    /// — shares this slot, so a single `set(...)` lights up
    /// `originate` / `hangup` for all WASM plugins at once.
    #[must_use]
    pub fn originator_slot(&self) -> Arc<OnceLock<Arc<dyn CallOriginator>>> {
        Arc::clone(&self.originator)
    }

    /// Fetch (or lazily create) the persistent KV store for `plugin`.
    /// Used by every entry point but also exposed so tests can
    /// inspect state between invocations.
    #[must_use]
    pub fn plugin_state(&self, plugin: &str) -> PluginState {
        self.states
            .entry(plugin.to_owned())
            .or_insert_with(|| Arc::new(PluginStore::new(self.state_budget_bytes)))
            .clone()
    }

    /// Register the permission set declared in `plugin`'s manifest.
    /// Overwrites any prior entry. Plugins not registered here run
    /// with no permissions, which means only `smiths::log` works —
    /// any other host fn traps with [`WasmError::PermissionDenied`].
    pub fn set_plugin_permissions<I, S>(&self, plugin: &str, permissions: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = permissions.into_iter().map(Into::into).collect();
        self.permissions.insert(plugin.to_owned(), Arc::new(set));
    }

    /// Look up the declared permissions for `plugin`. Missing → empty.
    #[must_use]
    pub fn plugin_permissions(&self, plugin: &str) -> PluginPermissions {
        self.permissions
            .get(plugin)
            .map_or_else(|| Arc::new(HashSet::new()), |e| Arc::clone(e.value()))
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

    /// Build a store for one invocation of `plugin`: host state with
    /// the plugin's persistent KV store and permissions, the memory
    /// cap as the store's resource limiter, `fuel`, and an epoch
    /// deadline `deadline` from now.
    fn new_store(
        &self,
        plugin: &str,
        fuel: u64,
        deadline: Duration,
    ) -> Result<Store<HostState>, WasmError> {
        let host = HostState::for_plugin_with_state(
            plugin,
            self.plugin_state(plugin),
            self.plugin_permissions(plugin),
        )
        .with_bus(self.bus.clone())
        .with_media(self.call_lookup.clone(), self.rtp_queue.clone())
        .with_originator(self.originator.get().cloned())
        .with_memory_limit(self.memory_limit_bytes);
        let mut store = Store::new(&self.engine, host);
        store.limiter(|host| host);
        store.set_fuel(fuel).map_err(WasmError::Fuel)?;
        // Relative to the engine's current epoch, so every concurrent
        // store carries its own absolute deadline against the one
        // shared counter the ticker thread advances.
        store.set_epoch_deadline(ticks_for(deadline));
        store.epoch_deadline_trap();
        Ok(store)
    }

    /// Run `entry` (arity ` -> `) with a fuel budget and the
    /// engine's default wall-clock deadline.
    pub fn run_entry(
        &self,
        module: &Module,
        entry: &str,
        fuel: u64,
        plugin: &str,
    ) -> Result<(), WasmError> {
        self.run_with_deadline(module, entry, fuel, plugin, self.invoke_timeout)
    }

    /// Run `entry` with a fuel budget **and** an explicit wall-clock
    /// deadline. When `deadline` elapses the guest's next epoch check
    /// traps with [`WasmError::Timeout`]; other guests running on the
    /// same engine are unaffected.
    pub fn run_with_deadline(
        &self,
        module: &Module,
        entry: &str,
        fuel: u64,
        plugin: &str,
        deadline: Duration,
    ) -> Result<(), WasmError> {
        let mut store = self.new_store(plugin, fuel, deadline)?;
        let mut linker = Linker::new(&self.engine);
        register(&mut linker)?;
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(WasmError::Link)?;
        let entry_fn = instance
            .get_typed_func::<(), ()>(&mut store, entry)
            .map_err(|_| WasmError::MissingExport(entry.to_owned()))?;
        entry_fn
            .call(&mut store, ())
            .map_err(|err| classify_trap(err, fuel, deadline))
    }

    /// Invoke a guest `describe -> i64` export and read the
    /// returned capability bytes out of the guest's exported
    /// `memory`. The returned `i64` is packed as `(ptr << 32) | len`.
    /// Used by the WASM plugin tier to discover advertised
    /// capabilities at load time. Bounded by the engine's fuel and
    /// wall-clock defaults.
    pub fn call_describe(&self, module: &Module, plugin: &str) -> Result<Vec<u8>, WasmError> {
        let fuel = self.fuel;
        let deadline = self.invoke_timeout;
        let mut store = self.new_store(plugin, fuel, deadline)?;
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
            .map_err(|e| classify_trap(e, fuel, deadline))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Trap(wasmtime::Error::msg("guest must export `memory`")))?;
        read_packed(&memory, &store, packed, "describe")
    }

    /// Invoke a guest method via the `invoke` ABI.
    ///
    /// ## Guest ABI
    ///
    /// The module must export `memory`, `alloc(len: i32) -> i32`
    /// (allocator that returns a pointer into `memory` with space for
    /// `len` bytes), and `invoke(method_ptr: i32, method_len: i32,
    /// params_ptr: i32, params_len: i32) -> i64` (the dispatcher).
    ///
    /// The host:
    /// 1. Serializes `params` as JSON.
    /// 2. Calls `alloc` twice — once for `method` bytes, once for
    ///    `params` bytes — and writes them into guest memory.
    /// 3. Calls `invoke(...)` and reads the packed `i64` return
    ///    (`(ptr << 32) | len`) as a JSON document.
    ///
    /// The guest must return a document matching one of:
    /// - `{"result": X}` — success, `X` is passed back to the caller.
    /// - `{"error": "msg"}` — plugin-level failure surfaced as
    ///   [`WasmError::PluginError`].
    ///
    /// Traps, fuel exhaustion, the wall-clock deadline, memory-cap
    /// hits, and missing exports surface as the usual typed
    /// [`WasmError`] variants. The store is built fresh per call, so
    /// no invocation can observe another's local state (persistent
    /// state still flows through the engine-side KV store).
    pub fn call_invoke(
        &self,
        module: &Module,
        plugin: &str,
        method: &str,
        params: &Value,
    ) -> Result<Value, WasmError> {
        let fuel = self.fuel;
        let deadline = self.invoke_timeout;
        let mut store = self.new_store(plugin, fuel, deadline)?;
        let mut linker = Linker::new(&self.engine);
        register(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, module)
            .map_err(WasmError::Link)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Trap(wasmtime::Error::msg("guest must export `memory`")))?;
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|_| WasmError::MissingExport("alloc".into()))?;
        let invoke = instance
            .get_typed_func::<(i32, i32, i32, i32), i64>(&mut store, "invoke")
            .map_err(|_| WasmError::MissingExport("invoke".into()))?;

        let params_bytes = serde_json::to_vec(params)
            .map_err(|e| WasmError::Trap(wasmtime::Error::msg(format!("params encode: {e}"))))?;
        let method_ptr = alloc_and_write(
            &mut store,
            &alloc,
            &memory,
            method.as_bytes(),
            "method",
            fuel,
            deadline,
        )?;
        let params_ptr = alloc_and_write(
            &mut store,
            &alloc,
            &memory,
            &params_bytes,
            "params",
            fuel,
            deadline,
        )?;

        let method_len = i32_from_len(method.len(), "method")?;
        let params_len = i32_from_len(params_bytes.len(), "params")?;

        let packed = invoke
            .call(&mut store, (method_ptr, method_len, params_ptr, params_len))
            .map_err(|e| classify_trap(e, fuel, deadline))?;

        let body = read_packed(&memory, &store, packed, "invoke")?;
        decode_invoke_response(&body)
    }
}

/// Number of epoch ticks a store deadline of `deadline` maps to. The
/// `+ 1` covers the partial tick already in progress when the store
/// is armed, so a guest always gets at least `deadline`.
fn ticks_for(deadline: Duration) -> u64 {
    let tick = EPOCH_TICK.as_millis().max(1);
    u64::try_from(deadline.as_millis().div_ceil(tick))
        .unwrap_or(u64::MAX)
        .saturating_add(1)
}

/// Background thread that advances the engine epoch every
/// [`EPOCH_TICK`]. Held by every `WasmEngine` clone through an `Arc`;
/// dropping the last clone flags the thread to exit within one tick.
struct EpochTicker {
    stop: Arc<AtomicBool>,
}

impl EpochTicker {
    fn spawn(engine: Engine) -> Result<Self, WasmError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        thread::Builder::new()
            .name("smiths-wasm-epoch".into())
            .spawn(move || {
                while !stop_for_thread.load(Ordering::Relaxed) {
                    thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })
            .map_err(|e| WasmError::Engine(wasmtime::Error::new(e).context("epoch ticker")))?;
        Ok(Self { stop })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Map a `wasmtime::Error` into our `WasmError` vocabulary. Fuel
/// exhaustion and epoch timeout get their own variants, and typed
/// errors raised by host fns or the resource limiter (permission
/// denial, memory cap, state budget) are recovered intact, because
/// operators want to distinguish "hostile/noisy plugin" from "plugin
/// bug" from "plugin was too slow" from "plugin exceeded its declared
/// permissions or resources".
fn classify_trap(err: wasmtime::Error, fuel: u64, deadline: Duration) -> WasmError {
    if let Some(trap) = err.downcast_ref::<Trap>() {
        if *trap == Trap::OutOfFuel {
            return WasmError::FuelExhausted { fuel };
        }
        if *trap == Trap::Interrupt {
            return WasmError::Timeout {
                millis: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            };
        }
    }
    match err.downcast::<WasmError>() {
        Ok(typed) => typed,
        Err(err) => WasmError::Trap(err),
    }
}

/// Allocate `bytes.len` bytes inside the guest via its `alloc`
/// export, write `bytes` there, and return the pointer. Used by the
/// `invoke` ABI trampoline on both the method and params buffers.
fn alloc_and_write(
    store: &mut Store<HostState>,
    alloc: &wasmtime::TypedFunc<i32, i32>,
    memory: &Memory,
    bytes: &[u8],
    label: &str,
    fuel: u64,
    deadline: Duration,
) -> Result<i32, WasmError> {
    let len = i32_from_len(bytes.len(), label)?;
    let ptr = alloc
        .call(&mut *store, len)
        .map_err(|e| classify_trap(e, fuel, deadline))?;
    let start = usize::try_from(ptr)
        .map_err(|_| WasmError::Trap(wasmtime::Error::msg(format!("{label}: negative ptr"))))?;
    let end = start.checked_add(bytes.len()).ok_or_else(|| {
        WasmError::Trap(wasmtime::Error::msg(format!("{label}: ptr/len overflow")))
    })?;
    let data = memory.data_mut(store);
    if end > data.len() {
        return Err(WasmError::Trap(wasmtime::Error::msg(format!(
            "{label}: alloc returned range {start}..{end} outside guest memory ({} bytes)",
            data.len()
        ))));
    }
    data[start..end].copy_from_slice(bytes);
    Ok(ptr)
}

/// Resolve a packed `(ptr << 32) | len` guest pointer into a byte
/// copy, validating bounds against the guest's exported `memory`.
fn read_packed(
    memory: &Memory,
    store: &Store<HostState>,
    packed: i64,
    label: &str,
) -> Result<Vec<u8>, WasmError> {
    #[allow(clippy::cast_sign_loss)] // packed is a guest-provided u64 we control
    let ptr = (packed >> 32) as u32 as usize;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let len = (packed & 0xFFFF_FFFF) as u32 as usize;
    let data = memory.data(store);
    let end = ptr.checked_add(len).ok_or_else(|| {
        WasmError::Trap(wasmtime::Error::msg(format!("{label}: ptr/len overflow")))
    })?;
    if end > data.len() {
        return Err(WasmError::Trap(wasmtime::Error::msg(format!(
            "{label}: range {ptr}..{end} outside guest memory ({} bytes)",
            data.len()
        ))));
    }
    Ok(data[ptr..end].to_vec())
}

fn i32_from_len(len: usize, label: &str) -> Result<i32, WasmError> {
    i32::try_from(len).map_err(|_| {
        WasmError::Trap(wasmtime::Error::msg(format!(
            "{label}: length {len} exceeds i32::MAX"
        )))
    })
}

/// Parse a guest `invoke` response body. Accepts `{"result": X}` or
/// `{"error": "msg"}`; anything else is reported as a trap.
fn decode_invoke_response(body: &[u8]) -> Result<Value, WasmError> {
    let raw: Value = serde_json::from_slice(body)
        .map_err(|e| WasmError::Trap(wasmtime::Error::msg(format!("invoke JSON parse: {e}"))))?;
    let obj = raw.as_object().ok_or_else(|| {
        WasmError::Trap(wasmtime::Error::msg(
            "invoke response must be a JSON object containing `result` or `error`",
        ))
    })?;
    if let Some(err) = obj.get("error") {
        let msg = err
            .as_str()
            .map_or_else(|| err.to_string(), ToOwned::to_owned);
        return Err(WasmError::PluginError(msg));
    }
    if let Some(result) = obj.get("result") {
        return Ok(result.clone());
    }
    Err(WasmError::Trap(wasmtime::Error::msg(
        "invoke response missing both `result` and `error`",
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_round_up_and_cover_the_partial_first_tick() {
        assert_eq!(ticks_for(Duration::ZERO), 1);
        assert_eq!(ticks_for(Duration::from_millis(1)), 2);
        assert_eq!(ticks_for(Duration::from_millis(10)), 2);
        assert_eq!(ticks_for(Duration::from_millis(11)), 3);
        assert_eq!(ticks_for(Duration::from_secs(5)), 501);
    }
}
