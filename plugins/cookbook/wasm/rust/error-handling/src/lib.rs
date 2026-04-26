//! Error-handling WASM plugin — cookbook recipe.
//!
//! Demonstrates how a WASM plugin should report structured errors
//! back to the host engine using the JSON-RPC error envelope.
//!
//! Patterns shown:
//!   - Validate input before processing.
//!   - Return error codes (-32602 invalid params, -32603 internal).
//!   - Distinguish "tool not found" from "bad arguments".
//!   - Structured error data with code + message.

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

// ---------------------------------------------------------------------------
// Host imports
// ---------------------------------------------------------------------------

#[link(wasm_import_module = "smiths")]
unsafe extern "C" {
    fn log(ptr: *const u8, len: usize);
}

fn host_log(msg: &str) {
    unsafe { log(msg.as_ptr(), msg.len()); }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn run() {
    host_log("error-handling: plugin loaded");
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

static DESCRIBE_JSON: &[u8] = br#"{"capability":"math.divide","plugin":"error-handling","abi":"1.0","description":"Integer division with proper error handling."}"#;

#[unsafe(no_mangle)]
pub extern "C" fn describe() -> i64 {
    let ptr = DESCRIBE_JSON.as_ptr() as u32 as i64;
    let len = DESCRIBE_JSON.len() as u32 as i64;
    (ptr << 32) | len
}

// ---------------------------------------------------------------------------
// alloc
// ---------------------------------------------------------------------------

const BUMP_CAP: usize = 4096;
static mut BUMP: [u8; BUMP_CAP] = [0u8; BUMP_CAP];
static mut BUMP_POS: usize = 0;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn alloc(len: i32) -> i32 {
    unsafe {
        let req = len as usize;
        let pos = BUMP_POS;
        let end = pos + req;
        if end > BUMP_CAP {
            core::arch::wasm32::unreachable();
        }
        BUMP_POS = end;
        let base = (&raw const BUMP).cast::<u8>() as usize;
        (base + pos) as i32
    }
}

// ---------------------------------------------------------------------------
// invoke — division with error handling
// ---------------------------------------------------------------------------

static mut RESPONSE: [u8; 512] = [0u8; 512];

/// JSON-RPC error codes.
const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;
const INTERNAL_ERROR: i32 = -32603;

#[unsafe(no_mangle)]
pub extern "C" fn invoke(
    method_ptr: i32,
    method_len: i32,
    params_ptr: i32,
    params_len: i32,
) -> i64 {
    let method = unsafe {
        core::slice::from_raw_parts(method_ptr as *const u8, method_len as usize)
    };
    let params = unsafe {
        core::slice::from_raw_parts(params_ptr as *const u8, params_len as usize)
    };

    match method {
        b"math.divide" => do_divide(params),
        _ => {
            host_log("error-handling: unknown method");
            pack_response(error_json(METHOD_NOT_FOUND, b"method not found"))
        }
    }
}

fn do_divide(params: &[u8]) -> i64 {
    // Extract "a" and "b" fields.
    let a = extract_number(params, b"\"a\":");
    let b = extract_number(params, b"\"b\":");

    match (a, b) {
        (None, _) | (_, None) => {
            host_log("error-handling: missing or invalid params");
            pack_response(error_json(INVALID_PARAMS, b"missing a or b"))
        }
        (_, Some(0)) => {
            host_log("error-handling: division by zero");
            pack_response(error_json(INTERNAL_ERROR, b"division by zero"))
        }
        (Some(a_val), Some(b_val)) => {
            let result = a_val / b_val;
            pack_response(result_json(result))
        }
    }
}

/// Build `{"result":<N>}`.
fn result_json(val: i32) -> usize {
    let prefix = br#"{"result":"#;
    let suffix = b"}";

    let mut digits = [0u8; 12]; // room for sign + 10 digits
    let dlen = itoa(val, &mut digits);

    unsafe {
        let total = prefix.len() + dlen + suffix.len();
        let buf = &mut RESPONSE[..total];
        let mut c = 0;
        buf[c..c + prefix.len()].copy_from_slice(prefix);
        c += prefix.len();
        buf[c..c + dlen].copy_from_slice(&digits[..dlen]);
        c += dlen;
        buf[c..c + suffix.len()].copy_from_slice(suffix);
        total
    }
}

/// Build `{"error":{"code":<C>,"message":"<M>"}}`.
fn error_json(code: i32, msg: &[u8]) -> usize {
    let p1 = br#"{"error":{"code":"#;
    let p2 = br#","message":""#;
    let p3 = br#""}}"#;

    let mut digits = [0u8; 12];
    let dlen = itoa(code, &mut digits);

    unsafe {
        let total = p1.len() + dlen + p2.len() + msg.len() + p3.len();
        let buf = &mut RESPONSE[..total];
        let mut c = 0;
        buf[c..c + p1.len()].copy_from_slice(p1);
        c += p1.len();
        buf[c..c + dlen].copy_from_slice(&digits[..dlen]);
        c += dlen;
        buf[c..c + p2.len()].copy_from_slice(p2);
        c += p2.len();
        buf[c..c + msg.len()].copy_from_slice(msg);
        c += msg.len();
        buf[c..c + p3.len()].copy_from_slice(p3);
        total
    }
}

fn pack_response(total: usize) -> i64 {
    let ptr = unsafe { RESPONSE.as_ptr() } as u32 as i64;
    let len = total as u32 as i64;
    (ptr << 32) | len
}

/// Minimal i32 → ASCII. Returns the number of digits written.
fn itoa(mut val: i32, buf: &mut [u8; 12]) -> usize {
    let negative = val < 0;
    if negative {
        val = -val;
    }

    let mut tmp = [0u8; 10];
    let mut tlen = 0;
    if val == 0 {
        tmp[0] = b'0';
        tlen = 1;
    } else {
        while val > 0 {
            tmp[tlen] = b'0' + (val % 10) as u8;
            val /= 10;
            tlen += 1;
        }
    }

    let mut pos = 0;
    if negative {
        buf[0] = b'-';
        pos = 1;
    }
    for i in (0..tlen).rev() {
        buf[pos] = tmp[i];
        pos += 1;
    }
    pos
}

/// Extract a number value after a given key prefix in JSON.
fn extract_number(params: &[u8], key: &[u8]) -> Option<i32> {
    let mut i = 0;
    while i + key.len() < params.len() {
        if &params[i..i + key.len()] == key {
            let start = i + key.len();
            // Skip whitespace.
            let mut s = start;
            while s < params.len() && (params[s] == b' ' || params[s] == b'\t') {
                s += 1;
            }
            // Parse digits.
            let negative = s < params.len() && params[s] == b'-';
            if negative {
                s += 1;
            }
            let mut val: i32 = 0;
            let mut found = false;
            while s < params.len() && params[s] >= b'0' && params[s] <= b'9' {
                val = val * 10 + (params[s] - b'0') as i32;
                s += 1;
                found = true;
            }
            if found {
                return Some(if negative { -val } else { val });
            }
            return None;
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// panic handler
// ---------------------------------------------------------------------------

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
