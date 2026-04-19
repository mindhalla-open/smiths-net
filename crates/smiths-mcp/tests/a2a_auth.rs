//! A2A bearer-auth integration: spin the real axum server, hit `/a2a`
//! with and without the token, confirm 401 vs 200.

use std::sync::Arc;
use std::time::Duration;

use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use smiths_core::{Config, EventBus, Metrics, RateLimitConfig};
use smiths_mcp::{ControlState, RateLimiter, ToolContext, builtin_resources, tools};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

async fn spawn_server(bearer: Option<String>) -> (std::net::SocketAddr, CancellationToken) {
    // Pick an ephemeral port, pass it straight to the server.
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

    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ =
            smiths_mcp::a2a::serve_http(addr, registry, resources, rl, metrics, bearer, ctx, c2)
                .await;
    });
    // Let axum bind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, cancel)
}

struct EmptyRegistry;

#[async_trait::async_trait]
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

#[async_trait::async_trait]
impl MediaFabric for NullMedia {
    async fn allocate(&self, _: std::net::IpAddr) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
        Err(MediaError::PortExhausted("null".into()))
    }
    async fn bridge(
        &self,
        _: EndpointId,
        _: std::net::SocketAddr,
        _: EndpointId,
        _: std::net::SocketAddr,
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

async fn post_rpc(
    addr: std::net::SocketAddr,
    bearer: Option<&str>,
    body: &str,
) -> (reqwest::StatusCode, String) {
    let client = reqwest::Client::new();
    let mut req = client
        .post(format!("http://{addr}/a2a"))
        .header("content-type", "application/json")
        .body(body.to_owned());
    if let Some(tok) = bearer {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    (status, text)
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_disabled_allows_unauthenticated_calls() {
    let (addr, cancel) = spawn_server(None).await;
    let (status, body) = post_rpc(addr, None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).await;
    assert_eq!(status, 200);
    assert!(body.contains("\"result\""), "body: {body}");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_bearer_returns_401() {
    let (addr, cancel) = spawn_server(Some("s3cret".into())).await;
    let (status, _body) = post_rpc(addr, None, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).await;
    assert_eq!(status, 401);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_bearer_returns_401() {
    let (addr, cancel) = spawn_server(Some("s3cret".into())).await;
    let (status, _body) = post_rpc(
        addr,
        Some("wrong"),
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    )
    .await;
    assert_eq!(status, 401);
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn correct_bearer_is_admitted() {
    let (addr, cancel) = spawn_server(Some("s3cret".into())).await;
    let (status, body) = post_rpc(
        addr,
        Some("s3cret"),
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("\"result\""), "body: {body}");
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoint_is_public_even_with_auth_on() {
    let (addr, cancel) = spawn_server(Some("s3cret".into())).await;
    let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    cancel.cancel();
}
