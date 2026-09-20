//! Generic HTTP webhook adapter.
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
//! 409 { "error": "..." } on a state conflict
//! 500 { "error": "..." }    on internal failure
//! ```
//!
//! No JSON-RPC envelope, no `id`, no `method` — the tool is in the
//! path, the args are the body. Same underlying
//! `ProtocolDispatch::invoke_as` as MCP/A2A so auth + rate-limit +
//! metrics + audit all apply identically.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, extract};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::bearer_gate;
use crate::control_protocol::{
    A2aHttpProtocol, ControlOutcome, ControlProtocol, McpHttpProtocol, McpStdioProtocol,
    ProtocolDispatch, WebhookHttpProtocol, agent_card,
};

/// Actor label recorded in audit events for this adapter.
const ACTOR: &str = "webhook-http";

/// Bind on `addr` and serve the webhook adapter until `cancel` fires.
pub async fn serve_http(
    addr: SocketAddr,
    dispatch: ProtocolDispatch,
    bearer_token: Option<String>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let bearer: Option<Arc<str>> = bearer_token.map(Arc::from);
    // `/hook/*` requires bearer auth; discovery + health stay
    // public so load balancers + agent-registries can still read
    // `agent.json` without credentials.
    let app = Router::new()
        .route(
            "/hook/{tool}",
            post(invoke).route_layer(middleware::from_fn_with_state(bearer, bearer_gate)),
        )
        .route("/.well-known/agent.json", get(discovery))
        .route("/health", get(health))
        .with_state(dispatch);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "webhook HTTP server listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { cancel.cancelled().await })
    .await
    .map_err(std::io::Error::other)?;
    info!("webhook HTTP server stopped");
    Ok(())
}

async fn invoke(
    State(dispatch): State<ProtocolDispatch>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(tool): Path<String>,
    body: Option<extract::Json<Value>>,
) -> impl IntoResponse {
    let args = body.map_or(Value::Null, |extract::Json(v)| v);
    let caller = peer.ip().to_string();
    let outcome = dispatch.invoke_as(ACTOR, Some(&caller), &tool, args).await;
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

async fn discovery(State(dispatch): State<ProtocolDispatch>) -> impl IntoResponse {
    let adapters: &[&dyn ControlProtocol] = &[
        &McpStdioProtocol,
        &McpHttpProtocol,
        &A2aHttpProtocol,
        &WebhookHttpProtocol,
    ];
    Json(agent_card(&dispatch, adapters, "/hook/<tool>"))
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
        assert_eq!(ControlOutcome::Conflict("x".into()).http_status(), 409);
    }
}
