//! Media plane for smiths-net.
//!
//! Today: a UDP bridge that parses RTP headers, rewrites SSRC
//! per-leg, forwards packets between the two sides of a call, and
//! emits periodic RTCP Sender Reports with live packet/byte/jitter
//! stats. Jitter buffer and per-frame plugin hooks are follow-up work.

pub mod bridge;
pub mod fabric;
pub mod port_allocator;
pub mod rtcp;
pub mod rtp_stats;

pub use bridge::{Bridge, BridgeConfig, Leg, RtcpLeg};
pub use fabric::UdpMediaFabric;
pub use port_allocator::{PortPair, allocate_rtp_rtcp_pair};
pub use rtp_stats::{StreamStats, StreamStatsSnapshot};

// Codec + RTP packet types live in `smiths-core` (pure math, no
// deps). Re-exported here so existing `smiths-media::*` paths keep
// working.
pub use smiths_core::{RtpPacket, linear_to_ulaw, pcm16_to_pcmu, pcmu_to_pcm16, ulaw_to_linear};
pub use smiths_core::{codec, rtp};
