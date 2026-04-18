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
