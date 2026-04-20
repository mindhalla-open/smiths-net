//! Fuzz target: feed arbitrary bytes through every parser reachable
//! from the UAS's `handle_datagram` hot path.
//!
//! Stages:
//! 1. `rsip::SipMessage::try_from` — the guard the UAS runs first.
//! 2. `summarize_request` — our hand-rolled request-summary parser
//!    (branch / Call-ID / From/To tags / Content-Type / Authorization).
//! 3. `extract_via_branch` — the response-router's branch extractor.
//!
//! Asserts no panic / no unwind / no OOB across every reachable
//! input. Discovered crashes land under `fuzz/artifacts/sip_parser/`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rsip::SipMessage::try_from(data);
    smiths_sip::uas::__fuzz::summarize_request(data);
    smiths_sip::uas::__fuzz::extract_via_branch(data);
});
