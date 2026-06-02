//! `sip-client` — a WASM call-control plugin for smiths-net.
//!
//! This is the "brain" half of the SIP-client work: it decides *when*
//! to place a call and asks the engine to do it via the
//! `smiths::originate` host function. It deliberately does **no**
//! media — a WASM sandbox has no sockets and no audio devices, so the
//! actual voice is carried by the native `smiths-softphone` client.
//!
//! ABI (see `plugins/cookbook/wasm/rust/canonical-hook`):
//! - `run`      — one-shot init; logs a banner.
//! - `describe` — advertises the `routing.dial` capability.
//! - `alloc`    — bump allocator the host writes `invoke` args into.
//! - `invoke`   — dispatcher. Method `dial` reads the target SIP URI
//!                from `params` and calls `smiths::originate`.

#![no_std]
#![no_main]

#[link(wasm_import_module = "smiths")]
unsafe extern "C" {
    /// `smiths::log(ptr, len)` — UTF-8 log line, tagged with our name.
    fn log(ptr: *const u8, len: usize);
    /// `smiths::originate(ptr, len) -> i32` — place an outbound call
    /// to the SIP URI at `[ptr, ptr+len)`. Returns 0 on dispatch.
    /// Gated by the `send_sip` permission in `plugin.toml`.
    fn originate(ptr: *const u8, len: usize) -> i32;
}

fn host_log(msg: &str) {
    // SAFETY: host validates ptr/len against guest memory bounds.
    unsafe { log(msg.as_ptr(), msg.len()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn run() {
    host_log("sip-client: call-control plugin loaded");
}

static DESCRIBE_JSON: &[u8] = br#"{"capability":"routing.dial","plugin":"sip-client","abi":"1.0","description":"Place outbound SIP calls via smiths::originate. Method `dial` takes a SIP URI in params."}"#;

#[unsafe(no_mangle)]
pub extern "C" fn describe() -> i64 {
    let ptr = DESCRIBE_JSON.as_ptr() as u32 as i64;
    let len = DESCRIBE_JSON.len() as u32 as i64;
    (ptr << 32) | len
}

// ---- bump allocator (host writes invoke args here) -----------------

const BUMP_CAP: usize = 4096;
static mut BUMP: [u8; BUMP_CAP] = [0u8; BUMP_CAP];
static mut BUMP_POS: usize = 0;

/// # Safety
/// Single-threaded WASM guest — no concurrent callers touch the bump
/// pointer. Traps on a negative `len` or exhaustion.
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

static OK: &[u8] = br#"{"result":"dialing"}"#;
static ACK: &[u8] = br#"{"result":"ack"}"#;
static ERR_EMPTY: &[u8] = br#"{"error":"dial: empty target URI"}"#;

/// Trim one layer of surrounding ASCII double-quotes, so a `params`
/// value that arrived as a JSON string (`"sip:bob@host"`) yields the
/// bare URI. Leaves an already-bare value untouched.
fn unquote(b: &[u8]) -> &[u8] {
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &b[1..b.len() - 1]
    } else {
        b
    }
}

/// Dispatcher. Branches on `method`:
/// - `dial` → `params` is the target SIP URI (typically a JSON
///   string); place the call via `smiths::originate`.
/// - anything else (e.g. `on_dialog_created`, delivered by the
///   engine's call-event hook) → the brain "saw" a call: log the
///   params (which carry the `call_id`) and ack. A richer plugin
///   would parse `params` and decide whether to originate / route.
#[unsafe(no_mangle)]
pub extern "C" fn invoke(
    method_ptr: i32,
    method_len: i32,
    params_ptr: i32,
    params_len: i32,
) -> i64 {
    let method = unsafe {
        core::slice::from_raw_parts(method_ptr as usize as *const u8, method_len as usize)
    };
    let params = unsafe {
        core::slice::from_raw_parts(params_ptr as usize as *const u8, params_len as usize)
    };

    if method == b"dial" {
        let target = unquote(params);
        if target.is_empty() {
            return pack(ERR_EMPTY);
        }
        // SAFETY: `target` points into our own guest memory; the host
        // bounds-checks ptr/len and dispatches the call asynchronously.
        unsafe { originate(target.as_ptr(), target.len()) };
        return pack(OK);
    }

    // Call-lifecycle event (auto-invoked by the engine). Prove the
    // brain observed the call by logging the params payload.
    host_log("sip-client: on_dialog_created");
    if let Ok(s) = core::str::from_utf8(params) {
        host_log(s);
    }
    pack(ACK)
}

/// Pack a static response slice into the `(ptr << 32) | len` return.
fn pack(body: &[u8]) -> i64 {
    let ptr = body.as_ptr() as u32 as i64;
    let len = body.len() as u32 as i64;
    (ptr << 32) | len
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
