//! Minimal smiths-net WASM plugin.
//!
//! Exports four functions:
//! - `run` — calls the host's `smiths::log` import with a fixed
//!   greeting. Smoke test for the host-function plumbing.
//! - `describe` — returns a packed `(ptr << 32) | len` pointing at a
//!   static JSON `CapabilityDescriptor` in linear memory. Consumed by
//!   `smiths-plugin`'s WASM loader tier at registration time.
//! - `alloc(len: i32) -> i32` — tiny bump allocator over a fixed
//!   static buffer, used by the host's `invoke` ABI to hand the guest
//!   the method + params bytes.
//! - `invoke(method_ptr, method_len, params_ptr, params_len) -> i64` —
//!   dispatcher. The MVP plugin ignores its inputs and always returns
//!   a fixed `{"result":"ok"}` response envelope.

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

// ---- invoke ABI ----------------------------------------------------
//
// The engine writes method + params bytes into guest memory via
// `alloc`, then calls `invoke(...)`. The response is a JSON envelope
// at a packed `(ptr << 32) | len` — either `{"result": X}` or
// `{"error": "msg"}`. This MVP plugin always returns a fixed OK.

/// Fixed-size scratch buffer the bump allocator hands out from.
/// 4 KiB is more than enough for the tiny method + params strings
/// this example receives, and keeps the whole guest under ~1 KiB of
/// code.
const BUMP_CAP: usize = 4096;
static mut BUMP: [u8; BUMP_CAP] = [0u8; BUMP_CAP];
static mut BUMP_POS: usize = 0;

/// Bump allocator. Returns a pointer into the static `BUMP` buffer.
/// Traps on exhaustion — tests never come close, and a real plugin
/// would swap this for wee_alloc or a per-call reset.
///
/// # Safety
/// The static-buffer bump pointer is touched here; the single-threaded
/// WASM guest means there are no concurrent callers. Negative `len`
/// from the host traps cleanly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc(len: i32) -> i32 {
    unsafe {
        let Ok(req) = usize::try_from(len) else {
            core::arch::wasm32::unreachable();
        };
        let pos = BUMP_POS;
        let Some(end) = pos.checked_add(req) else {
            core::arch::wasm32::unreachable();
        };
        if end > BUMP_CAP {
            core::arch::wasm32::unreachable();
        }
        BUMP_POS = end;
        let base = (&raw const BUMP).cast::<u8>() as usize;
        (base + pos) as i32
    }
}

/// Static success envelope. 15 bytes: `{"result":"ok"}`.
static INVOKE_OK: &[u8] = br#"{"result":"ok"}"#;

/// The invoke dispatcher. Ignores `method`/`params` at this tier —
/// the minimal contract is "guest can be called and returns a
/// well-formed envelope the host parses". Real plugins branch on
/// `method` and build a response in their own scratch buffer.
#[unsafe(no_mangle)]
pub extern "C" fn invoke(
    _method_ptr: i32,
    _method_len: i32,
    _params_ptr: i32,
    _params_len: i32,
) -> i64 {
    let ptr = INVOKE_OK.as_ptr() as u32 as i64;
    let len = INVOKE_OK.len() as u32 as i64;
    (ptr << 32) | len
}

/// Panics in a WASM guest must not unwind into host code; abort to a
/// trap instead. Required on `#![no_std]` targets that do not ship
/// the default panic handler.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
