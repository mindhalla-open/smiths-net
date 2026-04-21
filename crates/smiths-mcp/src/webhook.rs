//! Generic HTTP webhook adapter — slice 4.4 / P20.
//!
//! The MCP and A2A adapters wrap JSON-RPC 2.0 around tool
//! invocations. That's ergonomic for agent frameworks (LLM hosts,
//! other agents running another MCP client); it's awkward for the
//! tier of callers that only speak "POST some JSON, get some JSON"
//! — Zapier, Make, Retool, bare `curl` from a shell script,
//! Slack-slash-command webhooks. This adapter accepts
//!
//! ```text
//! POST /hook/<tool>
//! Content-Type: application/json
//! { ...args... }
//! ```
//!
//! and returns
//!
//! ```text
//! 200 { "result": ... }     on success
//! 400 { "error": "..." }    on invalid-arguments
//! 403 { "error": "..." }    on rate-limit / auth
//! 404 { "error": "..." }    on unknown tool / referent
//! 500 { "error": "..." }    on internal failure
//! ```
//!
//! No JSON-RPC envelope, no `id`, no `method` — the tool is in the
//! path, the args are the body. Same underlying
//! `ProtocolDispatch::invoke` as MCP/A2A so auth + rate-limit +
//! metrics + audit all apply identically.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, extract};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::control_protocol::{
    A2aHttpProtocol, ControlOutcome, ControlProtocol, McpStdioProtocol, ProtocolDispatch,
    WebhookHttpProtocol, agent_card,
};
use crate::resource::ResourceRegistry;

// `_resources` below is kept on `AppState` for symmetry with the
// MCP + A2A adapters and so a future webhook `GET /resource/...`
// path doesn't need a signature change.

/// Actor label recorded in audit events for this adapter.
const ACTOR: &str = "webhook-http";

/// Shared state threaded onto every request.
#[derive(Clone)]
struct AppState {
    dispatch: ProtocolDispatch,
    #[allow(dead_code)] // reserved for future `GET /resource/...`; see module-level note.
    resources: Arc<ResourceRegistry>,
    bearer_token: Option<Arc<str>>,
}

/// Bind on `addr` and serve the webhook adapter until `cancel` fires.
pub async fn serve_http(
    addr: SocketAddr,
    dispatch: ProtocolDispatch,
    resources: Arc<ResourceRegistry>,
    bearer_token: Option<String>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let state = AppState {
        dispatch,
        resources,
        bearer_token: bearer_token.map(Arc::from),
    };
    // `/hook/*` requires bearer auth; discovery + health stay
    // public so load balancers + agent-registries can still read
    // `agent.json` without credentials.
    let app = Router::new()
        .route(
            "/hook/{tool}",
            post(invoke).route_layer(middleware::from_fn_with_state(state.clone(), bearer_auth)),
        )
        .route("/.well-known/agent.json", get(discovery))
        .route("/health", get(health))
        .with_state(state);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "webhook HTTP server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .map_err(std::io::Error::other)?;
    info!("webhook HTTP server stopped");
    Ok(())
}

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

async fn invoke(
    State(state): State<AppState>,
    Path(tool): Path<String>,
    body: Option<extract::Json<Value>>,
) -> impl IntoResponse {
    let args = body.map_or(Value::Null, |extract::Json(v)| v);
    let outcome = state.dispatch.invoke(ACTOR, &tool, args).await;
    let status =
        StatusCode::from_u16(outcome.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = match &outcome {
        ControlOutcome::Ok(v) => json!({ "result": v }),
        _ => json!({ "error": outcome.message() }),
    };
    if !matches!(outcome, ControlOutcome::Ok(_)) {
        warn!(%tool, status = %status.as_u16(), "webhook tool error: {}", outcome.message());
    }
    (status, Json(body))
}

async fn discovery(State(state): State<AppState>) -> impl IntoResponse {
    let adapters: &[&dyn ControlProtocol] =
        &[&McpStdioProtocol, &A2aHttpProtocol, &WebhookHttpProtocol];
    Json(agent_card(&state.dispatch, adapters, "/hook/<tool>"))
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_status_table() {
        assert_eq!(ControlOutcome::Ok(Value::Null).http_status(), 200);
        assert_eq!(ControlOutcome::NotFound("x".into()).http_status(), 404);
    }
}
