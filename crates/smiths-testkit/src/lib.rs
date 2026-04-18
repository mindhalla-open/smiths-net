//! Integration-test helpers used by the smiths-net test suites.
//!
//! Nothing here is production code. The goal is to give tests a tiny
//! purpose-built UAC, a byte-level RTP packer, the standard G.711 μ-law
//! codec, a minimal WAV writer, and a signal generator so end-to-end
//! audio scenarios don't need external tooling.

pub mod codec;
pub mod rtp;
pub mod signal;
pub mod uac;
pub mod wav;

pub use codec::{pcm16_to_pcmu, pcmu_to_pcm16};
pub use rtp::RtpPacket;
pub use uac::TestUac;
