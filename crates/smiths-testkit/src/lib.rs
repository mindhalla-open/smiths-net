//! Integration-test helpers used by the smiths-net test suites.
//!
//! Nothing here is production code. The goal is to give tests a tiny
//! purpose-built UAC + UAS pair, a byte-level RTP packer, the standard
//! G.711 μ-law codec, a minimal WAV writer, and a signal generator so
//! end-to-end audio and signaling scenarios don't need external tools.

pub mod codec;
pub mod fake_uac;
pub mod fake_uas;
pub mod rtp;
pub mod signal;
pub mod wav;

pub use codec::{pcm16_to_pcmu, pcmu_to_pcm16};
pub use fake_uac::FakeUac;
pub use fake_uas::{CapturedRequest, FakeUas};
pub use rtp::RtpPacket;
