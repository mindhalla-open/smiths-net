//! MCP over HTTP — the Streamable HTTP transport (spec revision
//! 2025-03-26) plus the pre-existing SSE side channel.
//!
//! Routes (all share the dispatcher in [`crate::control_protocol`]):
//!
//! - `POST /mcp` — one JSON-RPC message (or a batch). Requests are
//!   answered either as `application/json` or, when the client's
//!   `Accept` prefers it, as a `text/event-stream` that carries the
//!   response(s) and then closes. A body holding only notifications
//!   or responses is acknowledged with `202 Accepted`.
//! - `GET /mcp` — long-lived `text/event-stream` of server→client
//!   notifications (`notifications/call/created`,...).
//! - `DELETE /mcp` — terminate the session named by `Mcp-Session-Id`.
//! - `GET /mcp/events` — legacy alias of `GET /mcp` for clients that
//!   predate Streamable HTTP.
//!
//! ## Sessions
//!
//! `initialize` mints an `Mcp-Session-Id` that the client echoes on
//! every later request. A request naming an unknown or expired
//! session gets `404`. A request with *no* session header is served
//! statelessly for backwards compatibility unless
//! [`McpHttpServer::with_require_session`] is on, in which case only
//! `initialize` may arrive without one (`400` otherwise). The
//! session store is bounded and idle sessions expire.
//!
//! ## Auth and rate limiting
//!
//! [`McpHttpServer::with_http_bearer`] gates every `/mcp*` route
//! behind a constant-time bearer check. The rate limiter is keyed
//! by session id when one is present, otherwise by the peer IP.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use dashmap::DashMap;
use futures::stream::{self, Stream, StreamExt};
use serde_json::{Value, json};
use smiths_core::{EventBus, Metrics};
use tokio::net::TcpListener;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::bearer_gate;
use crate::control_protocol::ProtocolDispatch;
use crate::jsonrpc::{ERR_PARSE, error_response};
use crate::mcp::{SUPPORTED_PROTOCOL_VERSIONS, event_to_notification, negotiate_protocol_version};
use crate::rate_limit::RateLimiter;
use crate::resource::ResourceRegistry;
use crate::tool::{ToolContext, ToolRegistry};

/// Actor label for audit events coming in over this adapter.
const ACTOR: &str = "mcp-http";

/// Session header name (spec: `Mcp-Session-Id`).
pub const SESSION_HEADER: &str = "mcp-session-id";
/// Protocol-version header name (spec revision 2025-06-18).
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";

/// Default idle lifetime of a session before it is reclaimed.
pub const DEFAULT_SESSION_IDLE: Duration = Duration::from_hours(1);
/// Default cap on concurrently tracked sessions.
pub const DEFAULT_MAX_SESSIONS: usize = 1024;

/// Builder for the MCP HTTP adapter.
pub struct McpHttpServer {
    dispatch: ProtocolDispatch,
    bus: EventBus,
    bearer: Option<Arc<str>>,
    require_session: bool,
    session_idle: Duration,
    max_sessions: usize,
}

impl McpHttpServer {
    /// Assemble the adapter from the engine's shared handles.
    #[must_use]
    pub fn new(
        registry: Arc<ToolRegistry>,
        resources: Arc<ResourceRegistry>,
        rate_limiter: Arc<RateLimiter>,
        metrics: Arc<Metrics>,
        ctx: ToolContext,
        bus: EventBus,
    ) -> Self {
        Self::from_dispatch(
            ProtocolDispatch::new(registry, resources, rate_limiter, metrics, ctx),
            bus,
        )
    }

    /// Assemble the adapter around an existing dispatcher.
    #[must_use]
    pub fn from_dispatch(dispatch: ProtocolDispatch, bus: EventBus) -> Self {
        Self {
            dispatch,
            bus,
            bearer: None,
            require_session: false,
            session_idle: DEFAULT_SESSION_IDLE,
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }

    /// Require `Authorization: Bearer <token>` on every `/mcp*`
    /// route. `None` leaves the adapter open (local development).
    /// Wire this from the operator's MCP HTTP bearer-token setting.
    #[must_use]
    pub fn with_http_bearer(mut self, token: Option<String>) -> Self {
        self.bearer = token.map(Arc::from);
        self
    }

    /// Reject non-`initialize` requests that carry no
    /// `Mcp-Session-Id` (`400`). Off by default so stateless legacy
    /// clients keep working.
    #[must_use]
    pub fn with_require_session(mut self, require: bool) -> Self {
        self.require_session = require;
        self
    }

    /// Idle lifetime after which a session is reclaimed.
    #[must_use]
    pub fn with_session_idle_timeout(mut self, idle: Duration) -> Self {
        self.session_idle = idle;
        self
    }

    /// Cap on concurrently tracked sessions; the least recently seen
    /// session is evicted to make room.
    #[must_use]
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max.max(1);
        self
    }

    /// Build the axum router (exposed so embedders can mount it
    /// under their own listener).
    pub fn router(self) -> Router {
        let state = AppState {
            dispatch: self.dispatch,
            bus: self.bus,
            sessions: Arc::new(SessionStore::new(self.session_idle, self.max_sessions)),
            require_session: self.require_session,
        };
        let gate = middleware::from_fn_with_state(self.bearer, bearer_gate);
        Router::new()
            .route(
                "/mcp",
                get(get_mcp)
                    .post(post_mcp)
                    .delete(delete_mcp)
                    .route_layer(gate.clone()),
            )
            .route("/mcp/events", get(legacy_events).route_layer(gate))
            .with_state(state)
    }

    /// Bind on `addr` and serve until `cancel` fires.
    pub async fn serve(self, addr: SocketAddr, cancel: CancellationToken) -> std::io::Result<()> {
        let app = self.router();
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "MCP HTTP listening");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
        .map_err(std::io::Error::other)?;
        info!("MCP HTTP server stopped");
        Ok(())
    }
}

/// Bind on `addr` and serve MCP over HTTP until `cancel` fires, with
/// no bearer token and stateless requests allowed. Equivalent to
/// [`McpHttpServer::new`] + [`McpHttpServer::serve`].
#[allow(clippy::too_many_arguments)] // mirrors the stdio entry point's wiring one-to-one
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
    McpHttpServer::new(registry, resources, rate_limiter, metrics, ctx, bus)
        .serve(addr, cancel)
        .await
}

/// Wiring shared between routes.
#[derive(Clone)]
struct AppState {
    dispatch: ProtocolDispatch,
    bus: EventBus,
    sessions: Arc<SessionStore>,
    require_session: bool,
}

// ---- sessions ----

struct Session {
    protocol_version: &'static str,
    last_seen: Instant,
}

/// Bounded, idle-expiring session table.
struct SessionStore {
    sessions: DashMap<String, Session>,
    idle: Duration,
    max: usize,
}

impl SessionStore {
    fn new(idle: Duration, max: usize) -> Self {
        Self {
            sessions: DashMap::new(),
            idle,
            max,
        }
    }

    /// Mint a fresh session id (128 random bits, hex-encoded).
    fn create(&self, protocol_version: &'static str) -> String {
        self.expire_idle();
        if self.sessions.len() >= self.max {
            self.evict_oldest();
        }
        let id = hex::encode(rand::random::<[u8; 16]>());
        self.sessions.insert(
            id.clone(),
            Session {
                protocol_version,
                last_seen: Instant::now(),
            },
        );
        id
    }

    /// Validate `id`, refreshing its idle clock. `None` when unknown
    /// or expired.
    fn touch(&self, id: &str) -> Option<&'static str> {
        let mut entry = self.sessions.get_mut(id)?;
        if entry.last_seen.elapsed() > self.idle {
            drop(entry);
            self.sessions.remove(id);
            return None;
        }
        entry.last_seen = Instant::now();
        Some(entry.protocol_version)
    }

    fn remove(&self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    fn expire_idle(&self) {
        let idle = self.idle;
        self.sessions.retain(|_, s| s.last_seen.elapsed() <= idle);
    }

    fn evict_oldest(&self) {
        let oldest = self
            .sessions
            .iter()
            .min_by_key(|e| e.value().last_seen)
            .map(|e| e.key().clone());
        if let Some(id) = oldest {
            self.sessions.remove(&id);
        }
    }
}

/// How the incoming request was bound to a session.
enum SessionScope {
    /// Header present and valid.
    Bound(String),
    /// No header; served statelessly.
    Stateless,
}

/// A request refused before dispatch: HTTP status + reason, rendered
/// as `{"error":...}`.
struct Refusal(StatusCode, String);

impl Refusal {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, message.into())
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({ "error": self.1 }))).into_response()
    }
}

/// Resolve the session header.
fn resolve_session(state: &AppState, headers: &HeaderMap) -> Result<SessionScope, Refusal> {
    match headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
        Some(id) => {
            if state.sessions.touch(id).is_some() {
                Ok(SessionScope::Bound(id.to_owned()))
            } else {
                Err(Refusal::new(
                    StatusCode::NOT_FOUND,
                    "unknown or expired Mcp-Session-Id",
                ))
            }
        }
        None => Ok(SessionScope::Stateless),
    }
}

/// `MCP-Protocol-Version` (2025-06-18) must name a revision we speak
/// when present.
fn check_protocol_version_header(headers: &HeaderMap) -> Result<(), Refusal> {
    match headers
        .get(PROTOCOL_VERSION_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) if !SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => Err(Refusal::new(
            StatusCode::BAD_REQUEST,
            format!("unsupported MCP-Protocol-Version `{v}`"),
        )),
        _ => Ok(()),
    }
}

/// `true` when the client prefers an SSE response to a POST: it
/// accepts `text/event-stream` and does not also accept JSON.
fn prefers_sse(headers: &HeaderMap) -> bool {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    accept.contains("text/event-stream")
        && !accept.contains("application/json")
        && !accept.contains("*/*")
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream") || a.contains("*/*"))
}

// ---- POST /mcp ----

async fn post_mcp(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(refusal) = check_protocol_version_header(&headers) {
        return refusal.into_response();
    }
    let scope = match resolve_session(&state, &headers) {
        Ok(s) => s,
        Err(refusal) => return refusal.into_response(),
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(error_response(
                    &Value::Null,
                    ERR_PARSE,
                    &format!("parse error: {e}"),
                )),
            )
                .into_response();
        }
    };
    let (messages, batch) = match payload {
        Value::Array(items) => (items, true),
        other => (vec![other], false),
    };

    let initializing = messages.iter().any(|m| m["method"] == "initialize");
    if matches!(scope, SessionScope::Stateless) && state.require_session && !initializing {
        return Refusal::new(StatusCode::BAD_REQUEST, "Mcp-Session-Id required").into_response();
    }

    let caller = match &scope {
        SessionScope::Bound(id) => id.clone(),
        SessionScope::Stateless => peer.ip().to_string(),
    };
    let mut responses = Vec::with_capacity(messages.len());
    for msg in &messages {
        if let Some(resp) = state
            .dispatch
            .handle_jsonrpc(ACTOR, Some(&caller), msg)
            .await
        {
            responses.push(resp);
        }
    }

    // A fresh session is minted once `initialize` has been answered,
    // and the negotiated version rides along on the session record.
    let new_session = if initializing && matches!(scope, SessionScope::Stateless) {
        let requested = messages
            .iter()
            .find(|m| m["method"] == "initialize")
            .and_then(|m| m["params"]["protocolVersion"].as_str());
        Some(state.sessions.create(negotiate_protocol_version(requested)))
    } else {
        None
    };

    let mut response = if responses.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else if prefers_sse(&headers) {
        let events = responses
            .into_iter()
            .map(|r| Ok::<_, Infallible>(Event::default().event("message").data(r.to_string())));
        Sse::new(stream::iter(events)).into_response()
    } else if batch {
        axum::Json(Value::Array(responses)).into_response()
    } else {
        axum::Json(responses.remove(0)).into_response()
    };
    if let Some(id) = new_session
        && let Ok(value) = HeaderValue::from_str(&id)
    {
        response
            .headers_mut()
            .insert(HeaderName::from_static(SESSION_HEADER), value);
    }
    response
}

// ---- GET /mcp, GET /mcp/events ----

async fn get_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(refusal) = check_protocol_version_header(&headers) {
        return refusal.into_response();
    }
    if !accepts_sse(&headers) {
        return Refusal::new(
            StatusCode::NOT_ACCEPTABLE,
            "GET /mcp requires `Accept: text/event-stream`",
        )
        .into_response();
    }
    match resolve_session(&state, &headers) {
        Ok(SessionScope::Stateless) if state.require_session => {
            return Refusal::new(StatusCode::BAD_REQUEST, "Mcp-Session-Id required")
                .into_response();
        }
        Ok(_) => {}
        Err(refusal) => return refusal.into_response(),
    }
    notification_stream(&state.bus).into_response()
}

async fn legacy_events(State(state): State<AppState>) -> Response {
    notification_stream(&state.bus).into_response()
}

fn notification_stream(
    bus: &EventBus,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    let rx = bus.subscribe();
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

// ---- DELETE /mcp ----

async fn delete_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return Refusal::new(StatusCode::BAD_REQUEST, "Mcp-Session-Id required").into_response();
    };
    if state.sessions.remove(id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Refusal::new(StatusCode::NOT_FOUND, "unknown or expired Mcp-Session-Id").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_store_is_bounded_and_expires_idle_entries() {
        let store = SessionStore::new(Duration::from_millis(30), 2);
        let a = store.create("2025-03-26");
        let b = store.create("2025-03-26");
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert_eq!(store.touch(&a), Some("2025-03-26"));
        // Third session evicts the least recently seen (b).
        let c = store.create("2025-06-18");
        assert!(store.touch(&b).is_none());
        assert_eq!(store.touch(&c), Some("2025-06-18"));
        assert!(store.remove(&c));
        assert!(!store.remove(&c));
        std::thread::sleep(Duration::from_millis(50));
        assert!(store.touch(&a).is_none(), "idle session must expire");
    }

    #[test]
    fn accept_header_selects_sse_only_when_json_is_not_acceptable() {
        let mut h = HeaderMap::new();
        assert!(!prefers_sse(&h));
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(prefers_sse(&h));
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        assert!(!prefers_sse(&h));
        assert!(accepts_sse(&h));
        h.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(!accepts_sse(&h));
    }
}
