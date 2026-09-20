//! `ControlProtocol` trait plus the shared [`ProtocolDispatch`] that
//! MCP (stdio / HTTP), A2A, the generic webhook, and any future
//! adapter (gRPC, Matrix, AMQP) route through.
//!
//! ## What's "shared"
//!
//! Every adapter funnels tool calls through
//! [`crate::dispatch::invoke_audited_as`], and every JSON-RPC adapter
//! answers the MCP method set (`initialize`, `ping`, `tools/list`,
//! `tools/call`, `resources/list`, `resources/read`) from
//! [`ProtocolDispatch::handle_jsonrpc`]. What varies is framing:
//!
//! * **MCP stdio** — newline-delimited JSON-RPC 2.0 per stdin line.
//! * **MCP HTTP** — Streamable HTTP: `POST /mcp` with JSON or SSE
//!   responses, `GET /mcp` for the server→client stream.
//! * **A2A HTTP** — POST `{jsonrpc, method, id, params}` to `/a2a`.
//! * **Webhook HTTP** — POST `{args}` to `/hook/<tool>`.
//!
//! The `ControlProtocol` trait doesn't try to unify framing. It
//! unifies *invocation*: given a tool name and arguments, every
//! adapter wants the same outcome classification
//! (`result | not_found | rate_limited | invalid_arguments |
//! forbidden | conflict | internal`). That's [`ControlOutcome`].

use std::sync::Arc;

use serde_json::{Map, Value, json};
use smiths_core::Metrics;
use tracing::debug;

use crate::dispatch::invoke_audited_as;
use crate::jsonrpc::{
    self, ERR_INVALID_PARAMS, ERR_METHOD_NOT_FOUND, ERR_PARSE, Request, error_response,
    success_response,
};
use crate::rate_limit::RateLimiter;
use crate::resource::ResourceRegistry;
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
    /// Referent exists but its state rejects the operation.
    Conflict(String),
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
            Err(ToolError::Conflict(m)) => Self::Conflict(m),
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
            Self::Conflict(_) => 409,
            Self::Internal(_) => 500,
        }
    }

    /// JSON-RPC error code paired with this outcome — the same table
    /// every JSON-RPC adapter uses (see [`crate::jsonrpc`]).
    #[must_use]
    pub fn json_rpc_code(&self) -> i64 {
        match self {
            Self::Ok(_) => 0,
            Self::InvalidArguments(_) => jsonrpc::ERR_INVALID_PARAMS,
            Self::NotFound(_) => jsonrpc::ERR_TOOL_NOT_FOUND,
            Self::Forbidden(_) => jsonrpc::ERR_FORBIDDEN,
            Self::Conflict(_) => jsonrpc::ERR_CONFLICT,
            Self::Internal(_) => jsonrpc::ERR_INTERNAL,
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
            | Self::Conflict(m)
            | Self::Internal(m) => m,
        }
    }
}

/// Shared dispatch helper. Holds `Arc` handles to the tool registry,
/// resource registry, rate limiter, metrics, and tool context so each
/// adapter call only needs the `actor` label + caller identity +
/// tool name + args.
///
/// Cloning is `Arc`-cheap; stash one per adapter.
#[derive(Clone)]
pub struct ProtocolDispatch {
    /// Tool registry (MCP/A2A/webhook all share one).
    pub registry: Arc<ToolRegistry>,
    /// Resource registry served by `resources/list` / `resources/read`.
    pub resources: Arc<ResourceRegistry>,
    /// Rate limiter — enforces per-caller / per-tool quotas.
    pub rate_limiter: Arc<RateLimiter>,
    /// Metrics handle.
    pub metrics: Arc<Metrics>,
    /// Engine-wide tool context. Cloned per-adapter.
    pub ctx: ToolContext,
}

impl ProtocolDispatch {
    /// Bundle the shared handles.
    #[must_use]
    pub fn new(
        registry: Arc<ToolRegistry>,
        resources: Arc<ResourceRegistry>,
        rate_limiter: Arc<RateLimiter>,
        metrics: Arc<Metrics>,
        ctx: ToolContext,
    ) -> Self {
        Self {
            registry,
            resources,
            rate_limiter,
            metrics,
            ctx,
        }
    }

    /// Dispatch one tool call on the tool's global rate-limit bucket.
    pub async fn invoke(&self, actor: &str, tool: &str, args: Value) -> ControlOutcome {
        self.invoke_as(actor, None, tool, args).await
    }

    /// Dispatch one tool call through the standard `invoke_audited`
    /// pipeline on behalf of `caller` (peer address, session id).
    /// Returns a [`ControlOutcome`] that the adapter renders into its
    /// wire shape.
    pub async fn invoke_as(
        &self,
        actor: &str,
        caller: Option<&str>,
        tool: &str,
        args: Value,
    ) -> ControlOutcome {
        let res = invoke_audited_as(
            &self.registry,
            &self.rate_limiter,
            &self.metrics,
            &self.ctx,
            actor,
            caller,
            tool,
            args,
        )
        .await;
        ControlOutcome::from_result(res)
    }

    /// Parse one raw JSON-RPC text frame and answer it. A parse error
    /// yields an error frame with `id: null`; a notification yields
    /// `None`.
    pub async fn handle_frame(
        &self,
        actor: &str,
        caller: Option<&str>,
        text: &str,
    ) -> Option<Value> {
        let req: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(
                    &Value::Null,
                    ERR_PARSE,
                    &format!("parse error: {e}"),
                ));
            }
        };
        self.handle_jsonrpc(actor, caller, &req).await
    }

    /// Answer one parsed JSON-RPC message. Notifications (no `id`)
    /// return `None` — per JSON-RPC 2.0 they never get a response,
    /// even on error.
    pub async fn handle_jsonrpc(
        &self,
        actor: &str,
        caller: Option<&str>,
        req: &Value,
    ) -> Option<Value> {
        let request = match Request::from_value(req) {
            Ok(r) => r,
            Err(frame) => {
                return if req.get("id").is_none() {
                    None
                } else {
                    Some(frame)
                };
            }
        };
        let result = self
            .dispatch_method(actor, caller, &request.method, request.params)
            .await;
        if request.is_notification {
            return None;
        }
        Some(match result {
            Ok(value) => success_response(&request.id, value),
            Err((code, message)) => error_response(&request.id, code, &message),
        })
    }

    /// Route one MCP method. Adapters that need a different framing
    /// for a method (A2A's `tools/call`) intercept before calling
    /// this.
    pub async fn dispatch_method(
        &self,
        actor: &str,
        caller: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(crate::mcp::initialize_response(&params)),
            "initialized" | "notifications/initialized" | "shutdown" => Ok(Value::Null),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.tools_list()),
            "tools/call" => self.tools_call(actor, caller, params).await,
            "resources/list" => Ok(self.resources_list()),
            "resources/read" => self.resources_read(params).await,
            other => Err((ERR_METHOD_NOT_FOUND, format!("unknown method: {other}"))),
        }
    }

    /// `tools/list` result.
    #[must_use]
    pub fn tools_list(&self) -> Value {
        json!({ "tools": tool_descriptors(&self.registry) })
    }

    /// `resources/list` result.
    #[must_use]
    pub fn resources_list(&self) -> Value {
        let resources: Vec<_> = self
            .resources
            .iter()
            .map(|r| {
                json!({
                    "uri":         r.uri(),
                    "name":        r.uri(),
                    "description": r.description(),
                })
            })
            .collect();
        json!({ "resources": resources })
    }

    /// `resources/read` — `{contents: [{uri, mimeType, text}]}`.
    pub async fn resources_read(&self, params: Value) -> Result<Value, (i64, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or((ERR_INVALID_PARAMS, "missing `uri`".to_owned()))?;
        let Some(resource) = self.resources.get(uri) else {
            return Err((ERR_METHOD_NOT_FOUND, format!("unknown resource: {uri}")));
        };
        match resource.read(&self.ctx).await {
            Ok(content) => Ok(json!({
                "contents": [ {
                    "uri":      uri,
                    "mimeType": content.mime_type(),
                    "text":     content.text(),
                } ],
            })),
            Err(e) => Err(jsonrpc::map_tool_error(&e)),
        }
    }

    /// Pull `name` + `arguments` out of `tools/call` params.
    pub fn tool_call_params(params: &Value) -> Result<(&str, Value), (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((ERR_INVALID_PARAMS, "missing `name`".to_owned()))?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::default()));
        Ok((name, args))
    }

    /// `tools/call` with MCP framing: tool output becomes a single
    /// JSON text block plus `structuredContent`; tool errors travel
    /// inside the success envelope with `isError: true`, as the MCP
    /// spec requires, rather than as JSON-RPC error frames.
    pub async fn tools_call(
        &self,
        actor: &str,
        caller: Option<&str>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let (name, args) = Self::tool_call_params(&params)?;
        match self.invoke_as(actor, caller, name, args).await {
            ControlOutcome::Ok(output) => {
                let text = serde_json::to_string(&output).unwrap_or_default();
                Ok(json!({
                    "content": [ { "type": "text", "text": text } ],
                    "isError": false,
                    "structuredContent": output,
                }))
            }
            outcome => {
                let message = outcome.message();
                debug!(%name, %message, "tool returned error");
                Ok(json!({
                    "content": [ { "type": "text", "text": message } ],
                    "isError": true,
                }))
            }
        }
    }
}

/// `{name, description, inputSchema}` for every registered tool —
/// the shape shared by `tools/list` and the agent card.
#[must_use]
pub fn tool_descriptors(registry: &ToolRegistry) -> Vec<Value> {
    registry
        .iter()
        .map(|t| {
            json!({
                "name":        t.name(),
                "description": t.description(),
                "inputSchema": t.input_schema(),
            })
        })
        .collect()
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

/// Build the shared agent-discovery document. A2A and the webhook
/// adapter both serve this at `/.well-known/agent.json` —
/// centralising it here keeps the fields consistent across adapters.
#[must_use]
pub fn agent_card(
    dispatch: &ProtocolDispatch,
    adapters: &[&dyn ControlProtocol],
    endpoint: &str,
) -> Value {
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
        "capabilities": { "tools": tool_descriptors(&dispatch.registry) },
    })
}

/// The adapters this crate ships. Exposed so operators who embed the
/// crate can include them in a hand-rolled discovery response without
/// re-declaring each one.
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

/// MCP Streamable HTTP adapter.
pub struct McpHttpProtocol;
impl ControlProtocol for McpHttpProtocol {
    fn label(&self) -> &'static str {
        "mcp-http"
    }
    fn description(&self) -> &'static str {
        "MCP Streamable HTTP (2025-03-26): POST /mcp, GET /mcp (SSE), DELETE /mcp."
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
    use crate::control::ControlState;
    use crate::tool::test_support::{default_config, empty_registry, null_media};
    use smiths_core::{EventBus, RateLimitConfig};
    use tokio_util::sync::CancellationToken;

    pub(crate) fn test_dispatch() -> (ProtocolDispatch, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        let ctx = ToolContext::new(state, empty_registry(), default_config(), null_media());
        let dispatch = ProtocolDispatch::new(
            Arc::new(crate::tools::builtin_registry()),
            Arc::new(crate::resource::builtin_registry()),
            Arc::new(RateLimiter::new(&RateLimitConfig::default())),
            Metrics::noop(),
            ctx,
        );
        (dispatch, cancel)
    }

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
        assert_eq!(nf.json_rpc_code(), jsonrpc::ERR_TOOL_NOT_FOUND);

        let f = ControlOutcome::from_result(Err(ToolError::Forbidden("rate".into())));
        assert_eq!(f.http_status(), 403);
        assert_eq!(f.json_rpc_code(), jsonrpc::ERR_FORBIDDEN);

        let c = ControlOutcome::from_result(Err(ToolError::Conflict("busy".into())));
        assert_eq!(c.http_status(), 409);
        assert_eq!(c.json_rpc_code(), jsonrpc::ERR_CONFLICT);

        let i = ControlOutcome::from_result(Err(ToolError::Internal("boom".into())));
        assert_eq!(i.http_status(), 500);
    }

    #[test]
    fn builtin_protocols_have_stable_labels() {
        assert_eq!(McpStdioProtocol.label(), "mcp-stdio");
        assert_eq!(McpHttpProtocol.label(), "mcp-http");
        assert_eq!(A2aHttpProtocol.label(), "a2a-http");
        assert_eq!(WebhookHttpProtocol.label(), "webhook-http");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn handle_frame_answers_requests_and_swallows_notifications() {
        let (d, _cancel) = test_dispatch();
        let resp = d
            .handle_frame("t", None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .await
            .unwrap();
        assert_eq!(resp["id"], 1);
        assert!(resp["result"].is_object());
        assert!(
            d.handle_frame(
                "t",
                None,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .await
            .is_none()
        );
        // Malformed notification (no id, no method) is still silent.
        assert!(
            d.handle_frame("t", None, r#"{"jsonrpc":"2.0"}"#)
                .await
                .is_none()
        );
        let parse = d.handle_frame("t", None, "{not json").await.unwrap();
        assert_eq!(parse["error"]["code"], ERR_PARSE);
        assert!(parse["id"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_call_wraps_tool_errors_in_is_error_envelope() {
        let (d, _cancel) = test_dispatch();
        let resp = d
            .handle_frame(
                "t",
                None,
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"get_call_status","arguments":{"call_id":"nope"}}}"#,
            )
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp.get("error").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_card_lists_tools_and_adapters() {
        let (d, _cancel) = test_dispatch();
        let card = agent_card(&d, &[&McpStdioProtocol, &McpHttpProtocol], "/x");
        assert_eq!(card["endpoint"], "/x");
        assert!(
            card["capabilities"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "health")
        );
        assert_eq!(card["adapters"].as_array().unwrap().len(), 2);
    }
}
