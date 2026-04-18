//! Registry of loaded plugins and their capability descriptors.
//!
//! Cheaply cloneable; the `Arc<DashMap>` lets MCP tools, the CLI, and
//! future invocation paths read the set concurrently.

use std::sync::Arc;

use dashmap::DashMap;
use smiths_sidecar::Sidecar;

use crate::descriptor::CapabilityDescriptor;
use crate::manifest::Manifest;

/// One registered plugin — its manifest, live sidecar handle, and the
/// capabilities it advertised at load.
#[derive(Clone, Debug)]
pub struct PluginEntry {
    /// Parsed manifest.
    pub manifest: Manifest,
    /// Live subprocess handle.
    pub sidecar: Sidecar,
    /// Descriptors returned by `describe_capabilities` at load.
    pub capabilities: Vec<CapabilityDescriptor>,
}

/// Plugin registry. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct AiRegistry {
    plugins: Arc<DashMap<String, PluginEntry>>,
}

impl AiRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a loaded plugin. Overwrites any existing entry with
    /// the same name.
    pub fn insert(&self, entry: PluginEntry) {
        self.plugins.insert(entry.manifest.name.clone(), entry);
    }

    /// Number of plugins registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// `true` if no plugins are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Get the entry for one plugin, if registered.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<PluginEntry> {
        self.plugins.get(name).map(|e| e.value().clone())
    }

    /// Snapshot every registered plugin's entry.
    #[must_use]
    pub fn snapshot(&self) -> Vec<PluginEntry> {
        self.plugins.iter().map(|e| e.value().clone()).collect()
    }

    /// Flatten capability descriptors across every plugin.
    #[must_use]
    pub fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.snapshot()
            .into_iter()
            .flat_map(|p| p.capabilities)
            .collect()
    }

    /// Shut down every plugin. Typically called on engine shutdown.
    pub async fn shutdown_all(&self) {
        let handles: Vec<Sidecar> = self
            .plugins
            .iter()
            .map(|e| e.value().sidecar.clone())
            .collect();
        for sc in handles {
            sc.shutdown().await;
        }
    }
}
