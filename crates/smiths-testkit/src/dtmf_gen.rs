//! RFC 4733 DTMF test generator — thin re-export of the helper that
//! lives in [`smiths_core::dtmf`] so tests reach for the same
//! implementation the MCP `send_dtmf` tool uses.
//!
//! Keeps the historical `smiths_testkit::dtmf_gen::*` import path
//! working even after the generator moved upstream into `smiths-core`.

pub use smiths_core::dtmf::{
    DTMF_GEN_CLOCK_RATE_HZ as CLOCK_RATE_HZ, DTMF_GEN_FRAME_MS as FRAME_MS,
    DTMF_GEN_FRAME_SAMPLES as FRAME_SAMPLES, generate_keypress,
};

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_core::{DtmfDetector, TelephoneEvent};

    #[test]
    fn generated_stream_decodes_back_to_the_source_digit() {
        let packets = generate_keypress('5', 80, 0x1234, 100, 1_000);
        // 80 ms / 20 ms = 4 intermediates, +3 ends.
        assert_eq!(packets.len(), 7);
        let mut detector = DtmfDetector::new("a", 8_000);
        let mut presses = Vec::new();
        for p in &packets {
            let ev = TelephoneEvent::parse(&p.payload).unwrap();
            if let Some(press) = detector.feed(&ev, p.timestamp) {
                presses.push(press);
            }
        }
        assert_eq!(presses.len(), 1, "exactly one keypress per stream");
        assert_eq!(presses[0].digit, '5');
        assert_eq!(presses[0].duration_ms, 80);
    }

    #[test]
    fn marker_bit_set_on_first_packet_only() {
        let packets = generate_keypress('1', 60, 0x1, 0, 0);
        assert!(packets[0].marker);
        for p in &packets[1..] {
            assert!(!p.marker);
        }
    }

    #[test]
    fn clock_rate_and_frame_ms_are_exposed() {
        // Re-exports resolve at compile time; assertion forces one
        // runtime touch so a mis-rename would break here too.
        assert_eq!(CLOCK_RATE_HZ, 8_000);
        assert_eq!(FRAME_MS, 20);
        assert_eq!(FRAME_SAMPLES, 160);
    }
}
