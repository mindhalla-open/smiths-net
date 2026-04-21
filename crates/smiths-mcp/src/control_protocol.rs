//! `ControlProtocol` trait — the adapter-agnostic shape that MCP,
//! A2A, the generic webhook, and any future adapter (gRPC, Matrix,
//! AMQP) share.
//!
//! Slice 4.4 / P20 pulls this out of the ad-hoc dispatch loops the
//! MCP and A2A adapters used to hand-roll. Neither adapter changes
//! its wire shape; they both now route through one
//! [`ProtocolDispatch`] helper so operator-authored adapters can
//! reuse the same authorization + audit + metrics pipeline.
//!
//! ## What's "shared"
//!
//! Every adapter in this crate funnels through
//! [`crate::dispatch::invoke_audited`]. That's the integration
//! point. What varies is framing:
//!
//! * **MCP stdio** — newline-delimited JSON-RPC 2.0 per stdin line.
//! * **MCP HTTP** — POST `{jsonrpc, method, id, params}` to `/mcp`.
//! * **A2A HTTP** — POST `{jsonrpc, method, id, params}` to `/a2a`.
//! * **Webhook HTTP** — POST `{args}` to `/hook/<tool>`.
//!
//! The `ControlProtocol` trait doesn't try to unify framing. It
//! unifies *invocation*: given a tool name and arguments, every
//! adapter wants the same outcome classification
//! (`result | not_found | rate_limited | invalid_arguments |
//! forbidden | internal`). That's [`ControlOutcome`].

use std::sync::Arc;

use serde_json::{Value, json};
use smiths_core::Metrics;

use crate::dispatch::invoke_audited;
use crate::rate_limit::RateLimiter;
use crate::tool::{ToolContext, ToolError, ToolRegistry};

/// One tool-call outcome, normalized across adapters. Each variant
/// maps to a stable HTTP status + JSON-RPC error code so downstream
/// wire layers render uniform errors.
#[derive(Debug)]
pub enum ControlOutcome {
    /// Tool ran to completion.
    Ok(Value),
    /// Input didn't match the tool's declared schema.
    InvalidArguments(String),
    /// Tool or referenced resource didn't exist.
    NotFound(String),
    /// Rate-limiter / auth path denied.
    Forbidden(String),
    /// Tool raised an unexpected internal error.
    Internal(String),
}

impl ControlOutcome {
    /// Convert a `Result<Value, ToolError>` into an outcome.
    #[must_use]
    pub fn from_result(r: Result<Value, ToolError>) -> Self {
        match r {
            Ok(v) => Self::Ok(v),
            Err(ToolError::InvalidArguments(m)) => Self::InvalidArguments(m),
            Err(ToolError::NotFound(m)) => Self::NotFound(m),
            Err(ToolError::Forbidden(m)) => Self::Forbidden(m),
            Err(ToolError::Internal(m)) => Self::Internal(m),
        }
    }

    /// HTTP status code typically paired with this outcome. Adapters
    /// that don't use HTTP ignore this.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Ok(_) => 200,
            Self::InvalidArguments(_) => 400,
            Self::NotFound(_) => 404,
            Self::Forbidden(_) => 403,
            Self::Internal(_) => 500,
        }
    }

    /// JSON-RPC error code paired with this outcome. Picked to match
    /// the MCP and A2A adapters' historical assignments so existing
    /// clients see no behaviour change.
    #[must_use]
    pub fn json_rpc_code(&self) -> i64 {
        match self {
            Self::Ok(_) => 0,
            Self::InvalidArguments(_) => -32602,
            Self::NotFound(_) => -32601,
            Self::Forbidden(_) => -32001,
            Self::Internal(_) => -32603,
        }
    }

    /// Borrow the payload / error message regardless of variant.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Ok(_) => "ok",
            Self::InvalidArguments(m)
            | Self::NotFound(m)
            | Self::Forbidden(m)
            | Self::Internal(m) => m,
        }
    }
}

/// Shared dispatch helper. Holds `Arc` handles to the tool registry,
/// rate limiter, metrics, and tool context so each adapter call
/// only needs the `actor` label + `tool` name + args.
///
/// Cloning is `Arc`-cheap; stash one per adapter.
#[derive(Clone)]
pub struct ProtocolDispatch {
    /// Tool registry (MCP/A2A/webhook all share one).
    pub registry: Arc<ToolRegistry>,
    /// Rate limiter — enforces per-tool quotas.
    pub rate_limiter: Arc<RateLimiter>,
    /// Metrics handle.
    pub metrics: Arc<Metrics>,
    /// Engine-wide tool context. Cloned per-adapter.
    pub ctx: ToolContext,
}

impl ProtocolDispatch {
    /// Dispatch one tool call through the standard `invoke_audited`
    /// pipeline. Returns a [`ControlOutcome`] that the adapter
    /// renders into its wire shape.
    pub async fn invoke(&self, actor: &str, tool: &str, args: Value) -> ControlOutcome {
        let res = invoke_audited(
            &self.registry,
            &self.rate_limiter,
            &self.metrics,
            &self.ctx,
            actor,
            tool,
            args,
        )
        .await;
        ControlOutcome::from_result(res)
    }
}

/// Trait an adapter implements to declare which control-plane
/// protocol it speaks. Implementations are thin — each adapter owns
/// its listener (stdio / TCP / UDP) and frames its wire protocol;
/// this trait just names it so discovery / docs / tests can
/// introspect.
pub trait ControlProtocol: Send + Sync + 'static {
    /// Short stable identifier (`"mcp-stdio"`, `"a2a-http"`,
    /// `"webhook-http"`). Logged + returned on `.well-known/agent.json`.
    fn label(&self) -> &'static str;

    /// Human-readable one-liner exposed in agent discovery.
    fn description(&self) -> &'static str;

    /// How agents should frame a call. One of `"jsonrpc-2.0"`,
    /// `"webhook"`, `"grpc"`, …. Purely advisory.
    fn framing(&self) -> &'static str;
}

/// Build the shared agent-discovery document. MCP and A2A both
/// serve this at `/.well-known/agent.json` — centralising it here
/// keeps the fields consistent across adapters.
#[must_use]
pub fn agent_card(
    dispatch: &ProtocolDispatch,
    adapters: &[&dyn ControlProtocol],
    endpoint: &str,
) -> Value {
    let tools: Vec<Value> = dispatch
        .registry
        .iter()
        .map(|t| {
            json!({
                "name":        t.name(),
                "description": t.description(),
                "inputSchema": t.input_schema(),
            })
        })
        .collect();
    let adapter_descriptors: Vec<Value> = adapters
        .iter()
        .map(|a| {
            json!({
                "label":       a.label(),
                "description": a.description(),
                "framing":     a.framing(),
            })
        })
        .collect();
    json!({
        "name":        "smiths-net",
        "version":     env!("CARGO_PKG_VERSION"),
        "description": "Lightweight, AI-first SIP engine control plane.",
        "endpoint":    endpoint,
        "adapters":    adapter_descriptors,
        "capabilities": { "tools": tools },
    })
}

/// The three adapters this crate ships. Exposed so operators who
/// embed the crate can include them in a hand-rolled discovery
/// response without re-declaring each one.
pub struct McpStdioProtocol;
impl ControlProtocol for McpStdioProtocol {
    fn label(&self) -> &'static str {
        "mcp-stdio"
    }
    fn description(&self) -> &'static str {
        "MCP (Model Context Protocol) over stdio, newline-delimited JSON-RPC 2.0."
    }
    fn framing(&self) -> &'static str {
        "jsonrpc-2.0"
    }
}

/// A2A HTTP adapter.
pub struct A2aHttpProtocol;
impl ControlProtocol for A2aHttpProtocol {
    fn label(&self) -> &'static str {
        "a2a-http"
    }
    fn description(&self) -> &'static str {
        "Agent-to-Agent HTTP JSON-RPC 2.0 adapter."
    }
    fn framing(&self) -> &'static str {
        "jsonrpc-2.0"
    }
}

/// Generic webhook adapter — POST `/hook/<tool>` with `{args}`.
pub struct WebhookHttpProtocol;
impl ControlProtocol for WebhookHttpProtocol {
    fn label(&self) -> &'static str {
        "webhook-http"
    }
    fn description(&self) -> &'static str {
        "Generic HTTP webhook: POST /hook/<tool> with a JSON body of args."
    }
    fn framing(&self) -> &'static str {
        "webhook"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_from_result_round_trips() {
        let ok = ControlOutcome::from_result(Ok(json!(42)));
        assert!(matches!(ok, ControlOutcome::Ok(_)));
        assert_eq!(ok.http_status(), 200);

        let bad = ControlOutcome::from_result(Err(ToolError::InvalidArguments("nope".into())));
        assert_eq!(bad.http_status(), 400);
        assert_eq!(bad.json_rpc_code(), -32602);
        assert_eq!(bad.message(), "nope");

        let nf = ControlOutcome::from_result(Err(ToolError::NotFound("x".into())));
        assert_eq!(nf.http_status(), 404);

        let f = ControlOutcome::from_result(Err(ToolError::Forbidden("rate".into())));
        assert_eq!(f.http_status(), 403);

        let i = ControlOutcome::from_result(Err(ToolError::Internal("boom".into())));
        assert_eq!(i.http_status(), 500);
    }

    #[test]
    fn builtin_protocols_have_stable_labels() {
        assert_eq!(McpStdioProtocol.label(), "mcp-stdio");
        assert_eq!(A2aHttpProtocol.label(), "a2a-http");
        assert_eq!(WebhookHttpProtocol.label(), "webhook-http");
    }
}
