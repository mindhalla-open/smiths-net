//! Call-control WASM plugin — cookbook recipe.
//!
//! Demonstrates the SIP call-control surface a "brain" plugin uses:
//! - `smiths::originate` — place an outbound call (needs the
//!   `send_sip` permission).
//! - the `on_dialog_created` invoke method — auto-delivered by the
//!   engine's call-event hook when a dialog goes live, so the plugin
//!   can react to inbound calls without an MCP request.
//!
//! It carries NO media — a WASM sandbox has no sockets or audio
//! devices. The voice rides the engine + a native client
//! (`smiths-softphone`); this plugin is the decision-maker.

#![no_std]
#![no_main]

#[link(wasm_import_module = "smiths")]
unsafe extern "C" {
    /// `smiths::log(ptr, len)` — UTF-8 log line, tagged with our name.
    fn log(ptr: *const u8, len: usize);
    /// `smiths::originate(ptr, len) -> i32` — place an outbound call to
    /// the SIP URI at `[ptr, ptr+len)`. Returns 0 on dispatch. Gated by
    /// the `send_sip` permission in `plugin.toml`.
    fn originate(ptr: *const u8, len: usize) -> i32;
}

fn host_log(msg: &str) {
    // SAFETY: the host bounds-checks ptr/len against guest memory.
    unsafe { log(msg.as_ptr(), msg.len()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn run() {
    host_log("call-control: plugin loaded");
}

static DESCRIBE_JSON: &[u8] = br#"{"capability":"routing.dial","plugin":"call-control","abi":"1.0","description":"Place outbound SIP calls via smiths::originate; method `dial` takes a SIP URI."}"#;

#[unsafe(no_mangle)]
pub extern "C" fn describe() -> i64 {
    pack(DESCRIBE_JSON)
}

// ---- bump allocator (host writes invoke args here) -----------------

const BUMP_CAP: usize = 4096;
static mut BUMP: [u8; BUMP_CAP] = [0u8; BUMP_CAP];
static mut BUMP_POS: usize = 0;

/// # Safety
/// Single-threaded WASM guest — no concurrent callers. Traps on a
/// negative `len` or exhaustion.
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

/// Trim one layer of surrounding ASCII double-quotes so a `params`
/// value that arrived as a JSON string (`"sip:bob@host"`) yields the
/// bare URI.
fn unquote(b: &[u8]) -> &[u8] {
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &b[1..b.len() - 1]
    } else {
        b
    }
}

/// Dispatcher. Branches on `method`:
/// - `dial` → `params` is the target SIP URI; call `smiths::originate`.
/// - anything else (e.g. `on_dialog_created`) → an inbound call event;
///   log the payload (which carries the `call_id`) and ack.
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

    host_log("call-control: on_dialog_created");
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
