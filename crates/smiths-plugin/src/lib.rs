//! Plugin discovery, manifest parsing, and capability registry.
//!
//! Today's scope — **sidecar plugins only** (`type = "sidecar"`). WASM
//! and embedded-script tiers land later. A plugin is:
//!
//! 1. A directory under `plugins.dir` containing `plugin.toml`.
//! 2. An executable entry point (relative to that directory).
//!
//! At boot the loader spawns each plugin via [`smiths_sidecar::Sidecar`],
//! calls `describe_capabilities`, validates the returned descriptors
//! against `05-ai-plugin-protocol.md`, and registers them in the
//! [`AiRegistry`]. The MCP layer reads that registry to serve
//! `list_ai_providers` / `describe_provider` / `synthesize` tools and
//! uses [`validate_controls`] to strict-reject unknown parameters.

pub mod controls;
pub mod descriptor;
pub mod error;
pub mod loader;
pub mod manifest;
pub mod registry;

pub use controls::{ValidationError, validate_controls};
pub use descriptor::CapabilityDescriptor;
pub use error::Error;
pub use loader::{LoadReport, load_plugins};
pub use manifest::{Manifest, PluginType};
pub use registry::{AiRegistry, PluginEntry};
