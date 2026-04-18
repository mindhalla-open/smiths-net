//! Minimal smiths-net WASM plugin.
//!
//! Exports one function, `run`, that calls the host's `smiths::log`
//! import with a fixed greeting. Useful as a smoke test for the
//! wasmtime engine + host-function plumbing before richer host
//! surface (`send_sip`, `send_rtp`, timers, state) lands.

#![no_std]
#![no_main]

#[link(wasm_import_module = "smiths")]
unsafe extern "C" {
    /// `smiths::log(ptr: *const u8, len: usize)` — emits a UTF-8
    /// string in the host's tracing log, tagged with the plugin name
    /// the host passed into `run_entry`.
    fn log(ptr: *const u8, len: usize);
}

/// Thin wrapper around the host import.
fn host_log(msg: &str) {
    // SAFETY: the host validates ptr/len against guest memory bounds
    // and returns a trap on overflow, so passing the slice's own
    // pointer and length is sound.
    unsafe {
        log(msg.as_ptr(), msg.len());
    }
}

const GREETING: &str = "rust-logger: hook fired";

/// Entry point the engine invokes via `WasmEngine::run_entry`.
///
/// `extern "C"` + `#[unsafe(no_mangle)]` so the symbol name survives
/// unchanged for the host linker to resolve.
#[unsafe(no_mangle)]
pub extern "C" fn run() {
    host_log(GREETING);
}

/// Panics in a WASM guest must not unwind into host code; abort to a
/// trap instead. Required on `#![no_std]` targets that do not ship
/// the default panic handler.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
