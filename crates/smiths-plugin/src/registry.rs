//! Registry of loaded plugins and their capability descriptors.
//!
//! Cheaply cloneable; the `Arc<DashMap>` lets MCP tools, the CLI, and
//! future invocation paths read the set concurrently.
//!
//! Implements the [`smiths_core::ai::AiRegistry`] trait so the MCP
//! control plane consumes it through the `smiths-core` seam and never
//! links this crate.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use smiths_core::ai::{
    AiProvider, AiRegistry as AiRegistryTrait, CapabilityDescriptor, ProviderError,
};
use smiths_sidecar::Sidecar;

use crate::manifest::Manifest;

/// One registered plugin — its manifest, live sidecar handle, and the
/// capabilities it advertised at load.
#[derive(Debug)]
pub struct PluginEntry {
    /// Parsed manifest.
    pub manifest: Manifest,
    /// Plugin directory (where `plugin.toml` and the entry executable
    /// live). Captured at load so `reload` can respawn from the same
    /// source without the caller reconstructing the path.
    pub dir: PathBuf,
    /// Live subprocess handle.
    pub sidecar: Sidecar,
    /// Descriptors returned by `describe_capabilities` at load.
    pub capabilities: Vec<CapabilityDescriptor>,
}

#[async_trait]
impl AiProvider for PluginEntry {
    fn name(&self) -> &str {
        &self.manifest.name
    }
    fn version(&self) -> &str {
        &self.manifest.version
    }
    fn description(&self) -> &str {
        &self.manifest.description
    }
    fn abi(&self) -> &str {
        &self.manifest.abi
    }
    fn capabilities(&self) -> &[CapabilityDescriptor] {
        &self.capabilities
    }
    async fn invoke(&self, method: &str, params: Value) -> Result<Value, ProviderError> {
        self.sidecar
            .call(method, params)
            .await
            .map_err(|e| ProviderError(format!("plugin `{}`: {e}", self.manifest.name)))
    }
}

/// Plugin registry. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct AiRegistry {
    plugins: Arc<DashMap<String, Arc<PluginEntry>>>,
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
        self.plugins
            .insert(entry.manifest.name.clone(), Arc::new(entry));
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
    pub fn get(&self, name: &str) -> Option<Arc<PluginEntry>> {
        self.plugins.get(name).map(|e| Arc::clone(e.value()))
    }

    /// Snapshot every registered plugin's entry.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Arc<PluginEntry>> {
        self.plugins.iter().map(|e| Arc::clone(e.value())).collect()
    }

    /// Flatten capability descriptors across every plugin.
    #[must_use]
    pub fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.snapshot()
            .into_iter()
            .flat_map(|p| p.capabilities.clone())
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

#[async_trait]
impl AiRegistryTrait for AiRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn AiProvider>> {
        self.get(name).map(|e| e as Arc<dyn AiProvider>)
    }
    fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
        self.snapshot()
            .into_iter()
            .map(|e| e as Arc<dyn AiProvider>)
            .collect()
    }
    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        AiRegistry::capabilities(self)
    }
    fn len(&self) -> usize {
        AiRegistry::len(self)
    }
    fn is_empty(&self) -> bool {
        AiRegistry::is_empty(self)
    }
    async fn shutdown_all(&self) {
        AiRegistry::shutdown_all(self).await;
    }
    async fn reload(&self, name: &str) -> Result<(), ProviderError> {
        let Some(existing) = self.get(name) else {
            return Err(ProviderError(format!("no loaded plugin named `{name}`")));
        };
        let dir = existing.dir.clone();
        // Drop the old sidecar first so the OS releases stdio fds
        // before we spawn its replacement.
        existing.sidecar.shutdown().await;
        self.plugins.remove(name);
        crate::loader::load_one(&dir, self)
            .await
            .map(|_| ())
            .map_err(|e| ProviderError(format!("reload `{name}`: {e}")))
    }
}
