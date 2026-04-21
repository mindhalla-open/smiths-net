//! Slice 4.4 / P20 — generic webhook adapter. Prove that a plain
//! HTTP client can invoke a tool via `POST /hook/<tool>` with a
//! bare JSON body (no JSON-RPC envelope) and get back either
//! `{"result": ...}` on success or `{"error": "..."}` with the
//! right HTTP status on failure.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use smiths_core::{Config, EventBus, Metrics, RateLimitConfig};
use smiths_mcp::{
    ControlState, ProtocolDispatch, RateLimiter, ToolContext, builtin_resources, tools,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct EmptyRegistry;
#[async_trait]
impl smiths_core::AiRegistry for EmptyRegistry {
    fn get(&self, _: &str) -> Option<Arc<dyn smiths_core::AiProvider>> {
        None
    }
    fn snapshot(&self) -> Vec<Arc<dyn smiths_core::AiProvider>> {
        Vec::new()
    }
    async fn shutdown_all(&self) {}
}

struct NullMedia;
#[async_trait]
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

async fn spawn_webhook(bearer: Option<String>) -> (std::net::SocketAddr, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (state, _task) = ControlState::spawn(&bus, cancel.clone());
    let ai_registry: Arc<dyn smiths_core::AiRegistry> = Arc::new(EmptyRegistry);
    let config = Arc::new(Config::default());
    let media: Arc<dyn MediaFabric> = Arc::new(NullMedia);
    let ctx = ToolContext::new(state, ai_registry, config, media);
    let registry = Arc::new(tools::builtin_registry());
    let resources = Arc::new(builtin_resources());
    let rl = Arc::new(RateLimiter::new(&RateLimitConfig::default()));
    let metrics = Metrics::noop();

    let dispatch = ProtocolDispatch {
        registry,
        rate_limiter: rl,
        metrics,
        ctx,
    };
    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ = smiths_mcp::webhook::serve_http(addr, dispatch, resources, bearer, c2).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, cancel)
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_invokes_health_tool_with_bare_json() {
    let (addr, cancel) = spawn_webhook(None).await;
    let client = reqwest::Client::new();

    // `health` takes no args — empty `{}` body is correct.
    let resp = client
        .post(format!("http://{addr}/hook/health"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let payload = &body["result"];
    assert_eq!(payload["status"], "ok");
    assert!(payload["uptime_secs"].is_number());

    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_unknown_tool_returns_404() {
    let (addr, cancel) = spawn_webhook(None).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/hook/no-such-tool"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("no-such-tool"));
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_invalid_arguments_returns_400() {
    let (addr, cancel) = spawn_webhook(None).await;
    let client = reqwest::Client::new();
    // `get_call_status` requires `call_id`; omitting it is
    // `InvalidArguments` → 400.
    let resp = client
        .post(format!("http://{addr}/hook/get_call_status"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_agent_card_advertises_tools_and_adapters() {
    let (addr, cancel) = spawn_webhook(None).await;
    let card: serde_json::Value = reqwest::get(format!("http://{addr}/.well-known/agent.json"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tools = card["capabilities"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["name"] == "health"));
    let adapters = card["adapters"].as_array().unwrap();
    let labels: Vec<_> = adapters
        .iter()
        .map(|a| a["label"].as_str().unwrap())
        .collect();
    assert!(labels.contains(&"mcp-stdio"));
    assert!(labels.contains(&"a2a-http"));
    assert!(labels.contains(&"webhook-http"));
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_bearer_guard_blocks_unauthenticated() {
    let (addr, cancel) = spawn_webhook(Some("s3cret".into())).await;
    let client = reqwest::Client::new();
    let unauth = client
        .post(format!("http://{addr}/hook/health"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401);

    let authed = client
        .post(format!("http://{addr}/hook/health"))
        .bearer_auth("s3cret")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), 200);
    cancel.cancel();
}
