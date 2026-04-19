//! Media plane for smiths-net.
//!
//! Today: a UDP bridge that parses RTP headers, rewrites SSRC
//! per-leg, and forwards packets between the two sides of a call.
//! Jitter buffer, RTCP sender/receiver reports, and per-frame plugin
//! hooks are follow-up work.

pub mod bridge;
pub mod fabric;
pub mod port_allocator;

pub use bridge::{Bridge, Leg};
pub use fabric::UdpMediaFabric;
pub use port_allocator::{PortPair, allocate_rtp_rtcp_pair};

// Codec + RTP packet types live in `smiths-core` (pure math, no
// deps). Re-exported here so existing `smiths-media::*` paths keep
// working.
pub use smiths_core::{RtpPacket, linear_to_ulaw, pcm16_to_pcmu, pcmu_to_pcm16, ulaw_to_linear};
pub use smiths_core::{codec, rtp};
