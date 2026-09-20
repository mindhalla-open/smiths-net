//! Host-function surface exposed to WASM guests.
//!
//! Each host fn is registered on a [`wasmtime::Linker`] under the
//! `smiths` module name. The guest imports them as
//! `(import "smiths" "<name>" ...)`. Keep the surface **intentionally
//! small** — every new symbol is ABI we are forever committed to.
//!
//! ## Permissions
//!
//! Host fns that do more than log check the plugin's declared
//! permission set before acting. The set is copied onto the
//! [`HostState`] at store construction; a missing permission returns
//! a trap carrying the plugin name + permission string so the
//! plugin author knows exactly what to add to `plugin.toml`.
//! `smiths::log` is unrestricted (it already pipes through tracing
//! and is rate-limited by the subscriber).
//!
//! ## State persistence
//!
//! Guest state persists across invocations via [`state_set`] /
//! [`state_get`] — both gated behind the `"state"` permission. Each
//! plugin has its own key-value namespace keyed on the plugin name;
//! two plugins can use the same key without collision. The map lives
//! on the engine (not the per-call store) so data survives the
//! ephemeral store drop between calls. Every namespace carries a
//! byte budget ([`PluginStore`]); a write that would exceed it traps
//! with [`WasmError::StateBudgetExceeded`] instead of growing host
//! memory without bound.
//!
//! ## Resource limits
//!
//! [`HostState`] doubles as the store's [`ResourceLimiter`]: linear-
//! memory growth past the engine's configured cap traps with
//! [`WasmError::MemoryLimit`], and tables are bounded to a fixed
//! element count.
//!
//! ## Outbound RTP
//!
//! `send_rtp` never spawns work per packet. Packets go through one
//! bounded [`RtpQueue`] per engine, drained by a single task; when
//! the queue is full the packet is dropped, counted, and the guest
//! sees [`SEND_RTP_DROPPED`]. RTP is best-effort by nature, so a
//! drop policy is the right backpressure for a guest that outruns
//! the media fabric.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dashmap::DashMap;
use smiths_core::call::CallOriginator;
use smiths_core::media::{EndpointId, MediaFabric};
use smiths_core::{CallLookup, Event, EventBus, PluginEvent};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wasmtime::{Caller, Linker, ResourceLimiter};

use crate::error::WasmError;

/// Default per-plugin byte budget for the persistent KV store.
pub const DEFAULT_STATE_BUDGET_BYTES: usize = 1024 * 1024;

/// Capacity (in packets) of the outbound RTP queue shared by every
/// guest on one engine. At 50 packets/s/leg this absorbs a burst of
/// roughly 20 s from a single leg before dropping.
pub const RTP_QUEUE_CAPACITY: usize = 1024;

/// `send_rtp` return code: the packet was dropped because the
/// outbound queue was full. Not a trap — RTP is best-effort.
pub const SEND_RTP_DROPPED: i32 = -2;

/// Upper bound on the elements of any guest table. Tables hold
/// `funcref` / `externref` cells, so this caps them at a few MiB.
const MAX_TABLE_ELEMENTS: usize = 1 << 16;

/// Per-plugin key/value store with a byte budget. Keys and values are
/// raw byte arrays to keep the ABI transparent to the guest's choice
/// of encoding. The budget counts `key.len + value.len` for every
/// live entry; overwriting a key only charges the size delta.
#[derive(Debug)]
pub struct PluginStore {
    map: DashMap<Vec<u8>, Vec<u8>>,
    used: AtomicUsize,
    budget: usize,
}

/// Rejected write: the store would exceed its byte budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateBudgetExceeded {
    /// Configured budget in bytes.
    pub budget: usize,
    /// Bytes the store would hold after the rejected write.
    pub would_use: usize,
}

impl PluginStore {
    /// Empty store allowed to hold up to `budget` bytes.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            map: DashMap::new(),
            used: AtomicUsize::new(0),
            budget,
        }
    }

    /// Copy of the value stored under `key`, if any.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.get(key).map(|v| v.value().clone())
    }

    /// Insert or overwrite `key`. Fails without modifying the store
    /// when the write would push the total past the budget.
    pub fn insert(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), StateBudgetExceeded> {
        let entry = self.map.entry(key);
        let previous = match &entry {
            dashmap::Entry::Occupied(o) => o.key().len() + o.get().len(),
            dashmap::Entry::Vacant(_) => 0,
        };
        let incoming = entry.key().len() + value.len();
        let budget = self.budget;
        let reserve = self
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                let next = used.saturating_sub(previous).saturating_add(incoming);
                (next <= budget).then_some(next)
            });
        match reserve {
            Ok(_) => {
                entry.insert(value);
                Ok(())
            }
            Err(used) => Err(StateBudgetExceeded {
                budget,
                would_use: used.saturating_sub(previous).saturating_add(incoming),
            }),
        }
    }

    /// Number of live keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` when no keys are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Bytes currently charged against the budget.
    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Configured budget in bytes.
    #[must_use]
    pub fn budget(&self) -> usize {
        self.budget
    }
}

/// Shared handle to one plugin's [`PluginStore`].
pub type PluginState = Arc<PluginStore>;

/// Per-plugin declared permission set. Cheap to clone — the inner
/// `HashSet` is behind an `Arc`.
pub type PluginPermissions = Arc<HashSet<String>>;

/// Permission string required by [`state_set`] and [`state_get`].
pub const PERM_STATE: &str = "state";

/// Permission string required by `publish_event`.
pub const PERM_EVENTS: &str = "events";

/// Permission string required by `timer_set`.
pub const PERM_TIMERS: &str = "timers";

/// Permission string required by `send_rtp`.
pub const PERM_SEND_RTP: &str = "send_rtp";

/// Permission string required by `originate` / `hangup` — the SIP
/// call-control surface a "brain" plugin uses to place and tear down
/// calls on the engine's behalf.
pub const PERM_SEND_SIP: &str = "send_sip";

/// One outbound packet queued by `send_rtp`.
#[derive(Debug)]
pub struct RtpPacket {
    /// Engine media endpoint the packet leaves from.
    pub endpoint: EndpointId,
    /// Peer address.
    pub remote: SocketAddr,
    /// Raw packet bytes.
    pub payload: Vec<u8>,
}

/// Bounded outbound RTP queue with a drop policy. One per engine;
/// every guest store holds a clone of the `Arc`. The drain task is
/// started lazily on the first `send_rtp` call because that is the
/// first point where a tokio runtime is guaranteed to be on scope.
pub struct RtpQueue {
    tx: mpsc::Sender<RtpPacket>,
    /// Receiver handed to the drain task on first use; `None` after.
    rx: Mutex<Option<mpsc::Receiver<RtpPacket>>>,
    fabric: Arc<dyn MediaFabric>,
    dropped: AtomicU64,
}

impl std::fmt::Debug for RtpQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpQueue")
            .field("capacity", &self.tx.max_capacity())
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

impl RtpQueue {
    /// Queue draining into `fabric` with room for `capacity` packets.
    #[must_use]
    pub fn new(fabric: Arc<dyn MediaFabric>, capacity: usize) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        Arc::new(Self {
            tx,
            rx: Mutex::new(Some(rx)),
            fabric,
            dropped: AtomicU64::new(0),
        })
    }

    /// Packets dropped so far because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Non-blocking enqueue. Returns `0` when queued and
    /// [`SEND_RTP_DROPPED`] when the packet had to be dropped.
    fn push(&self, plugin: &str, packet: RtpPacket) -> i32 {
        match self.tx.try_send(packet) {
            Ok(()) => 0,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                debug!(%plugin, dropped = n, "send_rtp: outbound queue full; packet dropped");
                SEND_RTP_DROPPED
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                warn!(%plugin, dropped = n, "send_rtp: outbound drain task is gone; packet dropped");
                SEND_RTP_DROPPED
            }
        }
    }

    /// Start the drain task on `handle` unless it is already running.
    fn ensure_drain(self: &Arc<Self>, handle: &tokio::runtime::Handle) {
        let rx = self
            .rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(mut rx) = rx else {
            return;
        };
        let fabric = Arc::clone(&self.fabric);
        handle.spawn(async move {
            while let Some(packet) = rx.recv().await {
                if let Err(e) = fabric
                    .send_packet(packet.endpoint, packet.remote, &packet.payload)
                    .await
                {
                    warn!(?e, endpoint = ?packet.endpoint, "send_rtp dispatch failed");
                }
            }
            debug!("send_rtp drain task exiting: queue closed");
        });
    }
}

/// Per-instance mutable state handed to each host-fn call. Rebuilt on
/// every `Store::new`; the persistent part (the per-plugin KV store)
/// and the permission set are injected on construction. Also serves
/// as the store's [`ResourceLimiter`].
pub struct HostState {
    /// Plugin instance name, for `plugin=...` in tracing events.
    pub plugin: String,
    /// The plugin's persistent, budgeted KV store. Cloned from the
    /// engine-side registry on store construction so state survives
    /// across calls.
    pub state: PluginState,
    /// Permissions the manifest declared. Checked by every gated
    /// host fn; empty set means the plugin can only call `log`.
    pub permissions: PluginPermissions,
    /// Event bus handle for `publish_event` / `timer_set`. Optional so
    /// tests that don't exercise bus-bound host fns can leave it
    /// unset — those fns then trap with a descriptive message.
    pub bus: Option<EventBus>,
    /// Call-id → (endpoint, `remote_rtp`) lookup for `send_rtp`.
    pub call_lookup: Option<Arc<dyn CallLookup>>,
    /// Bounded outbound queue `send_rtp` pushes into.
    pub rtp_queue: Option<Arc<RtpQueue>>,
    /// Call originator for `originate` / `hangup`. `None` (e.g. on a
    /// bare-test engine or before the UAC is up) makes those host fns
    /// trap with a descriptive message.
    pub originator: Option<Arc<dyn CallOriginator>>,
    /// Cap on each linear memory of this store, in bytes. Growth past
    /// it traps with [`WasmError::MemoryLimit`].
    pub memory_limit_bytes: usize,
}

impl Default for HostState {
    fn default() -> Self {
        Self::for_plugin("")
    }
}

impl HostState {
    /// Build state tagged with the invoking plugin's name and a fresh
    /// KV store / empty permission set. Useful for tests that don't
    /// care about cross-call persistence or permission gating.
    #[must_use]
    pub fn for_plugin(plugin: impl Into<String>) -> Self {
        Self::for_plugin_with_state(
            plugin,
            Arc::new(PluginStore::new(DEFAULT_STATE_BUDGET_BYTES)),
            Arc::new(HashSet::new()),
        )
    }

    /// Build state for `plugin` with a caller-provided persistent KV
    /// store and permission set — typically retrieved from the
    /// engine's per-plugin registry at invocation time.
    #[must_use]
    pub fn for_plugin_with_state(
        plugin: impl Into<String>,
        state: PluginState,
        permissions: PluginPermissions,
    ) -> Self {
        Self {
            plugin: plugin.into(),
            state,
            permissions,
            bus: None,
            call_lookup: None,
            rtp_queue: None,
            originator: None,
            memory_limit_bytes: crate::engine::DEFAULT_MEMORY_LIMIT_BYTES,
        }
    }

    /// Attach the engine's event bus to this state. Builder-style so
    /// existing construction sites don't need to change signatures.
    #[must_use]
    pub fn with_bus(mut self, bus: Option<EventBus>) -> Self {
        self.bus = bus;
        self
    }

    /// Attach call lookup + outbound queue for `send_rtp`.
    #[must_use]
    pub fn with_media(
        mut self,
        call_lookup: Option<Arc<dyn CallLookup>>,
        rtp_queue: Option<Arc<RtpQueue>>,
    ) -> Self {
        self.call_lookup = call_lookup;
        self.rtp_queue = rtp_queue;
        self
    }

    /// Attach a call originator for `originate` / `hangup`. Builder-
    /// style; resolved per-call from the engine's late-bound slot.
    #[must_use]
    pub fn with_originator(mut self, originator: Option<Arc<dyn CallOriginator>>) -> Self {
        self.originator = originator;
        self
    }

    /// Set the linear-memory cap enforced through [`ResourceLimiter`].
    #[must_use]
    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit_bytes = bytes;
        self
    }

    /// Return a typed permission-denied trap if `permission` is not in
    /// the plugin's declared set.
    fn require(&self, permission: &str, op: &str) -> wasmtime::Result<()> {
        if self.permissions.contains(permission) {
            return Ok(());
        }
        Err(wasmtime::Error::new(WasmError::PermissionDenied {
            plugin: self.plugin.clone(),
            permission: permission.to_owned(),
            op: op.to_owned(),
        }))
    }
}

impl ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_limit_bytes {
            // Returning `Err` turns the failed `memory.grow` into a
            // trap carrying our typed error instead of handing the
            // guest a `-1` it may ignore and retry forever.
            return Err(wasmtime::Error::new(WasmError::MemoryLimit {
                plugin: self.plugin.clone(),
                limit: self.memory_limit_bytes,
                requested: desired,
            }));
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }
}

/// Register every host function on `linker`. The engine builds one
/// linker per store, so this runs once per invocation.
pub fn register(linker: &mut Linker<HostState>) -> Result<(), WasmError> {
    linker
        .func_wrap("smiths", "log", host_log)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "state_set", host_state_set)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "state_get", host_state_get)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "publish_event", host_publish_event)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "timer_set", host_timer_set)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "send_rtp", host_send_rtp)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "originate", host_originate)
        .map_err(WasmError::Link)?;
    linker
        .func_wrap("smiths", "hangup", host_hangup)
        .map_err(WasmError::Link)?;
    Ok(())
}

/// `smiths::log(ptr: i32, len: i32)` — read a UTF-8 string from guest
/// memory and emit it as a tracing `info!` event tagged with the
/// plugin name.
fn host_log(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) -> wasmtime::Result<()> {
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory` to use smiths::log"))?;
    let data = memory.data(&caller);
    let text = read_slice(data, ptr, len, "log")?;
    let text = std::str::from_utf8(text)
        .map_err(|e| wasmtime::Error::msg(format!("log: invalid UTF-8: {e}")))?;
    let plugin = caller.data().plugin.clone();
    info!(target: "smiths_wasm::guest", plugin = %plugin, "{text}");
    Ok(())
}

/// `smiths::state_set(key_ptr, key_len, val_ptr, val_len) -> i32` —
/// write `value` into the plugin's KV store under `key`. Returns
/// `0` on success; any error path traps, including a write that
/// would exceed the plugin's byte budget
/// ([`WasmError::StateBudgetExceeded`]). Requires the `state`
/// permission.
fn host_state_set(
    mut caller: Caller<'_, HostState>,
    key_ptr: i32,
    key_len: i32,
    val_ptr: i32,
    val_len: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_STATE, "state_set")?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
    let data = memory.data(&caller);
    let key = read_slice(data, key_ptr, key_len, "state_set key")?.to_vec();
    let val = read_slice(data, val_ptr, val_len, "state_set value")?.to_vec();
    caller.data().state.insert(key, val).map_err(|e| {
        wasmtime::Error::new(WasmError::StateBudgetExceeded {
            plugin: caller.data().plugin.clone(),
            budget: e.budget,
            would_use: e.would_use,
        })
    })?;
    Ok(0)
}

/// `smiths::state_get(key_ptr, key_len, out_ptr, out_cap) -> i32` —
/// look up `key` in the plugin's KV store and write up to `out_cap`
/// bytes of the value into the guest buffer at `out_ptr`. Returns
/// the value's full length (so the guest can detect truncation by
/// comparing to `out_cap`), or `-1` when the key is missing.
/// Requires the `state` permission.
fn host_state_get(
    mut caller: Caller<'_, HostState>,
    key_ptr: i32,
    key_len: i32,
    out_ptr: i32,
    out_cap: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_STATE, "state_get")?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
    // Extract the key first while we still hold an immutable borrow.
    let key = {
        let data = memory.data(&caller);
        read_slice(data, key_ptr, key_len, "state_get key")?.to_vec()
    };
    let Some(value) = caller.data().state.get(&key) else {
        return Ok(-1);
    };
    let cap = usize::try_from(out_cap).map_err(|_| wasmtime::Error::msg("negative out_cap"))?;
    let start = usize::try_from(out_ptr).map_err(|_| wasmtime::Error::msg("negative out_ptr"))?;
    let to_write = value.len().min(cap);
    let end = start
        .checked_add(to_write)
        .ok_or_else(|| wasmtime::Error::msg("state_get out ptr/len overflow"))?;
    let data = memory.data_mut(&mut caller);
    if end > data.len() {
        return Err(wasmtime::Error::msg(format!(
            "state_get: out buffer {start}..{end} outside guest memory"
        )));
    }
    data[start..end].copy_from_slice(&value[..to_write]);
    i32::try_from(value.len()).map_err(|_| wasmtime::Error::msg("value too large for i32 return"))
}

/// `smiths::publish_event(topic_ptr, topic_len, data_ptr, data_len) -> i32`
/// — forward a free-form `(topic, data)` pair to the engine's event
/// bus as [`PluginEvent::Published`]. Returns `0` on success;
/// `-1` when no subscribers were live for the publish (non-fatal —
/// lets the guest avoid holding on to data nobody is listening for).
/// Requires the `events` permission.
fn host_publish_event(
    mut caller: Caller<'_, HostState>,
    topic_ptr: i32,
    topic_len: i32,
    data_ptr: i32,
    data_len: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_EVENTS, "publish_event")?;
    let bus = caller
        .data()
        .bus
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("publish_event: no EventBus bound to HostState"))?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
    let (topic, data) = {
        let mem = memory.data(&caller);
        let topic_bytes = read_slice(mem, topic_ptr, topic_len, "publish_event topic")?;
        let topic = std::str::from_utf8(topic_bytes)
            .map_err(|e| wasmtime::Error::msg(format!("publish_event topic not UTF-8: {e}")))?
            .to_owned();
        let data = read_slice(mem, data_ptr, data_len, "publish_event data")?.to_vec();
        (topic, data)
    };
    let plugin = caller.data().plugin.clone();
    match bus.publish(Event::Plugin(PluginEvent::Published {
        plugin,
        topic,
        data,
    })) {
        Ok(_) => Ok(0),
        Err(_) => Ok(-1),
    }
}

/// `smiths::timer_set(delay_ms: i32, event_id: i32) -> i32` — schedule
/// a one-shot host timer. When the delay elapses, the engine
/// publishes a [`PluginEvent::TimerFired`] carrying `event_id` back
/// on the bus. Returns `0` on schedule, traps on negative delay or
/// when no tokio runtime is on scope. Requires the `timers`
/// permission.
///
/// The timer is a tokio sleep on the runtime the host fn is called
/// from — the engine runs guests on that runtime's blocking pool, so
/// the handle is always available in production.
// `Caller` is passed by value to match wasmtime's `func_wrap` ABI; the
// needless_pass_by_value lint is off-base for this signature shape.
#[allow(clippy::needless_pass_by_value)]
fn host_timer_set(
    caller: Caller<'_, HostState>,
    delay_ms: i32,
    event_id: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_TIMERS, "timer_set")?;
    let bus = caller
        .data()
        .bus
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("timer_set: no EventBus bound to HostState"))?;
    let plugin = caller.data().plugin.clone();
    let delay_ms_u64 =
        u64::try_from(delay_ms).map_err(|_| wasmtime::Error::msg("timer_set: negative delay"))?;
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| wasmtime::Error::msg("timer_set: no tokio runtime on this thread"))?;
    handle.spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms_u64)).await;
        // Publish failures mean no subscribers — harmless to drop.
        let _ = bus.publish(Event::Plugin(PluginEvent::TimerFired { plugin, event_id }));
    });
    Ok(0)
}

/// `smiths::send_rtp(call_id_ptr, call_id_len, bytes_ptr, bytes_len) -> i32`
/// — send one RTP (or generic UDP) packet out on the media endpoint
/// of the call identified by `call_id`. Returns `0` when the packet
/// was queued, `-1` when the call is unknown / has no media,
/// [`SEND_RTP_DROPPED`] (`-2`) when the outbound queue was full, and
/// traps for permission / ABI errors. Requires the `send_rtp`
/// permission.
///
/// Packets are handed to the engine's bounded [`RtpQueue`]; one
/// drain task pushes them through `MediaFabric::send_packet`. The
/// guest can watch the drop code to detect that it is outrunning
/// the fabric.
fn host_send_rtp(
    mut caller: Caller<'_, HostState>,
    call_id_ptr: i32,
    call_id_len: i32,
    bytes_ptr: i32,
    bytes_len: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_SEND_RTP, "send_rtp")?;
    let lookup = caller
        .data()
        .call_lookup
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("send_rtp: no CallLookup bound to HostState"))?;
    let queue = caller
        .data()
        .rtp_queue
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("send_rtp: no RtpQueue bound to HostState"))?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
    let (call_id, payload) = {
        let mem = memory.data(&caller);
        let id_bytes = read_slice(mem, call_id_ptr, call_id_len, "send_rtp call_id")?;
        let call_id = std::str::from_utf8(id_bytes)
            .map_err(|e| wasmtime::Error::msg(format!("send_rtp call_id not UTF-8: {e}")))?
            .to_owned();
        let payload = read_slice(mem, bytes_ptr, bytes_len, "send_rtp payload")?.to_vec();
        (call_id, payload)
    };

    let Some((endpoint, remote)) = lookup.endpoint_for(&call_id) else {
        return Ok(-1);
    };

    // Requires a tokio runtime on the calling thread — the engine
    // invokes guests from the runtime's blocking pool, so this holds
    // in production. Tests that exercise `send_rtp` are `#[tokio::test]`.
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| wasmtime::Error::msg("send_rtp: no tokio runtime on this thread"))?;
    queue.ensure_drain(&handle);
    Ok(queue.push(
        &caller.data().plugin,
        RtpPacket {
            endpoint,
            remote,
            payload,
        },
    ))
}

/// `smiths::originate(target_ptr, target_len) -> i32` — place an
/// outbound call to the SIP URI in guest memory. Returns `0` once the
/// call attempt is dispatched, traps on permission / ABI errors.
/// Requires the `send_sip` permission.
///
/// The underlying `CallOriginator::place_call` is async while
/// wasmtime host fns are sync, so the attempt runs as a
/// fire-and-forget tokio task. The guest learns the outcome (and the
/// allocated Call-ID) by subscribing to engine call events, not from
/// this return value — call setup takes a SIP round-trip the guest
/// can't block on.
#[allow(clippy::needless_pass_by_value)]
fn host_originate(
    mut caller: Caller<'_, HostState>,
    target_ptr: i32,
    target_len: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_SEND_SIP, "originate")?;
    let originator =
        caller.data().originator.clone().ok_or_else(|| {
            wasmtime::Error::msg("originate: no CallOriginator bound to HostState")
        })?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
    let target = {
        let mem = memory.data(&caller);
        let bytes = read_slice(mem, target_ptr, target_len, "originate target")?;
        std::str::from_utf8(bytes)
            .map_err(|e| wasmtime::Error::msg(format!("originate target not UTF-8: {e}")))?
            .to_owned()
    };
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| wasmtime::Error::msg("originate: no tokio runtime on this thread"))?;
    let plugin = caller.data().plugin.clone();
    handle.spawn(async move {
        match originator.place_call(&target).await {
            Ok(call_id) => info!(%plugin, %call_id, %target, "wasm plugin originated call"),
            Err(e) => warn!(%plugin, %target, ?e, "wasm originate failed"),
        }
    });
    Ok(0)
}

/// `smiths::hangup(call_id_ptr, call_id_len) -> i32` — tear down an
/// outbound call by Call-ID. Returns `0` on dispatch, traps on
/// permission / ABI errors. Requires the `send_sip` permission.
/// Fire-and-forget for the same reason as [`host_originate`].
#[allow(clippy::needless_pass_by_value)]
fn host_hangup(
    mut caller: Caller<'_, HostState>,
    call_id_ptr: i32,
    call_id_len: i32,
) -> wasmtime::Result<i32> {
    caller.data().require(PERM_SEND_SIP, "hangup")?;
    let originator = caller
        .data()
        .originator
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("hangup: no CallOriginator bound to HostState"))?;
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))?;
    let call_id = {
        let mem = memory.data(&caller);
        let bytes = read_slice(mem, call_id_ptr, call_id_len, "hangup call_id")?;
        std::str::from_utf8(bytes)
            .map_err(|e| wasmtime::Error::msg(format!("hangup call_id not UTF-8: {e}")))?
            .to_owned()
    };
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| wasmtime::Error::msg("hangup: no tokio runtime on this thread"))?;
    let plugin = caller.data().plugin.clone();
    handle.spawn(async move {
        if let Err(e) = originator.hangup(&call_id).await {
            warn!(%plugin, %call_id, ?e, "wasm hangup failed");
        }
    });
    Ok(0)
}

/// Slice a bounded region of guest memory. Returns a descriptive
/// error instead of panicking on out-of-bounds / negative args.
fn read_slice<'a>(data: &'a [u8], ptr: i32, len: i32, op: &str) -> wasmtime::Result<&'a [u8]> {
    let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative ptr"))?;
    let length = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative len"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| wasmtime::Error::msg("ptr/len overflow"))?;
    if end > data.len() {
        return Err(wasmtime::Error::msg(format!(
            "{op}: range {start}..{end} outside guest memory ({} bytes)",
            data.len()
        )));
    }
    Ok(&data[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_store_charges_key_plus_value_and_refunds_on_overwrite() {
        let store = PluginStore::new(16);
        store.insert(b"k".to_vec(), vec![0; 8]).unwrap();
        assert_eq!(store.used_bytes(), 9);
        // Overwrite with a smaller value: charged the delta only.
        store.insert(b"k".to_vec(), vec![0; 2]).unwrap();
        assert_eq!(store.used_bytes(), 3);
        // Second key that fits exactly.
        store.insert(b"j".to_vec(), vec![0; 12]).unwrap();
        assert_eq!(store.used_bytes(), 16);
        // Anything more is rejected and leaves the store untouched.
        let err = store.insert(b"z".to_vec(), vec![0; 1]).unwrap_err();
        assert_eq!(
            err,
            StateBudgetExceeded {
                budget: 16,
                would_use: 18
            }
        );
        assert_eq!(store.used_bytes(), 16);
        assert_eq!(store.len(), 2);
        assert!(store.get(b"z").is_none());
    }
}
