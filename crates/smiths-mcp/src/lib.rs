//! Control plane: tools, resources, and transport adapters.
//!
//! The engine's control surface — listing calls, interrogating state,
//! and (later) driving plugins / placing calls — is expressed as a set
//! of typed [`Tool`]s and [`Resource`]s that are adapter-agnostic.
//! This crate ships four adapters:
//!
//! * **MCP** over stdio — for LLM agents spawning the engine.
//! * **MCP** over Streamable HTTP (+ SSE) — for long-running daemons.
//! * **A2A** (simplified Agent-to-Agent) over HTTP — for
//!   agent-to-agent automation.
//! * **Webhook** over HTTP — bare JSON for low-code integrations.
//!
//! Every adapter serves the exact same [`Tool`] implementations
//! through one [`ProtocolDispatch`] pipeline (rate limit, schema
//! validation, audit, metrics). That is the crate's reason to exist:
//! **one set of tools, many protocols**. Adding a new adapter (gRPC,
//! Matrix, AMQP,...) is wrapping the transport around the same
//! dispatcher.

#![warn(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod a2a;
pub mod audit;
pub mod auth;
pub mod cluster;
pub mod config_history;
pub mod control;
pub mod control_protocol;
pub mod dispatch;
pub mod jsonrpc;
pub mod mcp;
pub mod mcp_http;
pub mod rate_limit;
pub mod resource;
pub mod schema;
pub mod tool;
pub mod tools;
pub mod webhook;

pub use cluster::ClusterStatusSource;
pub use control::ControlState;
pub use control_protocol::{
    A2aHttpProtocol, ControlOutcome, ControlProtocol, McpHttpProtocol, McpStdioProtocol,
    ProtocolDispatch, WebhookHttpProtocol, agent_card,
};
pub use mcp_http::McpHttpServer;
pub use rate_limit::RateLimiter;
pub use resource::{
    Resource, ResourceContent, ResourceRegistry, builtin_registry as builtin_resources,
};
pub use tool::{Tool, ToolContext, ToolError, ToolRegistry};
