//! N:N audio conferencing.
//!
//! A conference is N ≥ 2 participants speaking 16-bit PCM. Every
//! 20 ms tick, each participant's **output** is the sum of every
//! *other* participant's input for that tick ("leave-one-out mix"),
//! run through a per-participant AGC so one loud talker doesn't
//! saturate the room. The mixer is pure `[i16]` in, pure `[i16]` out
//! — G.711 encode/decode happens at the edges via `smiths-core`'s
//! codec helpers, exactly like the transcoder crate. Ingress goes
//! through a per-participant `smiths_media::JitterBuffer`, so the
//! mixer tick is the single clock every participant is re-paced on.
//!
//! ## What this crate ships
//!
//! - [`mixer::Mixer`] — the core leave-one-out sum + clipping loop.
//!   Deterministic, allocation-free per frame (the mixer keeps its
//!   accumulator). Unit tested against known-answer fixtures.
//! - [`agc::Agc`] — per-stream automatic gain control. Tracks a
//!   short-window RMS estimate; attenuates when that RMS climbs
//!   past a threshold so three loud participants together don't
//!   clip on a fourth participant's ear.
//! - [`vad::Vad`] + [`vad::EnergyVad`] — voice-activity detection
//!   hook. `EnergyVad` is a straightforward band-less energy
//!   detector; richer detectors (WebRTC-style, neural) are plugin
//!   territory and swap in through the trait.
//! - [`conference::Conference`] — process-wide N-participant state
//!   plus a mixer tick task. Participants join and leave
//!   dynamically; frames enter through a jitter buffer keyed by RTP
//!   sequence number and the tick publishes each participant's
//!   mixed output on a bounded channel the caller drains.
//! - [`fabric::MixerFabric`] — a [`MediaFabric`] implementation
//!   that delegates point-to-point calls to a wrapped
//!   [`UdpMediaFabric`] and layers conference creation / join /
//!   leave on top. The UAS's router picks the fabric per call.
//! - [`registry::ConferenceRegistry`] — in-memory registry MCP
//!   tools ([`smiths_mcp`'s `create_conference`,
//!   `join_conference`, `leave_conference`](
//!   crate::registry::InMemoryConferenceRegistry)) interact with.
//!
//! ## What this crate does NOT do
//!
//! - **Codec diversity inside one conference.** The mixer wants
//!   a common PCM16 sample rate and the participant session speaks
//!   PCMU only. Heterogeneous codec mixes (one Opus participant,
//!   one PCMU participant) would route through `smiths-transcode`
//!   at the edge — each participant's codec converting to PCM16 on
//!   ingress and from PCM16 on egress.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![warn(missing_docs)]

pub mod agc;
pub mod conference;
pub mod fabric;
pub mod metrics;
pub mod mixer;
pub mod orchestrator;
pub mod participant;
pub mod registry;
pub mod vad;

pub use agc::{Agc, AgcConfig};
pub use conference::{
    Conference, ConferenceConfig, ConferenceError, ConferenceId, ConferenceStats, ParticipantFrame,
    ParticipantId, default_jitter_config,
};
pub use fabric::MixerFabric;
pub use metrics::{ConferenceLabel, IngressDropReason, MixerMetrics};
pub use mixer::{Mixer, MixerConfig};
pub use orchestrator::{DirectConferenceOrchestrator, MixerConferenceOrchestrator};
pub use participant::ConferenceParticipantSession;
pub use registry::{ConferenceRegistry, ConferenceRegistryError, InMemoryConferenceRegistry};
pub use smiths_media::jitter::{JitterBuffer, JitterConfig, JitterStats};
pub use vad::{EnergyVad, Vad, VadScore};
