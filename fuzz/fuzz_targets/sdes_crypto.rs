//! Fuzz target: drive `SdesCrypto::parse` with arbitrary bytes.
//!
//! Asserts the SDES `a=crypto:` parser (RFC 4568 §9.1) never panics,
//! reads out of bounds, or over-allocates on adversarial input. The
//! parser handles several formats in sequence:
//!
//! 1. `a=crypto:` prefix detection.
//! 2. Whitespace-separated token splitting (tag, suite, key-params).
//! 3. Suite lookup against the engine's supported table.
//! 4. `inline:` + base64 decode of the key-material blob.
//! 5. `|lifetime|mki:mki_len` suffix handling.
//!
//! Each stage is a potential crash surface — a corrupt base64 body
//! can cause OOM on naive decoders, a stray `|` can skew the split,
//! and UTF-8 handling on the caller's `&str` matters once we accept
//! raw bytes. Round-tripping through `.parse()` is enough to catch
//! all of these.
//!
//! Also exercises `SessionDescription::parse` on the same byte slice
//! — an `a=crypto:` line inside a full SDP document takes a
//! different code path (the line-by-line dispatcher logs + skips
//! bad crypto rather than failing the whole document; fuzzing
//! confirms that soft-fail path never panics either).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `SdesCrypto::parse` takes `&str`. Skip non-UTF-8 inputs — they're
    // the domain of other targets, and forcing lossy conversion here
    // would hide genuine `&str`-specific bugs.
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = smiths_sdp::SdesCrypto::parse(text);

    // Also feed the bytes through the full SDP parser so the
    // `a=crypto:` branch inside `apply_attribute` gets exercised from
    // the line-oriented dispatcher side.
    let _ = smiths_sdp::SessionDescription::parse(text);
});
