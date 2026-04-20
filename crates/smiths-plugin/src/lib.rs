//! Plugin discovery, manifest parsing, and capability registry.
//!
//! Today's scope — **sidecar plugins only** (`type = "sidecar"`). WASM
//! and embedded-script tiers land later and live in the re-exported
//! sub-namespaces [`wasm`] / [`script`] / [`sidecar`]. A plugin is:
//!
//! 1. A directory under `plugins.dir` containing `plugin.toml`.
//! 2. An executable entry point (relative to that directory).
//!
//! At boot the loader spawns each plugin via [`sidecar::Sidecar`],
//! calls `describe_capabilities`, validates the returned descriptors
//! against `05-ai-plugin-protocol.md`, and registers them in the
//! [`AiRegistry`]. Plugin metadata and the control-schema validator
//! live in `smiths-core::ai`; this crate just implements the traits
//! and owns the host-tier sub-crates.

// Slice 1.7 lint tightening.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod dispatcher;
pub mod error;
pub mod loader;
pub mod manifest;
pub mod registry;
pub mod wasm_provider;
pub mod watcher;

// Host tiers owned by the plugin umbrella — this is the one documented
// cross-sibling exception in the dependency graph.
pub use smiths_script as script;
pub use smiths_sidecar as sidecar;
pub use smiths_wasm as wasm;

// Re-export the trait seams and common data types from core so crate
// consumers don't need a second import path.
pub use smiths_core::ai::{
    AiProvider, AiRegistry as AiRegistryTrait, CapabilityDescriptor, ConcurrencyHint, LatencyHint,
    ProviderError, ValidationError, validate_controls,
};

pub use dispatcher::{Dispatcher, Hook, HookReport, MemoryDispatcher};
pub use error::Error;
pub use loader::{LoadReport, LoaderOpts, load_plugins};
pub use manifest::{Manifest, PluginType};
pub use registry::{AiRegistry, PluginEntry};
pub use wasm_provider::WasmProvider;
pub use watcher::{WatcherHandle, spawn as spawn_watcher};
