//! Plugin discovery, manifest parsing, and capability registry.
//!
//! Three plugin tiers load through one loader and register in one
//! [`AiRegistry`]:
//!
//! - `type = "sidecar"` — a subprocess speaking JSON-RPC over stdio,
//!   supervised by [`sidecar::Sidecar`];
//! - `type = "wasm"` — a guest module run by the in-process
//!   [`wasm::WasmEngine`];
//! - `type = "script"` — an embedded Rhai script run by
//!   [`script::ScriptRuntime`].
//!
//! A plugin is a directory containing `plugin.toml` plus its entry
//! (executable, `.wasm`, or script). The loader scans the configured
//! root — descending through container directories such as
//! `plugins/examples` and `plugins/cookbook/...` — runs each plugin's
//! `describe_capabilities` handshake, validates the returned
//! descriptors against the plugin protocol, registers the provider,
//! and wires the lifecycle hooks its manifest declares into the
//! registry's priority-ordered dispatcher. Plugin metadata and the
//! control-schema validator live in `smiths-core::ai`; this crate
//! implements the traits and owns the host-tier sub-crates.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod dispatcher;
pub mod error;
pub mod event_hook;
pub mod loader;
pub mod manifest;
pub mod registry;
pub mod script_provider;
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
pub use event_hook::{HOOK_DIALOG_CREATED, HOOK_DIALOG_TERMINATED, spawn_call_event_hooks};
pub use loader::{LoadReport, LoaderOpts, MAX_SCAN_DEPTH, load_plugins};
pub use manifest::{
    DEFAULT_PRIORITY, MAX_PRIORITY, Manifest, PluginType, SUPPORTED_HOOKS, ScriptEngine,
};
pub use registry::{AiRegistry, DEFAULT_HOOK_BUDGET, PluginEntry, ProviderHook};
pub use script_provider::{ROLLBACK_AFTER, ScriptProvider};
pub use wasm_provider::WasmProvider;
pub use watcher::{WatcherHandle, spawn as spawn_watcher};
