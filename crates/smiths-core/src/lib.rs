//! Core runtime primitives for smiths-net.
//!
//! This crate hosts everything every other module depends on:
//! configuration, the typed event bus, graceful shutdown, and the shared
//! error type. It pulls in no sibling workspace crates — it is the root
//! of the dependency graph.

pub mod ai;
pub mod bus;
pub mod call;
pub mod codec;
pub mod config;
pub mod error;
pub mod event;
pub mod media;
pub mod metrics;
pub mod rtp;
pub mod sdp;
pub mod shutdown;

pub use ai::{
    AiProvider, AiRegistry, CapabilityDescriptor, ConcurrencyHint, LatencyHint, ProviderError,
    ValidationError, validate_controls,
};
pub use bus::EventBus;
pub use call::{CallError, CallLookup, CallOriginator, DialogKey, DialogRecord, DialogState};
pub use codec::{linear_to_ulaw, pcm16_to_pcmu, pcmu_to_pcm16, ulaw_to_linear};
pub use config::{
    A2aConfig, BindSpec, BindSpecError, Config, CoreConfig, LogFormat, McpConfig,
    ObservabilityConfig, PluginsConfig, RateLimitConfig, SipConfig, SipTransport,
};
pub use error::Error;
pub use event::{Event, PluginEvent, SipEvent, SystemEvent};
pub use media::{
    BridgeId, Endpoint, EndpointId, EndpointKind, MediaEndpoint, MediaError, MediaFabric,
    MediaSession,
};
pub use metrics::Metrics;
pub use rtp::RtpPacket;
pub use sdp::{NegotiationOutcome, SdpNegotiator};
pub use shutdown::Shutdown;
