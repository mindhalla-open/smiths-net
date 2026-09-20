//! MCP Streamable HTTP integration: spin the axum server, drive the
//! session lifecycle over `POST` / `GET` / `DELETE /mcp`, and read
//! the SSE streams. The legacy stateless `POST /mcp` and
//! `GET /mcp/events` paths stay covered too.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use smiths_core::ai::AiRegistry;
use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use smiths_core::{Config, Event, EventBus, Metrics, RateLimitConfig, SipEvent};
use smiths_mcp::{ControlState, McpHttpServer, RateLimiter, ToolContext, builtin_resources, tools};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const SESSION_HEADER: &str = "mcp-session-id";

struct EmptyRegistry;

#[async_trait::async_trait]
impl AiRegistry for EmptyRegistry {
    fn get(&self, _: &str) -> Option<Arc<dyn smiths_core::AiProvider>> {
        None
    }
    fn snapshot(&self) -> Vec<Arc<dyn smiths_core::AiProvider>> {
        Vec::new()
    }
    async fn shutdown_all(&self) {}
}

struct NullMedia;

#[async_trait::async_trait]
impl MediaFabric for NullMedia {
    async fn allocate(&self, _: std::net::IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
        Err(MediaError::PortExhausted("null".into()))
    }
    async fn bridge(
        &self,
        _: smiths_core::BridgeLeg,
        _: smiths_core::BridgeLeg,
    ) -> Result<BridgeId, MediaError> {
        Err(MediaError::PortExhausted("null".into()))
    }
    async fn release_bridge(&self, _: BridgeId) {}
    async fn release_endpoint(&self, _: EndpointId) {}
    async fn send_packet(
        &self,
        _: EndpointId,
        _: std::net::SocketAddr,
        _: &[u8],
    ) -> Result<(), MediaError> {
        Err(MediaError::PortExhausted("null".into()))
    }
}

/// Server-side knobs a test can flip.
#[derive(Default)]
struct Opts {
    bearer: Option<String>,
    require_session: bool,
}

async fn spawn_server() -> (std::net::SocketAddr, EventBus, CancellationToken) {
    spawn_server_with(Opts::default()).await
}

async fn spawn_server_with(opts: Opts) -> (std::net::SocketAddr, EventBus, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let bus = EventBus::new(32);
    let cancel = CancellationToken::new();
    let (state, _task) = ControlState::spawn(&bus, cancel.clone());
    let ai_registry: Arc<dyn AiRegistry> = Arc::new(EmptyRegistry);
    let media: Arc<dyn MediaFabric> = Arc::new(NullMedia);
    let ctx = ToolContext::new(state, ai_registry, Arc::new(Config::default()), media);
    let registry = Arc::new(tools::builtin_registry());
    let resources = Arc::new(builtin_resources());
    let rl = Arc::new(RateLimiter::new(&RateLimitConfig::default()));
    let metrics = Metrics::noop();
    let bus_clone = bus.clone();
    let c2 = cancel.clone();

    tokio::spawn(async move {
        let _ = McpHttpServer::new(registry, resources, rl, metrics, ctx, bus_clone)
            .with_http_bearer(opts.bearer)
            .with_require_session(opts.require_session)
            .serve(addr, c2)
            .await;
    });
    // Let axum bind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, bus, cancel)
}

fn rpc(id: u64, method: &str, params: &Value) -> Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// POST `initialize` and return `(session id, negotiated version)`.
async fn initialize(
    client: &reqwest::Client,
    addr: std::net::SocketAddr,
    version: &str,
) -> (String, String) {
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&rpc(
            1,
            "initialize",
            &serde_json::json!({"protocolVersion": version}),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let sid = resp
        .headers()
        .get(SESSION_HEADER)
        .expect("initialize issues Mcp-Session-Id")
        .to_str()
        .unwrap()
        .to_owned();
    let v: Value = resp.json().await.unwrap();
    let negotiated = v["result"]["protocolVersion"].as_str().unwrap().to_owned();
    (sid, negotiated)
}

/// Read an SSE body until `needle` shows up or the deadline passes.
async fn read_sse_until(resp: reqwest::Response, needle: &str) -> String {
    use futures::StreamExt as _;
    let mut bytes = resp.bytes_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut accumulated = String::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), bytes.next()).await {
            Ok(Some(Ok(chunk))) => {
                accumulated.push_str(&String::from_utf8_lossy(&chunk));
                if accumulated.contains(needle) {
                    break;
                }
            }
            Ok(Some(Err(_)) | None) => break,
            Err(_) => {}
        }
    }
    accumulated
}

#[tokio::test(flavor = "multi_thread")]
async fn post_tools_call_health_round_trip() {
    let (addr, _bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "health", "arguments": {} }
    });
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["isError"], false);
    assert_eq!(v["result"]["structuredContent"]["status"], "ok");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn post_initialize_advertises_resources_and_tools() {
    let (addr, _bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "initialize"
    });
    let v: Value = client
        .post(format!("http://{addr}/mcp"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(v["result"]["capabilities"]["tools"].is_object());
    assert!(v["result"]["capabilities"]["resources"].is_object());
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn sse_stream_receives_dialog_created_notification() {
    let (addr, bus, cancel) = spawn_server().await;

    // Start reading the SSE stream first so we don't race the event.
    let url = format!("http://{addr}/mcp/events");
    let stream_handle = tokio::spawn(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        assert!(
            ctype.starts_with("text/event-stream"),
            "got content-type: {ctype}"
        );
        let mut bytes = resp.bytes_stream();
        // Read chunks until we see a DialogCreated frame or timeout.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut accumulated = String::new();
        while tokio::time::Instant::now() < deadline {
            use futures::StreamExt as _;
            if let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(Duration::from_millis(500), bytes.next()).await
            {
                accumulated.push_str(&String::from_utf8_lossy(&chunk));
                if accumulated.contains("call/created") {
                    return accumulated;
                }
            }
        }
        accumulated
    });

    // Give the client a moment to connect + SSE keep-alive to arm.
    tokio::time::sleep(Duration::from_millis(200)).await;
    bus.publish(Event::Sip(SipEvent::DialogCreated {
        call_id: "sse-test@example".into(),
        media_endpoint: None,
        remote_rtp: None,
    }))
    .unwrap();

    let got = stream_handle.await.unwrap();
    assert!(
        got.contains("call/created") && got.contains("sse-test@example"),
        "SSE stream missing expected frame:\n{got}"
    );
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_issues_session_and_negotiates_protocol_version() {
    let (addr, _bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let (sid, negotiated) = initialize(&client, addr, "2025-06-18").await;
    assert_eq!(sid.len(), 32);
    assert_eq!(negotiated, "2025-06-18");
    let (second_sid, negotiated) = initialize(&client, addr, "2024-11-05").await;
    assert_ne!(sid, second_sid);
    assert_eq!(negotiated, "2024-11-05");
    let (_, negotiated) = initialize(&client, addr, "1999-01-01").await;
    assert_eq!(negotiated, "2025-03-26");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn session_lifecycle_post_delete_and_404_after_delete() {
    let (addr, _bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let (sid, _) = initialize(&client, addr, "2025-03-26").await;

    // Notifications-only body → 202 with no content.
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .json(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    assert!(resp.bytes().await.unwrap().is_empty());

    // A request on the session is answered as JSON.
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .header("mcp-protocol-version", "2025-03-26")
        .json(&rpc(
            2,
            "tools/call",
            &serde_json::json!({"name": "health", "arguments": {}}),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["structuredContent"]["status"], "ok");

    // DELETE ends the session; the id stops working.
    let resp = client
        .delete(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .json(&rpc(7, "ping", &serde_json::json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .delete(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .delete(format!("http://{addr}/mcp"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn batches_unknown_sessions_and_bad_protocol_headers() {
    let (addr, _bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let (sid, _) = initialize(&client, addr, "2025-03-26").await;

    // Batches come back as arrays.
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .json(&serde_json::json!([
            rpc(3, "ping", &serde_json::json!({})),
            rpc(4, "tools/list", &serde_json::json!({}))
        ]))
        .send()
        .await
        .unwrap();
    let v: Value = resp.json().await.unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[1]["id"], 4);

    // Unknown session → 404; unsupported protocol header → 400.
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, "deadbeef")
        .json(&rpc(5, "ping", &serde_json::json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .header("mcp-protocol-version", "1999-01-01")
        .json(&rpc(6, "ping", &serde_json::json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn require_session_rejects_headerless_requests_but_not_initialize() {
    let (addr, _bus, cancel) = spawn_server_with(Opts {
        require_session: true,
        ..Opts::default()
    })
    .await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .json(&rpc(1, "ping", &serde_json::json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let (sid, _) = initialize(&client, addr, "2025-03-26").await;
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .json(&rpc(2, "ping", &serde_json::json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // GET without a session is refused too.
    let resp = client
        .get(format!("http://{addr}/mcp"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn post_with_sse_only_accept_returns_event_stream_response() {
    let (addr, _bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/mcp"))
        .header("accept", "text/event-stream")
        .json(&rpc(
            9,
            "tools/call",
            &serde_json::json!({"name": "health", "arguments": {}}),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(ctype.starts_with("text/event-stream"), "got {ctype}");
    let body = read_sse_until(resp, "structuredContent").await;
    assert!(body.contains("event: message"), "{body}");
    assert!(body.contains("\"id\":9"), "{body}");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn get_mcp_streams_notifications_for_a_session() {
    let (addr, bus, cancel) = spawn_server().await;
    let client = reqwest::Client::new();
    let (sid, _) = initialize(&client, addr, "2025-03-26").await;

    // Without the SSE accept header the stream is refused.
    let resp = client
        .get(format!("http://{addr}/mcp"))
        .header(SESSION_HEADER, &sid)
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 406);

    let url = format!("http://{addr}/mcp");
    let sid_for_task = sid.clone();
    let stream_handle = tokio::spawn(async move {
        let resp = reqwest::Client::new()
            .get(&url)
            .header(SESSION_HEADER, sid_for_task)
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        read_sse_until(resp, "call/terminated").await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    bus.publish(Event::Sip(SipEvent::DialogTerminated {
        call_id: "get-mcp@example".into(),
    }))
    .unwrap();
    let got = stream_handle.await.unwrap();
    assert!(
        got.contains("call/terminated") && got.contains("get-mcp@example"),
        "SSE stream missing expected frame:\n{got}"
    );
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_gates_every_mcp_route() {
    let (addr, _bus, cancel) = spawn_server_with(Opts {
        bearer: Some("s3cret".into()),
        ..Opts::default()
    })
    .await;
    let client = reqwest::Client::new();
    let body = rpc(1, "ping", &serde_json::json!({}));
    for (token, expected) in [(None, 401), (Some("wrong"), 401), (Some("s3cret"), 200)] {
        let mut req = client.post(format!("http://{addr}/mcp")).json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        assert_eq!(
            req.send().await.unwrap().status(),
            expected,
            "token {token:?}"
        );
    }
    let resp = client
        .get(format!("http://{addr}/mcp/events"))
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .delete(format!("http://{addr}/mcp"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_body_is_a_parse_error() {
    let (addr, _bus, cancel) = spawn_server().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], -32700);
    cancel.cancel();
}
