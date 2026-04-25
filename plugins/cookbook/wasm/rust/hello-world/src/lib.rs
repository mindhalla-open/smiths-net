//! Minimal smiths-net WASM plugin — hello-world cookbook recipe.
//!
//! Exports one function:
//! - `run` — calls the host's `smiths::log` import with a greeting.
//!
//! This is the simplest possible plugin. For the full four-export ABI
//! (`run` + `describe` + `alloc` + `invoke`), see the `canonical-hook`
//! recipe.

#![no_std]
#![no_main]

// Import the host's log function. The `wasm_import_module` attribute
// tells the linker to look for this symbol under the `smiths` module
// — the engine provides it automatically.
#[link(wasm_import_module = "smiths")]
unsafe extern "C" {
    fn log(ptr: *const u8, len: usize);
}

/// Thin wrapper so call sites stay safe.
fn host_log(msg: &str) {
    unsafe {
        log(msg.as_ptr(), msg.len());
    }
}

const GREETING: &str = "hello-world: hook fired";

/// Entry point the engine invokes via `WasmEngine::run_entry`.
#[unsafe(no_mangle)]
pub extern "C" fn run() {
    host_log(GREETING);
}

/// Panics must not unwind into host code — abort to a trap.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
