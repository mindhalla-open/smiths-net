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

use dashmap::DashMap;
use tracing::info;
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
        }
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
