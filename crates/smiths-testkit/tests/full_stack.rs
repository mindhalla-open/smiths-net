//! End-to-end: TLS SIP + SRTP + WASM + sidecar + metrics scrape +
//! drain-during-call, all in one scenario.
//!
//! This is the "does the whole engine actually work together?" test
//! that gates a tagged release. Everything else in `smiths-testkit`
//! exercises a single subsystem; this one stitches them together and
//! asserts invariants at the seams.
//!
//! ## Scenario steps
//!
//! 1. Boot the engine with TLS SIP + metrics + WASM loader + an
//!    echo sidecar plugin.
//! 2. Two fake UAs (`FakeUac` + `FakeUas`) rendezvous through a
//!    `RTP/SAVP` INVITE; verify SRTP keys installed on each leg.
//! 3. Walk an RTP frame through the bridge; a WASM plugin
//!    inspects/passes it; a sidecar plugin logs it via the event bus.
//! 4. Scrape `/metrics`; assert `sip_dialogs_active`,
//!    `rtp_packets_forwarded`, `plugin_invocations_total` all moved.
//! 5. Send SIGTERM-equivalent drain; assert new INVITEs get `503 +
//!    Retry-After: 0` and the live dialog completes normally.
//!
//! ## Status
//!
//! **Placeholder / smoke.** Slice 1.9 establishes the test file and
//! the `cargo test --test full_stack` invocation the CI release
//! pipeline depends on. The individual subsystem tests already
//! exercise each piece in isolation (`sdp_srtp.rs`, `wasm::engine`,
//! `sidecar::supervisor`, `drain.rs`); this file gains steps over
//! time rather than landing in one go.

use std::time::Duration;

/// Smoke: the testkit is wired correctly + subordinate crates link.
/// Actual stitched flow lands in follow-on commits as each step grows
/// a stable assertion surface.
#[tokio::test(flavor = "multi_thread")]
async fn full_stack_smoke_all_crates_link() {
    // Touch every crate the full flow needs. If any of these imports
    // start failing, the test fails fast with a clear compile error
    // rather than an assertion much further down.
    let _bus = smiths_core::EventBus::new(16);
    let _negotiator = smiths_sdp::Negotiator::with_default_codecs(std::net::IpAddr::V4(
        std::net::Ipv4Addr::LOCALHOST,
    ));
    let _cert = smiths_core::SelfSignedCert::generate("full-stack-smoke")
        .expect("self-signed cert mint must succeed under test");

    // Future steps land here — when each arrives it should assert
    // the specific invariant, not just that types exist.
    //
    // TODO(slice-1.9+):
    //   - boot engine with TLS SIP + metrics.
    //   - run TWO fake UAs through an RTP/SAVP rendezvous.
    //   - load an echo WASM plugin; touch the RTP frame path.
    //   - spawn a stdio sidecar; exercise the bus → plugin hop.
    //   - scrape /metrics; assert counters moved.
    //   - drain during call; verify 503 for new INVITEs.

    // Heartbeat — if this `sleep` ever hangs, tokio's test runtime
    // is broken and we'd notice long before the full-stack steps do.
    tokio::time::sleep(Duration::from_millis(1)).await;
}
