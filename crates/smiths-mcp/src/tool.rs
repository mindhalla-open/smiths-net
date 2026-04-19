//! Adapter-agnostic tool abstraction.
//!
//! A [`Tool`] is one named operation the engine exposes on its control
//! plane. Both the MCP and A2A adapters register the same
//! [`ToolRegistry`] and invoke tools through it. The wire protocol of
//! the adapter has no say in what the tool does or what it returns;
//! that keeps the tool set small and testable independent of any
//! transport.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use smiths_core::Config;
use smiths_core::ai::AiRegistry;
use thiserror::Error;

use crate::control::ControlState;

/// Context passed to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    /// Live engine state, updated by the event-bus subscription.
    pub state: ControlState,
    /// Loaded AI plugins behind the core trait seam. Empty registry
    /// when no plugins dir is configured or when every plugin failed
    /// to load — tools that depend on it should return a clean error
    /// in that case.
    pub plugins: Arc<dyn AiRegistry>,
    /// Engine configuration snapshot. Resources like `config://current`
    /// read from this; tools that need a config knob copy it into their
    /// arg schema rather than reaching through here.
    pub config: Arc<Config>,
}

impl ToolContext {
    /// Build a context from its components.
    #[must_use]
    pub fn new(state: ControlState, plugins: Arc<dyn AiRegistry>, config: Arc<Config>) -> Self {
        Self {
            state,
            plugins,
            config,
        }
    }
}

/// Errors a tool can return to the adapter layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// Input JSON didn't conform to the tool's declared schema.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The caller referenced something that doesn't exist (e.g. unknown call id).
    #[error("not found: {0}")]
    NotFound(String),
    /// Caller doesn't have permission. Placeholder — hooked up once
    /// auth lands.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Unexpected internal failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// One typed control-plane operation.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique, stable identifier used on the wire (e.g. `list_calls`).
    fn name(&self) -> &'static str;

    /// One-sentence human description exposed to MCP clients.
    fn description(&self) -> &'static str;

    /// JSON Schema (draft 2020-12) describing the expected `arguments`
    /// object. Empty schema = no arguments.
    fn input_schema(&self) -> Value;

    /// Execute. Implementations should validate `args` against their
    /// schema; the registry does not do that for them.
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError>;
}

/// Ordered collection of tools, keyed by name.
#[derive(Default)]
pub struct ToolRegistry {
    // BTreeMap so `list` output is deterministic.
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; duplicate names overwrite the previous entry.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Iterate over every registered tool in name order.
    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Tool>> + '_ {
        self.tools.values().cloned()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// `true` if no tools are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Tiny [`AiRegistry`] double used by MCP tests without dragging in
    //! the real plugin host.

    use std::sync::Arc;

    use async_trait::async_trait;
    use smiths_core::Config;
    use smiths_core::ai::{AiProvider, AiRegistry};

    /// Registry with no providers. Every lookup returns `None`.
    pub(crate) struct EmptyRegistry;

    #[async_trait]
    impl AiRegistry for EmptyRegistry {
        fn get(&self, _name: &str) -> Option<Arc<dyn AiProvider>> {
            None
        }
        fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
            Vec::new()
        }
        async fn shutdown_all(&self) {}
    }

    /// Convenience: build an `Arc<dyn AiRegistry>` holding an [`EmptyRegistry`].
    pub(crate) fn empty_registry() -> Arc<dyn AiRegistry> {
        Arc::new(EmptyRegistry)
    }

    /// Default-config [`Arc<Config>`] for test contexts.
    pub(crate) fn default_config() -> Arc<Config> {
        Arc::new(Config::default())
    }
}
