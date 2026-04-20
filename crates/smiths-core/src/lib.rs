//! Core runtime primitives for smiths-net.
//!
//! This crate hosts everything every other module depends on:
//! configuration, the typed event bus, graceful shutdown, and the shared
//! error type. It pulls in no sibling workspace crates — it is the root
//! of the dependency graph.

// Per-package tightening: smiths-core has zero production-code
// `.unwrap()` / `.expect()` (every use lives in `#[cfg(test)]` mods).
// Promoting the lint here guards future drift at no current cost.
// Unit tests inside `src/**/*.rs` are allowed to `.unwrap()` freely —
// that's the idiomatic test style and the only purpose of `cfg_attr`
// below. The lint lives in `lib.rs` rather than `Cargo.toml` because
// Cargo 1.74+ doesn't permit mixing `[lints] workspace = true` with
// a `[lints.clippy]` override.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Slice 1.7: missing_docs promoted to warn on smiths-core so every
// public item carries at least a one-line description. Zero-fire
// today; a CI gate in slice 1.8 blocks regressions.
#![warn(missing_docs)]

pub mod ai;
pub mod bus;
pub mod call;
pub mod codec;
pub mod config;
pub mod drain;
pub mod dtls;
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
pub use call::{
    CallError, CallLookup, CallOriginator, DialogKey, DialogRecord, DialogState,
    RegistrationSnapshot, RegistrationView,
};
pub use codec::{linear_to_ulaw, pcm16_to_pcmu, pcmu_to_pcm16, ulaw_to_linear};
pub use config::{
    A2aConfig, AuthBackend, AuthConfig, BindSpec, BindSpecError, Config, CoreConfig, LogFormat,
    McpConfig, ObservabilityConfig, PluginsConfig, RateLimitConfig, SandboxConfig, SeccompPolicy,
    SipConfig, SipRateLimit, SipTransport, SqliteAuthConfig,
};
pub use drain::Drain;
pub use dtls::{DtlsCertError, SelfSignedCert};
pub use error::Error;
pub use event::{Event, MediaSecurityFailure, PluginEvent, SipEvent, SystemEvent};
pub use media::{
    BridgeId, BridgeLeg, Endpoint, EndpointId, EndpointKind, MediaEndpoint, MediaError,
    MediaFabric, MediaSession, SrtpError, SrtpSuite, SrtpTransform,
};
pub use metrics::Metrics;
pub use rtp::RtpPacket;
pub use sdp::{NegotiationOutcome, SdpNegotiator, SrtpKeys};
pub use shutdown::Shutdown;
