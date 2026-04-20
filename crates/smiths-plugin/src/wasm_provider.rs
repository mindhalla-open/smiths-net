//! WASM-tier plugin provider.
//!
//! Wraps a compiled `wasmtime::Module` and the capability descriptors
//! the guest returned from its `describe()` export. Implements
//! [`AiProvider`] so it registers alongside sidecar providers in
//! [`crate::AiRegistry`].
//!
//! Invocation is deliberately **not yet** wired — the MVP tier just
//! surfaces WASM plugins in `list_ai_providers` / `describe_provider`.
//! Full method dispatch lands alongside the richer host surface
//! (`send_sip` / `send_rtp` / permission checks).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use smiths_core::Metrics;
use smiths_core::ai::{AiProvider, CapabilityDescriptor, ProviderError, parse_descriptors};
use smiths_wasm::{Module, WasmEngine};

use crate::manifest::Manifest;
use crate::registry::record_invocation;

/// A WASM plugin registered with the engine's `AiRegistry`.
#[derive(Clone, Debug)]
pub struct WasmProvider {
    manifest: Manifest,
    capabilities: Vec<CapabilityDescriptor>,
    /// Plugin directory — captured for parity with `PluginEntry` so
    /// `AiRegistry::reload` can respawn from the same location once
    /// WASM hot-reload lands.
    pub dir: PathBuf,
    /// The compiled module. Cheap to clone thanks to `Module`'s
    /// internal `Arc`.
    pub module: Module,
    /// Shared engine handle (same `WasmEngine` every WASM plugin
    /// uses, so state + fuel + epoch config are uniform).
    pub engine: WasmEngine,
    /// Optional metrics handle; when present, each `invoke` records
    /// `plugin_invocations` + `plugin_invoke_duration_seconds`.
    pub metrics: Option<Arc<Metrics>>,
}

impl WasmProvider {
    /// Compile the `.wasm` at `manifest_dir / manifest.entry`, call
    /// its exported `describe()` to populate capability descriptors,
    /// and return the fully-built provider.
    ///
    /// `describe()` contract: zero-argument export returning an `i64`
    /// whose high 32 bits are an offset into the guest's exported
    /// `memory` and whose low 32 bits are a length. The bytes at that
    /// range must be a UTF-8 JSON document matching
    /// `CapabilityDescriptor` (or an array of them).
    pub fn load(engine: WasmEngine, manifest: Manifest, dir: PathBuf) -> Result<Self, String> {
        let wasm_path = if manifest.entry.is_absolute() {
            manifest.entry.clone()
        } else {
            dir.join(&manifest.entry)
        };
        let bytes =
            std::fs::read(&wasm_path).map_err(|e| format!("read {}: {e}", wasm_path.display()))?;
        let module = engine
            .load(&bytes)
            .map_err(|e| format!("compile {}: {e}", wasm_path.display()))?;

        // Register the manifest's permission set with the engine
        // *before* describe runs, so describe itself is gated by the
        // same rules as any other guest entry point.
        engine.set_plugin_permissions(&manifest.name, manifest.permissions.iter().cloned());

        let descriptor_bytes = engine
            .call_describe(&module, &manifest.name)
            .map_err(|e| format!("describe: {e}"))?;
        let raw: Value = serde_json::from_slice(&descriptor_bytes)
            .map_err(|e| format!("describe JSON parse: {e}"))?;
        let descriptors = parse_descriptors(raw)?;

        // Declared `provides` must be covered by the descriptors —
        // same sanity check the sidecar loader does.
        for declared in &manifest.provides {
            if !descriptors.iter().any(|d| &d.capability == declared) {
                return Err(format!(
                    "manifest claims `{declared}` but plugin didn't describe it"
                ));
            }
        }

        // Clamp the descriptor's plugin field to the manifest name so
        // downstream code can trust the binding.
        let capabilities: Vec<CapabilityDescriptor> = descriptors
            .into_iter()
            .map(|mut d| {
                d.plugin.clone_from(&manifest.name);
                d
            })
            .collect();

        Ok(Self {
            manifest,
            capabilities,
            dir,
            module,
            engine,
            metrics: None,
        })
    }

    /// Attach a metrics handle. Builder-style so the loader can
    /// install it after construction without a larger signature.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

#[async_trait]
impl AiProvider for WasmProvider {
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
        // Compilation ran at load; here we just trampoline through the
        // engine's `invoke` ABI. Wasmtime's sync call runs on this
        // thread — acceptable at MVP scale, but the next slice should
        // park it on a blocking pool if plugins get compute-heavy.
        let name = &self.manifest.name;
        let start = std::time::Instant::now();
        let result = self
            .engine
            .call_invoke(&self.module, name, method, &params)
            .map_err(|e| ProviderError(format!("plugin `{name}`: {e}")));
        record_invocation(self.metrics.as_deref(), name, &result, start);
        result
    }
}
