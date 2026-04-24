//! Slice 5.2 / P18 Small 1 — wire-format throughput benchmark.
//!
//! Not criterion; just a self-timed loop. Compares prost vs the
//! hand-rolled flat layout on the RTP hot path. Asserts the flat
//! layout's encode+decode round trip is at least 2× faster than
//! prost's on a 160-byte PCMU-shaped frame — the slice-level
//! acceptance criterion.
//!
//! Marked `#[ignore]` so CI doesn't pay the cost on every run;
//! `cargo test -p smiths-proto --test wire_format_throughput --
//! --ignored --nocapture` exercises it.

#![allow(
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::uninlined_format_args
)]

use std::time::{Duration, Instant};

use smiths_proto::{FlatbuffersWireFormat, ProtoWireFormat, RtpFrame, WireFormat, WireFormatKind};

const ITERATIONS: usize = 50_000;

fn sample_frame() -> RtpFrame {
    RtpFrame {
        call_id: "abc@smiths.local".into(),
        ssrc: 0xDEAD_BEEF,
        sequence: 12_345,
        timestamp: 98_765_432,
        payload_type: 0,
        direction: "a_to_b".into(),
        payload: vec![0xAB; 160], // one PCMU frame at 20 ms / 8 kHz
    }
}

fn round_trip<W: WireFormat>(wire: &W, frame: &RtpFrame, n: usize) -> Duration {
    // Warm-up so the first allocation doesn't skew timing.
    for _ in 0..1024 {
        let bytes = wire.encode_rtp_frame(frame);
        let _ = wire.decode_rtp_frame(&bytes).unwrap();
    }
    let start = Instant::now();
    for _ in 0..n {
        let bytes = wire.encode_rtp_frame(frame);
        let back = wire.decode_rtp_frame(&bytes).unwrap();
        // Defeat dead-code-elimination: read a field that forces
        // the decode path to actually run.
        assert_eq!(back.ssrc, frame.ssrc);
    }
    start.elapsed()
}

#[test]
#[ignore = "bench; run with --ignored --nocapture to see numbers"]
fn flatbuffers_round_trip_is_at_least_2x_prost() {
    let frame = sample_frame();
    let proto = ProtoWireFormat;
    let flat = FlatbuffersWireFormat;

    assert_eq!(proto.kind(), WireFormatKind::Proto);
    assert_eq!(flat.kind(), WireFormatKind::Flatbuffers);

    let proto_elapsed = round_trip(&proto, &frame, ITERATIONS);
    let flat_elapsed = round_trip(&flat, &frame, ITERATIONS);

    let proto_ns_per = proto_elapsed.as_nanos() / ITERATIONS as u128;
    let flat_ns_per = flat_elapsed.as_nanos() / ITERATIONS as u128;
    let speedup = proto_ns_per as f64 / flat_ns_per.max(1) as f64;
    let proto_bytes = proto.encode_rtp_frame(&frame).len();
    let flat_bytes = flat.encode_rtp_frame(&frame).len();

    println!(
        "[bench] rtp frame n={n} iters\n\
         [bench]   proto       : {proto_ns} ns/iter  ({proto_bytes} bytes/msg)\n\
         [bench]   flatbuffers : {flat_ns} ns/iter  ({flat_bytes} bytes/msg)\n\
         [bench]   speedup     : {speedup:.2}×",
        n = ITERATIONS,
        proto_ns = proto_ns_per,
        flat_ns = flat_ns_per,
        proto_bytes = proto_bytes,
        flat_bytes = flat_bytes,
    );

    assert!(
        speedup >= 2.0,
        "slice acceptance requires ≥2× speedup; got {speedup:.2}×. \
         proto={proto_ns_per} ns, flat={flat_ns_per} ns"
    );
}
