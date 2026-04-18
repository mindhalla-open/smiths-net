//! Host-function surface exposed to WASM guests.
//!
//! Each host fn is registered on the shared [`wasmtime::Linker`] under
//! the `smiths` module name. The guest imports them as
//! `(import "smiths" "<name>" ...)`. Keep the surface **intentionally
//! small** — every new symbol is ABI we are forever committed to.

use tracing::info;
use wasmtime::{Caller, Linker};

use crate::error::WasmError;

/// Per-instance mutable state handed to each host-fn call. Today just
/// a marker; future hosts read call-id, plugin name, etc. from here.
#[derive(Default)]
pub struct HostState {
    /// Plugin instance name, for `plugin=...` in tracing events.
    pub plugin: String,
}

impl HostState {
    /// Build state tagged with the invoking plugin's name.
    #[must_use]
    pub fn for_plugin(plugin: impl Into<String>) -> Self {
        Self {
            plugin: plugin.into(),
        }
    }
}

/// Register every host function on `linker`. Call once per engine.
pub fn register(linker: &mut Linker<HostState>) -> Result<(), WasmError> {
    linker
        .func_wrap("smiths", "log", host_log)
        .map_err(WasmError::Link)?;
    Ok(())
}

/// `smiths::log(ptr: i32, len: i32)` — read a UTF-8 string from guest
/// memory and emit it as a tracing `info!` event tagged with the
/// plugin name. Out-of-bounds or non-UTF-8 is a trap: the guest's
/// arithmetic is its problem, not ours to quietly swallow.
fn host_log(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) -> wasmtime::Result<()> {
    // Wasmtime 43 exposes `wasmtime::Result<T>` (= `Result<T, wasmtime::Error>`).
    // `wasmtime::Error` is still constructible from arbitrary display
    // values, so the ergonomics are the same as the old anyhow path.
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory` to use smiths::log"))?;
    let start = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative ptr"))?;
    let length = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative len"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| wasmtime::Error::msg("ptr/len overflow"))?;
    let data = memory.data(&caller);
    if end > data.len() {
        return Err(wasmtime::Error::msg(format!(
            "log: range {start}..{end} outside guest memory ({} bytes)",
            data.len()
        )));
    }
    let text = std::str::from_utf8(&data[start..end])
        .map_err(|e| wasmtime::Error::msg(format!("log: invalid UTF-8: {e}")))?;
    let plugin = caller.data().plugin.clone();
    info!(target: "smiths_wasm::guest", plugin = %plugin, "{text}");
    Ok(())
}
