//! Control plane: tools, resources, and transport adapters.
//!
//! The engine's control surface — listing calls, interrogating state,
//! and (later) driving plugins / placing calls — is expressed as a set
//! of typed [`Tool`]s and [`Resource`]s that are adapter-agnostic.
//! This crate ships two adapters today:
//!
//! * **MCP** (Model Context Protocol) over stdio — for LLM agents.
//! * **A2A** (simplified Agent-to-Agent) over HTTP — for
//!   agent-to-agent automation.
//!
//! Both adapters serve the exact same [`Tool`] implementations. That
//! is the crate's reason to exist: **one set of tools, many
//! protocols**. Adding a new adapter (gRPC, Matrix, webhook, ...) is
//! wrapping the transport around the same trait set.

// Slice 1.7 lint tightening.
#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod a2a;
pub mod audit;
pub mod control;
pub mod control_protocol;
pub mod dispatch;
pub mod mcp;
pub mod mcp_http;
pub mod rate_limit;
pub mod resource;
pub mod tool;
pub mod tools;
pub mod webhook;

pub use control::ControlState;
pub use control_protocol::{
    A2aHttpProtocol, ControlOutcome, ControlProtocol, McpStdioProtocol, ProtocolDispatch,
    WebhookHttpProtocol, agent_card,
};
pub use rate_limit::RateLimiter;
pub use resource::{
    Resource, ResourceContent, ResourceRegistry, builtin_registry as builtin_resources,
};
pub use tool::{Tool, ToolContext, ToolError, ToolRegistry};
