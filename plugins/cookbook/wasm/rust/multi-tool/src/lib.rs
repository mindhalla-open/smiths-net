//! Multi-tool WASM plugin — cookbook recipe.
//!
//! Demonstrates how a single plugin can expose **multiple tools**
//! through the `describe` + `invoke` ABI. The engine's MCP server
//! presents each tool as a separate callable endpoint.
//!
//! Tools provided:
//!   - `text.upper` — converts input text to uppercase.
//!   - `text.reverse` — reverses the input string.
//!   - `text.length` — returns the byte length of the input.

#![no_std]
#![no_main]

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
    host_log("multi-tool: plugin loaded");
}

// ---------------------------------------------------------------------------
// describe — THREE capabilities in one descriptor
// ---------------------------------------------------------------------------

static DESCRIBE_JSON: &[u8] = br#"[{"capability":"text.upper","plugin":"multi-tool","abi":"1.0","description":"Convert text to UPPERCASE."},{"capability":"text.reverse","plugin":"multi-tool","abi":"1.0","description":"Reverse a string."},{"capability":"text.length","plugin":"multi-tool","abi":"1.0","description":"Return byte length of input."}]"#;

#[unsafe(no_mangle)]
pub extern "C" fn describe() -> i64 {
    let ptr = DESCRIBE_JSON.as_ptr() as u32 as i64;
    let len = DESCRIBE_JSON.len() as u32 as i64;
    (ptr << 32) | len
}

// ---------------------------------------------------------------------------
// alloc — bump allocator
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
// invoke — multi-tool dispatcher
// ---------------------------------------------------------------------------

static mut RESPONSE: [u8; 1024] = [0u8; 1024];

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

    // Extract the "input" value from params JSON (simplified parser).
    let input = extract_input(params);

    let result = match method {
        b"text.upper" => upper(input),
        b"text.reverse" => reverse(input),
        b"text.length" => length(input),
        _ => build_error(b"unknown tool"),
    };

    let ptr = result.as_ptr() as u32 as i64;
    let len = result.len() as u32 as i64;
    (ptr << 32) | len
}

/// Uppercase ASCII bytes in-place, return JSON envelope.
fn upper(input: &[u8]) -> &'static [u8] {
    let prefix = br#"{"result":""#;
    let suffix = br#""}"#;
    let total = prefix.len() + input.len() + suffix.len();

    unsafe {
        let buf = &mut RESPONSE[..total];
        let mut c = 0;
        buf[c..c + prefix.len()].copy_from_slice(prefix);
        c += prefix.len();
        for &b in input {
            buf[c] = if b >= b'a' && b <= b'z' { b - 32 } else { b };
            c += 1;
        }
        buf[c..c + suffix.len()].copy_from_slice(suffix);
        &RESPONSE[..total]
    }
}

/// Reverse bytes, return JSON envelope.
fn reverse(input: &[u8]) -> &'static [u8] {
    let prefix = br#"{"result":""#;
    let suffix = br#""}"#;
    let total = prefix.len() + input.len() + suffix.len();

    unsafe {
        let buf = &mut RESPONSE[..total];
        let mut c = 0;
        buf[c..c + prefix.len()].copy_from_slice(prefix);
        c += prefix.len();
        for i in (0..input.len()).rev() {
            buf[c] = input[i];
            c += 1;
        }
        buf[c..c + suffix.len()].copy_from_slice(suffix);
        &RESPONSE[..total]
    }
}

/// Return byte length as JSON.
fn length(input: &[u8]) -> &'static [u8] {
    let len_val = input.len();
    // Format: {"result":NNN}
    let prefix = br#"{"result":"#;
    let suffix = b"}";

    // Convert length to decimal digits.
    let mut digits = [0u8; 10];
    let mut dlen = 0;
    let mut n = len_val;
    if n == 0 {
        digits[0] = b'0';
        dlen = 1;
    } else {
        while n > 0 {
            digits[dlen] = b'0' + (n % 10) as u8;
            dlen += 1;
            n /= 10;
        }
        // Reverse digits.
        let mut i = 0;
        let mut j = dlen - 1;
        while i < j {
            let tmp = digits[i];
            digits[i] = digits[j];
            digits[j] = tmp;
            i += 1;
            j -= 1;
        }
    }

    let total = prefix.len() + dlen + suffix.len();
    unsafe {
        let buf = &mut RESPONSE[..total];
        let mut c = 0;
        buf[c..c + prefix.len()].copy_from_slice(prefix);
        c += prefix.len();
        buf[c..c + dlen].copy_from_slice(&digits[..dlen]);
        c += dlen;
        buf[c..c + suffix.len()].copy_from_slice(suffix);
        &RESPONSE[..total]
    }
}

fn build_error(msg: &[u8]) -> &'static [u8] {
    let prefix = br#"{"error":""#;
    let suffix = br#""}"#;
    let total = prefix.len() + msg.len() + suffix.len();
    unsafe {
        let buf = &mut RESPONSE[..total];
        let mut c = 0;
        buf[c..c + prefix.len()].copy_from_slice(prefix);
        c += prefix.len();
        buf[c..c + msg.len()].copy_from_slice(msg);
        c += msg.len();
        buf[c..c + suffix.len()].copy_from_slice(suffix);
        &RESPONSE[..total]
    }
}

/// Simplified JSON "input" field extractor.
/// Looks for `"input":"<value>"` and returns the value bytes.
fn extract_input(params: &[u8]) -> &[u8] {
    // Find `"input":"` pattern.
    let needle = br#""input":""#;
    let mut i = 0;
    while i + needle.len() < params.len() {
        if &params[i..i + needle.len()] == needle {
            let start = i + needle.len();
            let mut end = start;
            while end < params.len() && params[end] != b'"' {
                end += 1;
            }
            return &params[start..end];
        }
        i += 1;
    }
    b""
}

// ---------------------------------------------------------------------------
// panic handler
// ---------------------------------------------------------------------------

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
