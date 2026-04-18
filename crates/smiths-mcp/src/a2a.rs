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
//! Methods exposed over `/a2a` mirror the MCP adapter: `tools/list`,
//! `tools/call`, `ping`, `initialize`. Two adapters, one registry.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, extract};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::tool::{ToolContext, ToolError, ToolRegistry};

/// Wiring shared between routes.
#[derive(Clone)]
struct AppState {
    registry: Arc<ToolRegistry>,
    ctx: ToolContext,
}

/// Bind on `addr` and serve the A2A API until `cancel` fires.
pub async fn serve_http(
    addr: SocketAddr,
    registry: Arc<ToolRegistry>,
    ctx: ToolContext,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let state = AppState { registry, ctx };
    let app = Router::new()
        .route("/.well-known/agent.json", get(agent_card))
        .route("/a2a", post(rpc))
        .route("/health", get(health))
        .with_state(state);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "A2A HTTP server listening");
    axum::serve(listener, app)
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
async fn agent_card(State(state): State<AppState>) -> impl IntoResponse {
    let tools: Vec<_> = state
        .registry
        .iter()
        .map(|t| {
            json!({
                "name": t.name(),
                "description": t.description(),
                "inputSchema": t.input_schema(),
            })
        })
        .collect();
    Json(json!({
        "name": "smiths-net",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Lightweight, AI-first SIP engine control plane.",
        "endpoint": "/a2a",
        "capabilities": { "tools": tools },
    }))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

async fn rpc(
    State(state): State<AppState>,
    extract::Json(req): extract::Json<Value>,
) -> impl IntoResponse {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result = match method.as_str() {
        "" => Err((ERR_INVALID_REQUEST, "missing method".to_owned())),
        "ping" => Ok(json!({})),
        "initialize" => Ok(json!({
            "serverInfo": { "name": "smiths-net", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "tools": { "listChanged": false } },
        })),
        "tools/list" => Ok(json!({
            "tools": state.registry.iter().map(|t| json!({
                "name": t.name(),
                "description": t.description(),
                "inputSchema": t.input_schema(),
            })).collect::<Vec<_>>()
        })),
        "tools/call" => invoke_tool(&state, params).await,
        other => Err((ERR_METHOD_NOT_FOUND, format!("unknown method: {other}"))),
    };

    match result {
        Ok(value) => (
            StatusCode::OK,
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": value })),
        ),
        Err((code, message)) => {
            warn!(%method, %code, %message, "a2a rpc error");
            (
                StatusCode::OK, // JSON-RPC transports errors in body, not HTTP status
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                })),
            )
        }
    }
}

async fn invoke_tool(state: &AppState, params: Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "missing `name`".to_owned()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::default()));

    let Some(tool) = state.registry.get(name) else {
        return Err((ERR_METHOD_NOT_FOUND, format!("unknown tool: {name}")));
    };
    match tool.call(args, &state.ctx).await {
        Ok(output) => Ok(json!({ "output": output, "isError": false })),
        Err(e) => match e {
            ToolError::InvalidArguments(m) => Err((ERR_INVALID_PARAMS, m)),
            ToolError::NotFound(m) => Err((ERR_TOOL_NOT_FOUND, m)),
            ToolError::Forbidden(m) => Err((ERR_FORBIDDEN, m)),
            ToolError::Internal(m) => Err((ERR_INTERNAL, m)),
        },
    }
}

const ERR_INVALID_REQUEST: i64 = -32600;
const ERR_METHOD_NOT_FOUND: i64 = -32601;
const ERR_INVALID_PARAMS: i64 = -32602;
const ERR_INTERNAL: i64 = -32603;
const ERR_TOOL_NOT_FOUND: i64 = -32001;
const ERR_FORBIDDEN: i64 = -32002;
