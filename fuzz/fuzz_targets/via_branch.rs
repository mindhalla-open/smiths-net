//! Fuzz target: structured Via-header inputs for the branch parsers.
//!
//! `sip_parser` throws raw bytes at the whole datagram path, so most
//! of its budget goes to the request line and to `rsip`'s message
//! guard. This target builds messages whose only variable part is
//! the Via header block, which lands the mutator's effort on the
//! three places a Via branch is parsed:
//!
//! 1. `smiths_sip::uas::__fuzz::extract_via_branch` — the response
//!    router's extractor (first `Via:` / `v:` line, `;branch=` value).
//! 2. `smiths_sip::uas::__fuzz::summarize_request` — the request
//!    summary parser, which carries its own copy of the branch scan.
//! 3. `rsip`'s typed `Via` parser, applied to each generated header
//!    value on its own.
//!
//! The generator keeps the header-name / separator grammar mostly
//! valid (so inputs get past the cheap prefix checks) and leaves the
//! rest — sent-protocol, host, the branch token, trailing parameters,
//! line endings, header folding — to the fuzzer, including non-UTF-8
//! bytes and mixed CR / LF. Any panic, out-of-bounds slice, or
//! unbounded allocation is a finding; crashes land under
//! `fuzz/artifacts/via_branch/`.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use rsip::headers::{ToTypedHeader, UntypedHeader};

/// Upper bound on generated Via lines per message: enough for the
/// "first matching line wins" logic plus stacked / folded variants,
/// small enough to keep every iteration short.
const MAX_VIAS: usize = 8;
/// Upper bound on any free-form byte field, so one input cannot
/// dominate an iteration with a multi-kilobyte host.
const MAX_FIELD: usize = 160;

const START_LINES: &[&[u8]] = &[
    b"INVITE sip:bob@example.com SIP/2.0\r\n",
    b"BYE sip:bob@example.com SIP/2.0\r\n",
    b"REGISTER sip:example.com SIP/2.0\r\n",
    b"OPTIONS sip:example.com SIP/2.0\r\n",
    b"SIP/2.0 200 OK\r\n",
    b"SIP/2.0 100 Trying\r\n",
];

const SENT_PROTOCOLS: &[&[u8]] = &[
    b"SIP/2.0/UDP",
    b"SIP/2.0/TCP",
    b"SIP/2.0/TLS",
    b"SIP/2.0/WSS",
    b"SIP/2.0/",
    b"sip/2.0/udp",
    b"",
];

/// How the `branch` parameter is introduced. Only the first form is
/// what RFC 3261 section 20.42 spells out; the rest probe the tolerant
/// scans in the UAS (a lowercase search for `;branch=`) and the strict
/// tokenizer in `rsip`.
const BRANCH_INTROS: &[&[u8]] = &[
    b";branch=",
    b";BRANCH=",
    b"; branch=",
    b";branch =",
    b";branch",
    b",branch=",
    b";rport;branch=",
    b";branch=;branch=",
];

/// Line terminators, including bare CR / LF and the two folding
/// continuations (`CRLF` + whitespace).
const LINE_ENDINGS: &[&[u8]] = &[b"\r\n", b"\n", b"\r", b"\r\n ", b"\r\n\t", b""];

fn pick<'a>(table: &'a [&'a [u8]], idx: u8) -> &'a [u8] {
    table[usize::from(idx) % table.len()]
}

#[derive(Arbitrary, Debug)]
struct ViaLine {
    /// `v:` (compact form) instead of `Via:`.
    compact: bool,
    /// Header-name case; the UAS scan is case-insensitive.
    upper: bool,
    /// Linear whitespace after the colon.
    lws: bool,
    protocol: u8,
    host: Vec<u8>,
    /// `None` = no branch parameter at all.
    branch: Option<Vec<u8>>,
    intro: u8,
    trailing: Vec<u8>,
    ending: u8,
}

impl ViaLine {
    /// The header value only (everything after the colon), which is
    /// what `rsip`'s typed parser consumes.
    fn value(&self) -> Vec<u8> {
        let mut v = Vec::new();
        if self.lws {
            v.push(b' ');
        }
        v.extend_from_slice(pick(SENT_PROTOCOLS, self.protocol));
        v.push(b' ');
        v.extend(self.host.iter().take(MAX_FIELD));
        if let Some(branch) = &self.branch {
            v.extend_from_slice(pick(BRANCH_INTROS, self.intro));
            v.extend(branch.iter().take(MAX_FIELD));
        }
        v.extend(self.trailing.iter().take(MAX_FIELD));
        v
    }

    fn write_line(&self, out: &mut Vec<u8>) {
        let name: &[u8] = match (self.compact, self.upper) {
            (true, true) => b"V:",
            (true, false) => b"v:",
            (false, true) => b"VIA:",
            (false, false) => b"Via:",
        };
        out.extend_from_slice(name);
        out.extend_from_slice(&self.value());
        out.extend_from_slice(pick(LINE_ENDINGS, self.ending));
    }
}

#[derive(Arbitrary, Debug)]
struct Input {
    start_line: u8,
    vias: Vec<ViaLine>,
    /// Raw bytes after the Via block and before the blank line, so
    /// header ordering and folding interplay get covered too.
    other_headers: Vec<u8>,
    body: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let mut msg = Vec::new();
    msg.extend_from_slice(pick(START_LINES, input.start_line));
    msg.extend_from_slice(b"Call-ID: fuzz@example.com\r\nCSeq: 1 INVITE\r\n");
    let vias = input.vias.iter().take(MAX_VIAS);
    for via in vias.clone() {
        via.write_line(&mut msg);
    }
    msg.extend(input.other_headers.iter().take(MAX_FIELD));
    msg.extend_from_slice(b"\r\n\r\n");
    msg.extend(input.body.iter().take(MAX_FIELD));

    smiths_sip::uas::__fuzz::extract_via_branch(&msg);
    smiths_sip::uas::__fuzz::summarize_request(&msg);
    let _ = rsip::SipMessage::try_from(msg.as_slice());

    for via in vias {
        if let Ok(value) = std::str::from_utf8(&via.value()) {
            let _ = rsip::headers::Via::new(value).typed();
        }
    }
});
