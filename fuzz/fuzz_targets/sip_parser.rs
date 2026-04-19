//! Fuzz target: feed arbitrary bytes to the SIP parser the UAS uses.
//!
//! Asserts no panic / no unwind across every reachable input. Discovered
//! crashes land under `fuzz/artifacts/sip_parser/` as a minimized byte
//! corpus entry, which the library unit tests can replay.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Primary parse — the exact call the UAS makes on every inbound
    // datagram.
    let _ = rsip::SipMessage::try_from(data);
});
