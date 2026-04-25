//! Canonical-hook smiths-net WASM plugin — cookbook recipe.
//!
//! Exports four functions required by the full plugin ABI:
//!
//! - `run`      — one-shot initialisation; calls `smiths::log`.
//! - `describe` — returns a packed `(ptr << 32) | len` pointing at a
//!                static JSON `CapabilityDescriptor` in linear memory.
//! - `alloc`    — bump allocator the host uses to write `invoke`
//!                arguments into guest memory.
//! - `invoke`   — tool dispatcher. This example echoes the method
//!                name back inside a `{"result":"…"}` envelope.

#![no_std]
#![no_main]

// ---------------------------------------------------------------------------
// Host imports
// ---------------------------------------------------------------------------

#[link(wasm_import_module = "smiths")]
unsafe extern "C" {
    /// Write a UTF-8 log line to the engine's tracing output.
    fn log(ptr: *const u8, len: usize);
}

fn host_log(msg: &str) {
    unsafe {
        log(msg.as_ptr(), msg.len());
    }
}

// ---------------------------------------------------------------------------
// run — one-shot initialisation
// ---------------------------------------------------------------------------

const GREETING: &str = "canonical-hook: plugin loaded";

#[unsafe(no_mangle)]
pub extern "C" fn run() {
    host_log(GREETING);
}

// ---------------------------------------------------------------------------
// describe — capability descriptor
// ---------------------------------------------------------------------------

/// Static JSON that the engine's WASM loader parses into a
/// `CapabilityDescriptor` at registration time. The engine reads
/// this via the `(ptr << 32) | len` packed return value.
static DESCRIBE_JSON: &[u8] = br#"{"capability":"ai.echo","plugin":"canonical-hook","abi":"1.0","description":"Echo the method name back - canonical ABI example."}"#;

/// Return the descriptor location as a packed `i64`.
///
/// The convention is `(ptr << 32) | len` — 32 high bits carry the
/// pointer into linear memory, 32 low bits carry the byte length.
/// The host reads `[ptr .. ptr + len)` from the guest's exported
/// `memory` and parses it as UTF-8 JSON.
#[unsafe(no_mangle)]
pub extern "C" fn describe() -> i64 {
    let ptr = DESCRIBE_JSON.as_ptr() as u32 as i64;
    let len = DESCRIBE_JSON.len() as u32 as i64;
    (ptr << 32) | len
}

// ---------------------------------------------------------------------------
// alloc — bump allocator
// ---------------------------------------------------------------------------

/// 4 KiB scratch buffer the bump allocator hands out from.
const BUMP_CAP: usize = 4096;
static mut BUMP: [u8; BUMP_CAP] = [0u8; BUMP_CAP];
static mut BUMP_POS: usize = 0;

/// Bump allocator. The host calls `alloc(n)` to reserve `n` bytes
/// in guest memory, then writes the method name and params JSON
/// into the returned pointer before calling `invoke`.
///
/// Returns a pointer into the static `BUMP` buffer. Traps on
/// exhaustion — a real plugin would use `wee_alloc` or reset the
/// bump pointer between calls.
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

// ---------------------------------------------------------------------------
// invoke — tool dispatcher
// ---------------------------------------------------------------------------

/// Scratch buffer for building the response envelope at runtime.
/// Big enough for the fixed prefix + any method name the host sends.
static mut RESPONSE: [u8; 512] = [0u8; 512];

/// Tool dispatcher. The host writes `method` and `params` into
/// guest memory via `alloc`, then calls `invoke`. The return value
/// uses the same `(ptr << 32) | len` packing as `describe`.
///
/// This example builds a response that echoes the method name:
/// ```json
/// {"result":"echo: <method>"}
/// ```
#[unsafe(no_mangle)]
pub extern "C" fn invoke(
    method_ptr: i32,
    method_len: i32,
    _params_ptr: i32,
    _params_len: i32,
) -> i64 {
    // Read the method name from guest memory.
    let method = unsafe {
        let ptr = method_ptr as usize as *const u8;
        let len = method_len as usize;
        core::slice::from_raw_parts(ptr, len)
    };

    // Build: {"result":"echo: <method>"}
    let prefix = br#"{"result":"echo: "#;
    let suffix = br#""}"#;

    let total = prefix.len() + method.len() + suffix.len();

    // Safety: single-threaded WASM guest — no concurrent access.
    let out = unsafe {
        let buf = &mut RESPONSE[..total];
        let mut cursor = 0;

        buf[cursor..cursor + prefix.len()].copy_from_slice(prefix);
        cursor += prefix.len();

        buf[cursor..cursor + method.len()].copy_from_slice(method);
        cursor += method.len();

        buf[cursor..cursor + suffix.len()].copy_from_slice(suffix);

        buf.as_ptr()
    };

    let ptr = out as u32 as i64;
    let len = total as u32 as i64;
    (ptr << 32) | len
}

// ---------------------------------------------------------------------------
// panic handler
// ---------------------------------------------------------------------------

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
