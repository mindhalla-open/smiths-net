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
use smiths_core::Metrics;
use smiths_core::ai::{
    AiProvider, AiRegistry as AiRegistryTrait, CapabilityDescriptor, ProviderError,
};
use smiths_core::metrics::{PluginLabel, PluginOutcomeLabel};
use smiths_sidecar::Sidecar;

use crate::manifest::Manifest;
use crate::script_provider::ScriptProvider;
use crate::wasm_provider::WasmProvider;

/// Hot-reload a script plugin: re-read the manifest, recompile the
/// source via `smiths_script`, and atomic-swap the new runtime into
/// the provider's hot slot — the previous version is retained so
/// the [`ScriptProvider`]'s error-count rollback supervisor can
/// restore it on the next N consecutive failures. The declared
/// capability set must match; a script whose new descriptor list
/// changes is treated as a manifest change and the swap is
/// refused.
async fn reload_script(provider: Arc<ScriptProvider>) -> Result<(), ProviderError> {
    let name = provider.name().to_owned();
    let dir = provider.dir().to_path_buf();
    let manifest = Manifest::from_dir(&dir)
        .map_err(|e| ProviderError(format!("reload `{name}`: manifest re-read failed: {e}")))?;
    if manifest.plugin_type != crate::manifest::PluginType::Script {
        return Err(ProviderError(format!(
            "reload `{name}`: manifest no longer declares `type = \"script\"`"
        )));
    }
    let entry_path = if manifest.entry.is_absolute() {
        manifest.entry.clone()
    } else {
        dir.join(&manifest.entry)
    };
    let new_runtime = match manifest.script_engine {
        crate::manifest::ScriptEngine::Rhai => {
            smiths_script::ScriptRuntime::load_rhai(&name, &entry_path, provider.limits())
                .map_err(|e| ProviderError(format!("reload `{name}`: compile: {e}")))?
        }
    };
    // Validate declared capabilities didn't shift.
    let raw = new_runtime
        .describe()
        .await
        .map_err(|e| ProviderError(format!("reload `{name}`: describe_capabilities: {e}")))?;
    let descs = smiths_core::ai::parse_descriptors(raw)
        .map_err(|e| ProviderError(format!("reload `{name}`: {e}")))?;
    let new_set: std::collections::BTreeSet<_> =
        descs.iter().map(|d| d.capability.clone()).collect();
    let old_set: std::collections::BTreeSet<_> = provider
        .capabilities()
        .iter()
        .map(|d| d.capability.clone())
        .collect();
    if new_set != old_set {
        return Err(ProviderError(format!(
            "reload `{name}`: capability set changed ({old_set:?} -> {new_set:?}); \
             restart the engine to pick up a new plugin contract"
        )));
    }
    provider.swap_runtime(new_runtime);
    Ok(())
}

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
    /// Optional metrics handle. When present, each `invoke` records
    /// `plugin_invocations` + `plugin_invoke_duration_seconds`.
    pub metrics: Option<Arc<Metrics>>,
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
        let name = &self.manifest.name;
        let start = std::time::Instant::now();
        let result = self
            .sidecar
            .call(method, params)
            .await
            .map_err(|e| ProviderError(format!("plugin `{name}`: {e}")));
        record_invocation(self.metrics.as_deref(), name, &result, start);
        result
    }
}

/// Record `plugin_invocations` + `plugin_invoke_duration_seconds`
/// for both the sidecar and WASM `AiProvider::invoke` paths.
pub(crate) fn record_invocation<T>(
    metrics: Option<&Metrics>,
    plugin: &str,
    result: &Result<T, ProviderError>,
    start: std::time::Instant,
) {
    let Some(m) = metrics else {
        return;
    };
    let outcome = if result.is_ok() { "ok" } else { "error" };
    m.plugin_invocations
        .get_or_create(&PluginOutcomeLabel {
            plugin: plugin.to_owned(),
            outcome: outcome.to_owned(),
        })
        .inc();
    m.plugin_invoke_duration
        .get_or_create(&PluginLabel {
            plugin: plugin.to_owned(),
        })
        .observe(start.elapsed().as_secs_f64());
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
    scripts: Arc<DashMap<String, Arc<ScriptProvider>>>,
}

impl std::fmt::Debug for AiRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiRegistry")
            .field("providers", &self.providers.len())
            .field("sidecars", &self.sidecars.len())
            .field("scripts", &self.scripts.len())
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

    /// Register a loaded script plugin (slice 4.1). Mirrored into
    /// a script-only index so hot-reload / error-rollback paths can
    /// recover the typed `ScriptProvider` without downcasting.
    pub fn insert_script(&self, provider: Arc<ScriptProvider>) {
        let name = provider.name().to_owned();
        self.providers
            .insert(name.clone(), Arc::clone(&provider) as Arc<dyn AiProvider>);
        self.scripts.insert(name, provider);
    }

    /// Get a script-backed provider by name (slice 4.1).
    #[must_use]
    pub fn get_script(&self, name: &str) -> Option<Arc<ScriptProvider>> {
        self.scripts.get(name).map(|e| Arc::clone(e.value()))
    }

    /// Snapshot every registered script plugin.
    #[must_use]
    pub fn script_snapshot(&self) -> Vec<Arc<ScriptProvider>> {
        self.scripts.iter().map(|e| Arc::clone(e.value())).collect()
    }

    /// Remove a plugin entry by name across every backend index
    /// (sidecar / script / unified `providers`). Returns `true` iff
    /// the entry existed. Used by the hot-reload rollback when a
    /// script script trips `ROLLBACK_AFTER` consecutive errors.
    #[must_use = "remove() returns whether the entry was present; ignoring it loses that signal"]
    pub fn remove(&self, name: &str) -> bool {
        let a = self.providers.remove(name).is_some();
        self.sidecars.remove(name);
        self.scripts.remove(name);
        a
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
        // WASM + script providers have no process to reap — dropping
        // the maps is enough.
        self.providers.clear();
        self.sidecars.clear();
        self.scripts.clear();
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
        // Script plugins hot-reload via in-place atomic swap so the
        // previous version stays available for auto-rollback. See
        // `script_provider::ROLLBACK_AFTER`.
        if let Some(provider) = self.get_script(name) {
            return reload_script(provider).await;
        }
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

    async fn reload_script_source(&self, name: &str, source: &str) -> Result<(), ProviderError> {
        let Some(provider) = self.get_script(name) else {
            return Err(ProviderError(format!(
                "plugin `{name}` not loaded or not script-backed"
            )));
        };
        let manifest = Manifest::from_dir(provider.dir()).map_err(|e| {
            ProviderError(format!(
                "reload_script_source `{name}`: manifest re-read failed: {e}"
            ))
        })?;
        let entry_path = if manifest.entry.is_absolute() {
            manifest.entry.clone()
        } else {
            provider.dir().join(&manifest.entry)
        };
        // Atomic write: tempfile in the same dir + rename so a
        // watcher observing the entry file never sees a half-written
        // source.
        let parent = entry_path.parent().ok_or_else(|| {
            ProviderError(format!(
                "entry `{}` has no parent dir",
                entry_path.display()
            ))
        })?;
        let tmp = parent.join(format!(".{name}.rhai.new"));
        std::fs::write(&tmp, source).map_err(|e| {
            ProviderError(format!("reload_script_source `{name}`: write temp: {e}"))
        })?;
        std::fs::rename(&tmp, &entry_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            ProviderError(format!(
                "reload_script_source `{name}`: rename into place: {e}"
            ))
        })?;
        reload_script(provider).await
    }
}
