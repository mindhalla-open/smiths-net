//! A2A (agent-to-agent) HTTP adapter — JSON-RPC over HTTP POST.
//!
//! Simplified compared to the full Google A2A spec: we implement the
//! minimum needed for another agent (Python, Node, LLM-driven, ...) to
//! discover and invoke the engine's tools.
//!
//! Endpoints:
//!
//! * `GET  /.well-known/agent.json` — agent card (discovery).
//! * `POST /a2a`                    — JSON-RPC 2.0 entry point.
//! * `GET  /health`                 — plain health (no JSON-RPC).
//!
//! Methods exposed over `/a2a` mirror the MCP adapter and route
//! through the same [`ProtocolDispatch`]: `initialize`, `ping`,
//! `tools/list`, `tools/call`, `resources/list`, `resources/read`.
//! The one framing difference is `tools/call`: A2A returns the tool
//! output as `{output, isError: false}` and surfaces tool errors as
//! JSON-RPC error frames, where MCP wraps them in `isError: true`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, extract};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use smiths_core::Metrics;

use crate::auth::bearer_gate;
use crate::control_protocol::{
    A2aHttpProtocol, ControlOutcome, ControlProtocol, McpHttpProtocol, McpStdioProtocol,
    ProtocolDispatch, WebhookHttpProtocol, agent_card,
};
use crate::jsonrpc::{Request, error_response, success_response};
use crate::rate_limit::RateLimiter;
use crate::resource::ResourceRegistry;
use crate::tool::{ToolContext, ToolRegistry};

/// Actor label recorded in audit events for this adapter.
const ACTOR: &str = "a2a-http";

/// Bind on `addr` and serve the A2A API until `cancel` fires. When
/// `bearer_token` is set, every `/a2a` request must carry a matching
/// `Authorization: Bearer <token>` or gets `401`.
#[allow(clippy::too_many_arguments)] // intentionally many — one wiring point is clearer than a builder for today's flow
pub async fn serve_http(
    addr: SocketAddr,
    registry: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    rate_limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    bearer_token: Option<String>,
    ctx: ToolContext,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let dispatch = ProtocolDispatch::new(registry, resources, rate_limiter, metrics, ctx);
    let bearer: Option<Arc<str>> = bearer_token.map(Arc::from);
    // Only `/a2a` requires auth; discovery (`agent.json`) and the
    // liveness probe (`health`) stay public so load balancers and
    // other agents can still find us.
    let app = Router::new()
        .route(
            "/a2a",
            post(rpc).route_layer(middleware::from_fn_with_state(bearer, bearer_gate)),
        )
        .route("/.well-known/agent.json", get(discovery))
        .route("/health", get(health))
        .with_state(dispatch);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "A2A HTTP server listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { cancel.cancelled().await })
    .await
    .map_err(std::io::Error::other)?;
    info!("A2A HTTP server stopped");
    Ok(())
}

// ---- handlers ----

/// Agent discovery card — the de-facto A2A `.well-known/agent.json`
/// shape. Agents read this to learn our name, capabilities, and
/// endpoint URL.
async fn discovery(State(dispatch): State<ProtocolDispatch>) -> impl IntoResponse {
    let adapters: &[&dyn ControlProtocol] = &[
        &McpStdioProtocol,
        &McpHttpProtocol,
        &A2aHttpProtocol,
        &WebhookHttpProtocol,
    ];
    Json(agent_card(&dispatch, adapters, "/a2a"))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn rpc(
    State(dispatch): State<ProtocolDispatch>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    extract::Json(req): extract::Json<Value>,
) -> impl IntoResponse {
    let request = match Request::from_value(&req) {
        Ok(r) => r,
        Err(frame) => return (StatusCode::OK, Json(frame)),
    };
    let caller = peer.ip().to_string();
    let result = if request.method == "tools/call" {
        invoke_tool(&dispatch, &caller, request.params).await
    } else {
        dispatch
            .dispatch_method(ACTOR, Some(&caller), &request.method, request.params)
            .await
    };
    // JSON-RPC transports errors in the body, not the HTTP status.
    match result {
        Ok(value) => (StatusCode::OK, Json(success_response(&request.id, value))),
        Err((code, message)) => {
            warn!(method = %request.method, %code, %message, "a2a rpc error");
            (
                StatusCode::OK,
                Json(error_response(&request.id, code, &message)),
            )
        }
    }
}

/// A2A framing of `tools/call`: `{output, isError: false}` on
/// success, a JSON-RPC error frame otherwise.
async fn invoke_tool(
    dispatch: &ProtocolDispatch,
    caller: &str,
    params: Value,
) -> Result<Value, (i64, String)> {
    let (name, args) = ProtocolDispatch::tool_call_params(&params)?;
    match dispatch.invoke_as(ACTOR, Some(caller), name, args).await {
        ControlOutcome::Ok(output) => Ok(json!({ "output": output, "isError": false })),
        outcome => Err((outcome.json_rpc_code(), outcome.message().to_owned())),
    }
}
