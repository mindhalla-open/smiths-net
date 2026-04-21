//! Adapter-agnostic resource abstraction + built-in resources.
//!
//! Resources are URIs that MCP/A2A clients read as typed content.
//! Like tools, the trait is adapter-independent; the same
//! [`ResourceRegistry`] is mounted by both the stdio MCP server and
//! the HTTP A2A server.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use crate::tool::{ToolContext, ToolError};

/// One content chunk returned by a resource read.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceContent {
    /// UTF-8 text body with an IANA media type.
    Text {
        /// e.g. `application/json`, `text/plain`.
        mime_type: String,
        /// Content body.
        text: String,
    },
}

impl ResourceContent {
    /// Build a `text/plain` content chunk.
    #[must_use]
    pub fn plain(body: impl Into<String>) -> Self {
        Self::Text {
            mime_type: "text/plain".into(),
            text: body.into(),
        }
    }

    /// Build an `application/json` content chunk by serializing `v`.
    pub fn json(v: &impl Serialize) -> Result<Self, ToolError> {
        serde_json::to_string(v)
            .map(|text| Self::Text {
                mime_type: "application/json".into(),
                text,
            })
            .map_err(|e| ToolError::Internal(format!("serialize resource: {e}")))
    }

    /// MIME type declared by this chunk.
    #[must_use]
    pub fn mime_type(&self) -> &str {
        match self {
            Self::Text { mime_type, .. } => mime_type,
        }
    }

    /// UTF-8 text body of this chunk.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Self::Text { text, .. } => text,
        }
    }
}

/// A named readable resource. Implementations are adapter-agnostic.
#[async_trait]
pub trait Resource: Send + Sync {
    /// Canonical URI (e.g. `health://status`).
    fn uri(&self) -> &'static str;

    /// One-line human description.
    fn description(&self) -> &'static str;

    /// Read the resource's current value.
    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError>;
}

/// URI-keyed collection of resources.
#[derive(Default)]
pub struct ResourceRegistry {
    // BTreeMap so `list` output is deterministic.
    resources: BTreeMap<String, Arc<dyn Resource>>,
}

impl ResourceRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a resource; duplicate URIs overwrite.
    pub fn register<R: Resource + 'static>(&mut self, resource: R) {
        self.resources
            .insert(resource.uri().to_owned(), Arc::new(resource));
    }

    /// Look up by URI.
    #[must_use]
    pub fn get(&self, uri: &str) -> Option<Arc<dyn Resource>> {
        self.resources.get(uri).cloned()
    }

    /// Iterate over every registered resource in URI order.
    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Resource>> + '_ {
        self.resources.values().cloned()
    }

    /// Number of registered resources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// `true` if no resources are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }
}

/// Build a registry populated with the shipped-in-box resource set.
#[must_use]
pub fn builtin_registry() -> ResourceRegistry {
    let mut reg = ResourceRegistry::new();
    reg.register(HealthResource);
    reg.register(CallsResource);
    reg.register(CurrentConfigResource);
    reg.register(RegistrationsResource);
    reg
}

// ---------------------------------------------------------------------
// Built-in resources
// ---------------------------------------------------------------------

/// `health://status` — uptime, known calls, live calls. Mirrors the
/// `health` tool but reachable through the resource URI space.
pub struct HealthResource;

#[async_trait]
impl Resource for HealthResource {
    fn uri(&self) -> &'static str {
        "health://status"
    }
    fn description(&self) -> &'static str {
        "Engine health: uptime, known calls, live calls."
    }
    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let calls = ctx.state.list_calls();
        let live = calls
            .iter()
            .filter(|c| matches!(c.phase, crate::control::CallPhase::Live))
            .count();
        ResourceContent::json(&json!({
            "status": "ok",
            "uptime_secs": ctx.state.uptime_secs(),
            "live_calls": live,
            "known_calls": calls.len(),
        }))
    }
}

/// `sip://calls` — full snapshot of every dialog the engine tracks.
pub struct CallsResource;

#[async_trait]
impl Resource for CallsResource {
    fn uri(&self) -> &'static str {
        "sip://calls"
    }
    fn description(&self) -> &'static str {
        "Active and recently-terminated SIP dialogs."
    }
    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let calls = ctx.state.list_calls();
        ResourceContent::json(&json!({
            "count": calls.len(),
            "calls": calls,
        }))
    }
}

/// `sip://registrations` — live subscriber-DB bindings (slice 2.1).
///
/// Returns an empty snapshot when no [`smiths_core::RegistrationView`]
/// is wired into the context. Operators can distinguish
/// "registrar disabled" from "idle registrar" by cross-referencing
/// `[auth] backend` on `config://current`.
pub struct RegistrationsResource;

#[async_trait]
impl Resource for RegistrationsResource {
    fn uri(&self) -> &'static str {
        "sip://registrations"
    }
    fn description(&self) -> &'static str {
        "Live SIP subscriber-DB registrations (AOR → contact bindings)."
    }
    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let (count, bindings) = match ctx.registrations.as_ref() {
            Some(view) => {
                let bs = view.snapshot();
                (bs.len(), bs)
            }
            None => (0, Vec::new()),
        };
        ResourceContent::json(&json!({
            "count": count,
            "bindings": bindings,
        }))
    }
}

/// `config://current` — the engine's merged configuration, with any
/// secret fields replaced by `"***"`.
pub struct CurrentConfigResource;

#[async_trait]
impl Resource for CurrentConfigResource {
    fn uri(&self) -> &'static str {
        "config://current"
    }
    fn description(&self) -> &'static str {
        "Current engine configuration (secrets redacted)."
    }
    async fn read(&self, ctx: &ToolContext) -> Result<ResourceContent, ToolError> {
        let mut v = serde_json::to_value(ctx.config.as_ref())
            .map_err(|e| ToolError::Internal(format!("serialize config: {e}")))?;
        redact_secrets(&mut v);
        ResourceContent::json(&v)
    }
}

/// Walk the config JSON and replace known secret fields with `"***"`.
/// Today: `a2a.bearer_token`. Extend as new secrets land.
fn redact_secrets(v: &mut Value) {
    const SECRET_PATHS: &[&[&str]] = &[&["a2a", "bearer_token"]];
    for path in SECRET_PATHS {
        if let Some(leaf) = walk_mut(v, path)
            && !leaf.is_null()
        {
            *leaf = Value::String("***".into());
        }
    }
}

fn walk_mut<'a>(root: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = cur.as_object_mut()?.get_mut(*seg)?;
    }
    Some(cur)
}
