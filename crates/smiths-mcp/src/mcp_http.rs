//! MCP over HTTP + SSE.
//!
//! Two routes, same dispatcher as [`crate::mcp`]:
//!
//! - `POST /mcp` — JSON-RPC 2.0 request → response body.
//! - `GET  /mcp/events` — server-sent-event stream mirroring the
//!   stdio server's bus-driven notifications (`call/created`,
//!   `call/terminated`).
//!
//! Kept deliberately thin. The adapter owns the transport; dispatch,
//! rate limit, audit, and metrics live on [`crate::dispatch`] so the
//! HTTP path stays behaviourally identical to stdio.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, extract};
use futures::stream::{Stream, StreamExt};
use serde_json::{Value, json};
use smiths_core::{EventBus, Metrics};
use tokio::net::TcpListener;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::mcp::{
    ERR_INVALID_REQUEST, ERR_METHOD_NOT_FOUND, ERR_PARSE, dispatch, event_to_notification,
};
use crate::rate_limit::RateLimiter;
use crate::resource::ResourceRegistry;
use crate::tool::{ToolContext, ToolRegistry};

/// Actor label for audit events coming in over this adapter.
const ACTOR: &str = "mcp-http";

/// Wiring shared between routes.
#[derive(Clone)]
struct AppState {
    registry: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    rate_limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    ctx: ToolContext,
    bus: EventBus,
}

/// Bind on `addr` and serve MCP over HTTP + SSE until `cancel` fires.
#[allow(clippy::too_many_arguments)]
pub async fn serve_http(
    addr: SocketAddr,
    registry: Arc<ToolRegistry>,
    resources: Arc<ResourceRegistry>,
    rate_limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    ctx: ToolContext,
    bus: EventBus,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let state = AppState {
        registry,
        resources,
        rate_limiter,
        metrics,
        ctx,
        bus,
    };
    let app = Router::new()
        .route("/mcp", post(rpc))
        .route("/mcp/events", get(events))
        .with_state(state);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "MCP HTTP listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .map_err(std::io::Error::other)?;
    info!("MCP HTTP server stopped");
    Ok(())
}

// ---- /mcp ----

async fn rpc(
    State(state): State<AppState>,
    extract::Json(req): extract::Json<Value>,
) -> impl IntoResponse {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return Json(error_response(&id, ERR_INVALID_REQUEST, "missing method"));
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let result = dispatch(
        method,
        params,
        &state.registry,
        &state.resources,
        &state.rate_limiter,
        &state.metrics,
        &state.ctx,
        ACTOR,
    )
    .await;

    match result {
        Ok(value) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": value })),
        Err((code, message)) => Json(error_response(&id, code, &message)),
    }
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

// ---- /mcp/events ----

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.bus.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(ev) => event_to_notification(&ev).map(|frame| {
                let data = frame.to_string();
                let method = frame
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("notification")
                    .to_owned();
                Ok(Event::default().event(method).data(data))
            }),
            Err(e) => {
                warn!(?e, "mcp sse bus stream error");
                None
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

// Keep constants in scope for potential future handlers (matches
// the pattern in a2a.rs / mcp.rs).
#[allow(dead_code)]
const _FORCE_USE: &[i64] = &[ERR_PARSE, ERR_METHOD_NOT_FOUND];
