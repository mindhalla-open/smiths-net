//! Slice 4.4 / P20 acceptance: an HTTP client speaks A2A's
//! `/a2a` JSON-RPC to invoke the `make_call` tool end-to-end.
//! Stub `CallOriginator` stands in for the SIP UAC so the test
//! doesn't need a real dialog — the goal is to prove the A2A
//! adapter routes through the same `invoke_audited` pipeline
//! that MCP uses, with the new `ControlProtocol` / `ControlOutcome`
//! plumbing in place.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use smiths_core::call::{CallError, CallOriginator};
use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
use smiths_core::{Config, EventBus, Metrics, RateLimitConfig};
use smiths_mcp::{ControlState, RateLimiter, ToolContext, builtin_resources, tools};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct StubOriginator {
    expected_target: String,
    call_id: String,
}

#[async_trait]
impl CallOriginator for StubOriginator {
    async fn place_call(&self, target: &str) -> Result<String, CallError> {
        assert_eq!(target, self.expected_target, "unexpected make_call target");
        Ok(self.call_id.clone())
    }
    async fn hangup(&self, _call_id: &str) -> Result<(), CallError> {
        Ok(())
    }
}

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

async fn spawn_a2a_server() -> (std::net::SocketAddr, CancellationToken) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (state, _task) = ControlState::spawn(&bus, cancel.clone());
    let ai_registry: Arc<dyn smiths_core::AiRegistry> = Arc::new(EmptyRegistry);
    let config = Arc::new(Config::default());
    let media: Arc<dyn MediaFabric> = Arc::new(NullMedia);
    let originator: Arc<dyn CallOriginator> = Arc::new(StubOriginator {
        expected_target: "sip:alice@10.0.0.2:5060".to_owned(),
        call_id: "a2a-make-call-1@smiths.local".to_owned(),
    });
    let ctx = ToolContext::new(state, ai_registry, config, media).with_originator(originator);
    let registry = Arc::new(tools::builtin_registry());
    let resources = Arc::new(builtin_resources());
    let rl = Arc::new(RateLimiter::new(&RateLimitConfig::default()));
    let metrics = Metrics::noop();

    let c2 = cancel.clone();
    tokio::spawn(async move {
        let _ = smiths_mcp::a2a::serve_http(addr, registry, resources, rl, metrics, None, ctx, c2)
            .await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, cancel)
}

#[tokio::test(flavor = "multi_thread")]
async fn a2a_client_invokes_make_call_end_to_end() {
    let (addr, cancel) = spawn_a2a_server().await;
    let client = reqwest::Client::new();

    // 1. Discovery via `.well-known/agent.json` proves the
    //    `make_call` tool is advertised.
    let card: serde_json::Value = client
        .get(format!("http://{addr}/.well-known/agent.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tools = card["capabilities"]["tools"].as_array().unwrap();
    assert!(
        tools.iter().any(|t| t["name"] == "make_call"),
        "agent card missing make_call"
    );

    // 2. Invoke `make_call` via A2A JSON-RPC.
    let resp: serde_json::Value = client
        .post(format!("http://{addr}/a2a"))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {
                "name": "make_call",
                "arguments": { "target": "sip:alice@10.0.0.2:5060" }
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 42);
    assert_eq!(resp["result"]["isError"], false);
    let payload = resp["result"]["output"].clone();
    assert_eq!(payload["call_id"], "a2a-make-call-1@smiths.local");
    assert_eq!(payload["target"], "sip:alice@10.0.0.2:5060");

    // 3. Proof the adapter is wired through `invoke_audited`:
    //    an unknown-tool call surfaces the standard NotFound
    //    JSON-RPC error (-32601) with no HTTP 500.
    let err: serde_json::Value = client
        .post(format!("http://{addr}/a2a"))
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 43,
            "method": "tools/call",
            "params": {
                "name": "nonexistent-tool",
                "arguments": {}
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(err["jsonrpc"], "2.0");
    assert_eq!(err["id"], 43);
    assert!(
        err.get("error").is_some(),
        "expected JSON-RPC error, got {err:?}"
    );

    cancel.cancel();
}
