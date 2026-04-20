//! Host-function surface exposed to WASM guests.
//!
//! Each host fn is registered on the shared [`wasmtime::Linker`] under
//! the `smiths` module name. The guest imports them as
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
//! ephemeral store drop between calls.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use smiths_core::media::MediaFabric;
use smiths_core::{CallLookup, Event, EventBus, PluginEvent};
use tracing::{info, warn};
use wasmtime::{Caller, Linker};

use crate::error::WasmError;

/// Per-plugin key/value store. Keys and values are raw byte arrays to
/// keep the ABI transparent to the guest's choice of encoding.
pub type PluginState = Arc<DashMap<Vec<u8>, Vec<u8>>>;

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

/// Per-instance mutable state handed to each host-fn call. Rebuilt on
/// every `Store::new`; the persistent part (the per-plugin KV map)
/// and the permission set are injected on construction.
#[derive(Default)]
pub struct HostState {
    /// Plugin instance name, for `plugin=...` in tracing events.
    pub plugin: String,
    /// The plugin's persistent KV map. Cloned from the engine-side
    /// registry on Store construction so state survives across calls.
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
    /// Media fabric handle for `send_rtp` to push packets through.
    pub media_fabric: Option<Arc<dyn MediaFabric>>,
}

impl HostState {
    /// Build state tagged with the invoking plugin's name and a fresh
    /// KV map / empty permission set. Useful for tests that don't care
    /// about cross-call persistence or permission gating.
    #[must_use]
    pub fn for_plugin(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
            state: Arc::new(DashMap::new()),
            permissions: Arc::new(HashSet::new()),
            bus: None,
            call_lookup: None,
            media_fabric: None,
        }
    }

    /// Build state for `plugin` with a caller-provided persistent KV
    /// map and permission set — typically retrieved from the engine's
    /// per-plugin registry at invocation time.
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
            media_fabric: None,
        }
    }

    /// Attach the engine's event bus to this state. Builder-style so
    /// existing construction sites don't need to change signatures.
    #[must_use]
    pub fn with_bus(mut self, bus: Option<EventBus>) -> Self {
        self.bus = bus;
        self
    }

    /// Attach call lookup + media fabric for `send_rtp`.
    #[must_use]
    pub fn with_media(
        mut self,
        call_lookup: Option<Arc<dyn CallLookup>>,
        media_fabric: Option<Arc<dyn MediaFabric>>,
    ) -> Self {
        self.call_lookup = call_lookup;
        self.media_fabric = media_fabric;
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

/// Register every host function on `linker`. Call once per engine.
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
/// `0` on success; any error path traps. Requires the `state`
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
    caller.data().state.insert(key, val);
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
    let value = match caller.data().state.get(&key) {
        Some(v) => v.value().clone(),
        None => return Ok(-1),
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
/// on the bus. Returns `0` on schedule, traps on negative delay.
/// Requires the `timers` permission.
///
/// Implementation note: uses a dedicated std thread rather than
/// `tokio::spawn` so timers work in sync `run_entry` callers that
/// don't have a tokio runtime on scope. The thread is cheap — it
/// sleeps then publishes and exits.
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
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(delay_ms_u64));
        // Publish failures mean no subscribers — harmless to drop.
        let _ = bus.publish(Event::Plugin(PluginEvent::TimerFired { plugin, event_id }));
    });
    Ok(0)
}

/// `smiths::send_rtp(call_id_ptr, call_id_len, bytes_ptr, bytes_len) -> i32`
/// — send one RTP (or generic UDP) packet out on the media endpoint
/// of the call identified by `call_id`. Returns `0` on dispatch,
/// `-1` when the call is unknown / has no media, and traps for
/// permission / ABI errors. Requires the `send_rtp` permission.
///
/// The actual `MediaFabric::send_packet` call is `async`; wasmtime
/// host fns are sync, so we spawn a fire-and-forget tokio task. The
/// guest can confirm delivery by subscribing to events if it needs
/// backpressure — RTP semantics are best-effort anyway.
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
    let fabric = caller
        .data()
        .media_fabric
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("send_rtp: no MediaFabric bound to HostState"))?;
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

    // Requires a tokio runtime on the calling thread — the CLI wires
    // the engine from inside one, so this holds in production. Tests
    // that exercise `send_rtp` are `#[tokio::test]`.
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| wasmtime::Error::msg("send_rtp: no tokio runtime on this thread"))?;
    let plugin = caller.data().plugin.clone();
    handle.spawn(async move {
        if let Err(e) = fabric.send_packet(endpoint, remote, &payload).await {
            warn!(%plugin, ?e, "send_rtp dispatch failed");
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
