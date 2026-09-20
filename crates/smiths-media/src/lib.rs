//! Media plane for smiths-net.
//!
//! - [`bridge`] — the byte-transparent two-leg UDP bridge: rewrites
//!   the SSRC per leg, runs SRTP/SRTCP where negotiated, terminates
//!   RTCP (compound SR + SDES out, peer SR/RR/SDES/BYE in, including
//!   RTCP multiplexed onto the RTP port) and sniffs DTMF.
//! - [`jitter`] — the adaptive playout buffer shared by every path
//!   that decodes audio and re-paces it on its own clock.
//! - [`transcoded`] — the two-leg session that decodes, buffers,
//!   re-paces and re-encodes when the legs speak different codecs.
//! - [`fabric`] — the [`smiths_core::media::MediaFabric`] over UDP
//!   that owns sockets and spawns bridges for the signaling layer.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod bridge;
pub mod dtls;
pub mod fabric;
pub mod jitter;
#[cfg(feature = "pcap")]
pub mod pcap;
pub mod port_allocator;
pub mod prompts;
pub mod rtcp;
pub mod rtp_stats;
pub mod srtp;
pub mod transcoded;

pub use bridge::{Bridge, BridgeConfig, BridgeStats, DtmfSink, Leg, LegSrtp, RtcpLeg};
pub use dtls::{HandshakeOutcome, HandshakeResult, PeerBoundUdp, classify_error};
pub use fabric::UdpMediaFabric;
pub use jitter::{JitterBuffer, JitterConfig, JitterStats};
pub use port_allocator::{PortPair, allocate_rtp_rtcp_pair};
pub use prompts::{Prompt, PromptError, PromptLibrary, encode_wav};
pub use rtp_stats::{StreamStats, StreamStatsSnapshot};
pub use srtp::AesCmHmacSha1_80Transform;
pub use transcoded::{TranscodedLeg, TranscodedSession};

// Codec + RTP packet types live in `smiths-core` (pure math, no
// deps). Re-exported here so existing `smiths-media::*` paths keep
// working.
pub use smiths_core::{RtpPacket, linear_to_ulaw, pcm16_to_pcmu, pcmu_to_pcm16, ulaw_to_linear};
pub use smiths_core::{codec, rtp};
