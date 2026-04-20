//! MCP HTTP + SSE integration: spin the axum server, POST a
//! JSON-RPC request, and subscribe to the SSE stream.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use smiths_core::ai::AiRegistry;
use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use smiths_core::{Config, Event, EventBus, Metrics, RateLimitConfig, SipEvent};
use smiths_mcp::{ControlState, RateLimiter, ToolContext, builtin_resources, tools};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

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

async fn spawn_server() -> (std::net::SocketAddr, EventBus, CancellationToken) {
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
        let _ = smiths_mcp::mcp_http::serve_http(
            addr, registry, resources, rl, metrics, ctx, bus_clone, c2,
        )
        .await;
    });
    // Let axum bind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, bus, cancel)
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
