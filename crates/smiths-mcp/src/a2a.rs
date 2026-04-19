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
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, extract};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::dispatch::invoke_audited;
use crate::rate_limit::RateLimiter;
use crate::resource::ResourceRegistry;
use crate::tool::{ToolContext, ToolError, ToolRegistry};

/// Actor label recorded in audit events for this adapter.
const ACTOR: &str = "a2a-http";

/// Wiring shared between routes.
#[derive(Clone)]
struct AppState {
    registry: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    rate_limiter: Arc<RateLimiter>,
    ctx: ToolContext,
    /// When set, every request must carry matching
    /// `Authorization: Bearer <token>` or we return 401.
    bearer_token: Option<Arc<str>>,
}

/// Bind on `addr` and serve the A2A API until `cancel` fires.
pub async fn serve_http(
    addr: SocketAddr,
    registry: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    rate_limiter: Arc<RateLimiter>,
    bearer_token: Option<String>,
    ctx: ToolContext,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let state = AppState {
        registry,
        resources,
        rate_limiter,
        ctx,
        bearer_token: bearer_token.map(Arc::from),
    };
    // Only `/a2a` requires auth; discovery (`agent.json`) and the
    // liveness probe (`health`) stay public so load balancers and
    // other agents can still find us.
    let app = Router::new()
        .route(
            "/a2a",
            post(rpc).route_layer(middleware::from_fn_with_state(state.clone(), bearer_auth)),
        )
        .route("/.well-known/agent.json", get(agent_card))
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

/// Bearer-token gate applied to `/a2a`. Does nothing when
/// `state.bearer_token` is `None` (auth disabled).
async fn bearer_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = state.bearer_token.as_ref() else {
        return next.run(req).await;
    };
    let submitted = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(str::trim));
    if submitted.is_some_and(|t| t == expected.as_ref()) {
        return next.run(req).await;
    }
    (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
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
        "tools/call" => invoke_tool_http(&state, params).await,
        "resources/list" => Ok(json!({
            "resources": state.resources.iter().map(|r| json!({
                "uri":         r.uri(),
                "name":        r.uri(),
                "description": r.description(),
            })).collect::<Vec<_>>()
        })),
        "resources/read" => read_resource(&state, params).await,
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

async fn read_resource(state: &AppState, params: Value) -> Result<Value, (i64, String)> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "missing `uri`".to_owned()))?;
    let Some(resource) = state.resources.get(uri) else {
        return Err((ERR_METHOD_NOT_FOUND, format!("unknown resource: {uri}")));
    };
    match resource.read(&state.ctx).await {
        Ok(content) => Ok(json!({
            "contents": [ {
                "uri":      uri,
                "mimeType": content.mime_type(),
                "text":     content.text(),
            } ],
        })),
        Err(e) => Err(match e {
            ToolError::InvalidArguments(m) => (ERR_INVALID_PARAMS, m),
            ToolError::NotFound(m) => (ERR_TOOL_NOT_FOUND, m),
            ToolError::Forbidden(m) => (ERR_FORBIDDEN, m),
            ToolError::Internal(m) => (ERR_INTERNAL, m),
        }),
    }
}

async fn invoke_tool_http(state: &AppState, params: Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "missing `name`".to_owned()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::default()));

    match invoke_audited(
        &state.registry,
        &state.rate_limiter,
        &state.ctx,
        ACTOR,
        name,
        args,
    )
    .await
    {
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
