//! Minimal smiths-net WASM plugin.
//!
//! Exports two functions:
//! - `run` — calls the host's `smiths::log` import with a fixed
//!   greeting. Smoke test for the host-function plumbing.
//! - `describe` — returns a packed `(ptr << 32) | len` pointing at a
//!   static JSON `CapabilityDescriptor` in linear memory. Consumed by
//!   `smiths-plugin`'s WASM loader tier at registration time.

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

/// Static JSON capability descriptor. The host's WASM loader tier
/// reads this byte range via the `describe()` export and parses it
/// into a [`CapabilityDescriptor`] at registration time.
static DESCRIBE_JSON: &[u8] = br#"{"capability":"ai.log","plugin":"rust-logger","abi":"1.0","description":"Smoke-test WASM plugin that just logs."}"#;

/// Return the descriptor's `(ptr << 32) | len` packed into an `i64`.
/// Contract: the bytes at `[ptr, ptr + len)` in the exported `memory`
/// are valid UTF-8 JSON matching `CapabilityDescriptor`.
#[unsafe(no_mangle)]
pub extern "C" fn describe() -> i64 {
    let ptr = DESCRIBE_JSON.as_ptr() as u32 as i64;
    let len = DESCRIBE_JSON.len() as u32 as i64;
    (ptr << 32) | len
}

/// Panics in a WASM guest must not unwind into host code; abort to a
/// trap instead. Required on `#![no_std]` targets that do not ship
/// the default panic handler.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
