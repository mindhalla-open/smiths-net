//! WASM-tier plugin provider.
//!
//! Wraps a compiled `wasmtime::Module` and the capability descriptors
//! the guest returned from its `describe` export. Implements
//! [`AiProvider`] so it registers alongside sidecar and script
//! providers in [`crate::AiRegistry`]. `invoke` trampolines through
//! the engine's `invoke` ABI on tokio's blocking pool, so a guest
//! that runs up to its wall-clock deadline never stalls an async
//! worker.

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
    /// Plugin directory the module was loaded from.
    pub dir: PathBuf,
    /// The compiled module. Cheap to clone thanks to `Module`'s
    /// internal `Arc`.
    pub module: Module,
    /// Engine handle used for every invocation. Shares persistent
    /// state, permissions, bus, media, and the originator slot with
    /// the engine the loader was given; the resource knobs (memory
    /// cap, deadline, fuel, state budget) are the loader's.
    pub engine: WasmEngine,
    /// Optional metrics handle; when present, each `invoke` records
    /// `plugin_invocations` + `plugin_invoke_duration_seconds`.
    pub metrics: Option<Arc<Metrics>>,
}

impl WasmProvider {
    /// Compile the `.wasm` at `manifest_dir / manifest.entry`, call
    /// its exported `describe` to populate capability descriptors,
    /// and return the fully-built provider. Synchronous — callers on
    /// an async runtime run it on the blocking pool.
    ///
    /// `describe` contract: zero-argument export returning an `i64`
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
        let descriptors = parse_descriptors(raw).map_err(|e| e.to_string())?;

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
        // The engine call is synchronous and bounded by the engine's
        // fuel + wall-clock deadline; park it on the blocking pool so
        // the runtime's workers stay free for that whole window.
        let name = self.manifest.name.clone();
        let start = std::time::Instant::now();
        let engine = self.engine.clone();
        let module = self.module.clone();
        let method = method.to_owned();
        let worker_name = name.clone();
        let result = tokio::task::spawn_blocking(move || {
            engine.call_invoke(&module, &worker_name, &method, &params)
        })
        .await
        .map_err(|e| ProviderError(format!("plugin `{name}`: invoke worker panicked: {e}")))
        .and_then(|r| r.map_err(|e| ProviderError(format!("plugin `{name}`: {e}"))));
        record_invocation(self.metrics.as_deref(), &name, &result, start);
        result
    }
}
