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
use crate::wasm_provider::WasmProvider;

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
///
/// Two backends today: sidecars (subprocess + JSON-RPC stdio) and
/// WASM providers (compiled module + describe-only MVP). Both impl
/// `AiProvider` and share a single `providers` map so `len`,
/// `capabilities`, `snapshot`, and `shutdown_all` iterate uniformly.
///
/// A parallel `sidecars` index keeps a strongly-typed handle on
/// sidecar-only entries — the streaming-notification bridge and
/// `reload` need it. WASM providers don't appear in that index.
#[derive(Clone, Default)]
pub struct AiRegistry {
    providers: Arc<DashMap<String, Arc<dyn AiProvider>>>,
    sidecars: Arc<DashMap<String, Arc<PluginEntry>>>,
}

impl std::fmt::Debug for AiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiRegistry")
            .field("providers", &self.providers.len())
            .field("sidecars", &self.sidecars.len())
            .finish()
    }
}

impl AiRegistry {
    /// Build an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a loaded sidecar plugin. Overwrites any existing
    /// entry with the same name. The entry is also mirrored into a
    /// sidecar-only index so streaming + reload paths can recover
    /// the typed `PluginEntry`.
    pub fn insert(&self, entry: PluginEntry) {
        let arc = Arc::new(entry);
        let name = arc.manifest.name.clone();
        self.providers
            .insert(name.clone(), Arc::clone(&arc) as Arc<dyn AiProvider>);
        self.sidecars.insert(name, arc);
    }

    /// Register a loaded WASM plugin.
    pub fn insert_wasm(&self, provider: Arc<WasmProvider>) {
        self.providers
            .insert(provider.name().to_owned(), provider as Arc<dyn AiProvider>);
    }

    /// Number of plugins registered (across all backends).
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// `true` if no plugins are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    /// Get the sidecar entry for one plugin, if registered (and if
    /// sidecar-backed). Used by consumers that need raw sidecar
    /// access — subscribing to notifications, driving hot reload.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<PluginEntry>> {
        self.sidecars.get(name).map(|e| Arc::clone(e.value()))
    }

    /// Snapshot every registered sidecar plugin. WASM providers are
    /// excluded — use the trait's `snapshot` for the union.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Arc<PluginEntry>> {
        self.sidecars
            .iter()
            .map(|e| Arc::clone(e.value()))
            .collect()
    }

    /// Flatten capability descriptors across every backend.
    #[must_use]
    pub fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.providers
            .iter()
            .flat_map(|e| e.value().capabilities().to_vec())
            .collect()
    }

    /// Shut down every plugin. Typically called on engine shutdown.
    pub async fn shutdown_all(&self) {
        let handles: Vec<Sidecar> = self
            .sidecars
            .iter()
            .map(|e| e.value().sidecar.clone())
            .collect();
        for sc in handles {
            sc.shutdown().await;
        }
        // WASM providers have no process to reap — dropping the map
        // is enough.
        self.providers.clear();
        self.sidecars.clear();
    }
}

#[async_trait]
impl AiRegistryTrait for AiRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn AiProvider>> {
        self.providers.get(name).map(|e| Arc::clone(e.value()))
    }
    fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
        self.providers
            .iter()
            .map(|e| Arc::clone(e.value()))
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
        self.providers.remove(name);
        self.sidecars.remove(name);
        crate::loader::load_one(&dir, self, crate::loader::LoaderOpts::default())
            .await
            .map(|_| ())
            .map_err(|e| ProviderError(format!("reload `{name}`: {e}")))
    }
}
