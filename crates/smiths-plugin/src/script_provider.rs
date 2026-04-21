//! `AiProvider` shim around a [`smiths_script::ScriptRuntime`].
//!
//! Script plugins (slice 4.1) compile once at load and serve every
//! invocation through the hot engine. The provider supports hot-
//! reload by atomic swap: the watcher compiles a new runtime, calls
//! [`ScriptProvider::swap_runtime`], and the very next invocation
//! runs against it. If the new version errors
//! [`ROLLBACK_AFTER`] times in a row and a previous runtime is
//! retained, the provider swaps back automatically and logs the
//! rollback.
//!
//! Capabilities are immutable across reloads — a script swap that
//! changes its declared capabilities is a manifest change, not a
//! hot reload. The watcher enforces that by re-running
//! `describe_capabilities` and refusing the swap if the descriptor
//! set shifts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use smiths_core::Metrics;
use smiths_core::ai::{AiProvider, CapabilityDescriptor, ProviderError};
use smiths_script::{ScriptLimits, ScriptRuntime};

/// Number of consecutive errors that trip the rollback supervisor
/// and force the script's previous version back into the hot path.
/// Matches the slice-4.1 spec ("rollback on 5-error-in-a-row").
pub const ROLLBACK_AFTER: u32 = 5;

/// Script-backed `AiProvider`. Cheap to clone.
pub struct ScriptProvider {
    name: String,
    version: String,
    description: String,
    abi: String,
    capabilities: Vec<CapabilityDescriptor>,
    /// Current hot runtime. Swapped atomically by
    /// [`Self::swap_runtime`]; invocations take a cheap `Arc` clone
    /// so they don't hold the lock across await points.
    runtime: Mutex<Arc<ScriptRuntime>>,
    /// Most recent previous runtime, kept for auto-rollback.
    /// Replaced on each successful swap; consumed on rollback.
    previous: Mutex<Option<Arc<ScriptRuntime>>>,
    /// Plugin directory (parent of the `.rhai` entry).
    dir: PathBuf,
    /// Consecutive-error counter; zeroed on every success.
    consecutive_errors: AtomicU32,
    metrics: Option<Arc<Metrics>>,
    limits: ScriptLimits,
}

impl ScriptProvider {
    /// Wrap a freshly-compiled runtime. `capabilities` usually comes
    /// from `runtime.describe()` at load time, validated through
    /// [`smiths_core::ai::parse_descriptors`] before calling.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        name: String,
        version: String,
        description: String,
        abi: String,
        capabilities: Vec<CapabilityDescriptor>,
        runtime: ScriptRuntime,
        dir: PathBuf,
        limits: ScriptLimits,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        Self {
            name,
            version,
            description,
            abi,
            capabilities,
            runtime: Mutex::new(Arc::new(runtime)),
            previous: Mutex::new(None),
            dir,
            consecutive_errors: AtomicU32::new(0),
            metrics,
            limits,
        }
    }

    /// Plugin directory. Used by the watcher to locate the
    /// `main.rhai` source on reload.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Compile-time limits.
    #[must_use]
    pub fn limits(&self) -> ScriptLimits {
        self.limits
    }

    /// Current consecutive-error tally.
    #[must_use]
    pub fn consecutive_errors(&self) -> u32 {
        self.consecutive_errors.load(Ordering::Acquire)
    }

    /// Install a freshly-compiled runtime in the hot slot and retain
    /// the prior one for possible rollback. Also resets the
    /// consecutive-error counter so the new version gets a clean
    /// probationary window.
    pub fn swap_runtime(&self, new_runtime: ScriptRuntime) {
        let new_arc = Arc::new(new_runtime);
        let mut guard = self.runtime_lock();
        let old = std::mem::replace(&mut *guard, new_arc);
        *self.previous_lock() = Some(old);
        self.consecutive_errors.store(0, Ordering::Release);
    }

    /// Swap the previous runtime back into the hot slot if one was
    /// retained. Returns `true` when a rollback actually happened.
    pub fn rollback(&self) -> bool {
        let Some(prev) = self.previous_lock().take() else {
            return false;
        };
        *self.runtime_lock() = prev;
        self.consecutive_errors.store(0, Ordering::Release);
        true
    }

    fn runtime_lock(&self) -> std::sync::MutexGuard<'_, Arc<ScriptRuntime>> {
        self.runtime
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn previous_lock(&self) -> std::sync::MutexGuard<'_, Option<Arc<ScriptRuntime>>> {
        self.previous
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl AiProvider for ScriptProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        &self.version
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn abi(&self) -> &str {
        &self.abi
    }
    fn capabilities(&self) -> &[CapabilityDescriptor] {
        &self.capabilities
    }
    async fn invoke(&self, method: &str, params: Value) -> Result<Value, ProviderError> {
        let started = std::time::Instant::now();
        let runtime = Arc::clone(&*self.runtime_lock());
        let result = runtime
            .call(method, params)
            .await
            .map_err(|e| ProviderError(format!("script `{}`: {e}", self.name)));
        if result.is_ok() {
            self.consecutive_errors.store(0, Ordering::Release);
        } else {
            let n = self.consecutive_errors.fetch_add(1, Ordering::AcqRel) + 1;
            if n >= ROLLBACK_AFTER && self.rollback() {
                tracing::warn!(
                    plugin = %self.name, errors = n,
                    "script tripped ROLLBACK_AFTER consecutive errors; previous version restored"
                );
            }
        }
        crate::registry::record_invocation(self.metrics.as_deref(), &self.name, &result, started);
        result
    }
}
