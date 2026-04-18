//! Adapter-agnostic resource abstraction.
//!
//! Resources are URIs that MCP/A2A clients read as typed content. The
//! trait mirrors [`crate::tool::Tool`] in shape — adapter-independent,
//! async, single content type on the wire.
//!
//! This turn ships the trait and type skeleton only; concrete resource
//! handlers (`sip://calls/*`, `config://current`, `metrics://snapshot`,
//! `health://status`) land incrementally as their backing data becomes
//! accessible through the event bus and fabric.

use async_trait::async_trait;
use serde::Serialize;

use crate::tool::ToolError;

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

/// A named readable resource. Implementations are adapter-agnostic.
#[async_trait]
pub trait Resource: Send + Sync {
    /// Canonical URI (e.g. `health://status`).
    fn uri(&self) -> &'static str;

    /// One-line human description.
    fn description(&self) -> &'static str;

    /// Read the resource's current value.
    async fn read(&self, ctx: &crate::tool::ToolContext) -> Result<ResourceContent, ToolError>;
}
