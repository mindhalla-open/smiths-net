//! Headless-Chromium WebRTC test harness.
//!
//! Gated behind the `browser` feature so CI environments without a
//! Chrome/Chromium binary can still run the rest of the test suite
//! unconditionally. Enable with `cargo test -p smiths-testkit
//! --features browser`.
//!
//! ## What the harness does
//!
//! 1. Boots a page against `localhost` serving a small HTML/JS app
//!    (`assets/webrtc_probe.html`) that calls
//!    `RTCPeerConnection.createOffer` and posts the SDP to a control
//!    port the test listens on.
//! 2. The test hands the SDP to a running [`smiths_sip::UasServer`].
//!    The engine answers 200 OK; the browser applies it.
//! 3. ICE + DTLS run end-to-end through `smiths-ice` + `smiths-dtls`.
//! 4. The browser plays a 440 Hz tone into the peer connection;
//!    the test asserts the engine-side RTP flow carries non-zero
//!    audio payload for at least 1 second.
//!
//! The concrete launcher ships in slice 1.5's follow-on once a
//! browser-driver dep (e.g. `chromiumoxide` or `fantoccini`) is
//! chosen. Until then this module holds the **shape** so the surface
//! is stable before the CI knob flips on.

use std::time::Duration;

/// Configuration for a [`WebRtcHarness`] run.
#[derive(Clone, Debug)]
pub struct HarnessConfig {
    /// How long to hold the browser page open while RTP flows.
    pub audio_window: Duration,
    /// `true` to run Chromium headless (typical CI); `false` for
    /// interactive debugging on a developer's machine.
    pub headless: bool,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            audio_window: Duration::from_secs(2),
            headless: true,
        }
    }
}

/// Placeholder harness value — the real runner lands with the
/// browser-driver dependency. Keeps the API frozen so downstream
/// test files can compile against the type today.
#[derive(Debug)]
pub struct WebRtcHarness {
    /// Config the harness was instantiated with.
    pub config: HarnessConfig,
}

impl WebRtcHarness {
    /// Build a harness with the supplied config. Doesn't launch
    /// anything yet — [`Self::run_audio_roundtrip`] is the driver.
    #[must_use]
    pub fn new(config: HarnessConfig) -> Self {
        Self { config }
    }

    /// Drive the browser page, gather RTP stats, return `true` when
    /// audio flowed both ways for the whole `audio_window`.
    ///
    /// Placeholder until the browser-driver dep lands — always
    /// returns `Err(HarnessError::NotImplemented)` so the `#[ignore]`
    /// gate on any test that calls it stays honest.
    // Signature is frozen `async` ahead of the real browser-driving
    // implementation; the stub body has nothing to await yet.
    #[allow(clippy::unused_async)]
    pub async fn run_audio_roundtrip(&self) -> Result<AudioStats, HarnessError> {
        Err(HarnessError::NotImplemented)
    }
}

/// Outcome of one harness run.
#[derive(Clone, Debug)]
pub struct AudioStats {
    /// Packets the engine bridge forwarded browser→SIP.
    pub forward_packets: u64,
    /// Packets the engine bridge forwarded SIP→browser.
    pub reverse_packets: u64,
}

/// Harness errors. Stable enum so test callers can `match` on the
/// kind once the real implementation ships.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Running the harness isn't implemented yet — the
    /// browser-driver dep picks the concrete launcher.
    #[error("webrtc harness not implemented; enable a browser-driver dep")]
    NotImplemented,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_headless_2_second_window() {
        let cfg = HarnessConfig::default();
        assert!(cfg.headless);
        assert_eq!(cfg.audio_window, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn placeholder_run_returns_not_implemented() {
        let h = WebRtcHarness::new(HarnessConfig::default());
        match h.run_audio_roundtrip().await {
            Err(HarnessError::NotImplemented) => {}
            Ok(stats) => panic!("placeholder harness unexpectedly succeeded: {stats:?}"),
        }
    }
}
