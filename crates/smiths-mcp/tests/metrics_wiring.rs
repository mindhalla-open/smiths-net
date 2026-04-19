//! Smoke: an admitted tool call bumps `tool_invocations_total{tool,outcome}`
//! and observes a sample into `tool_duration_seconds{tool}`. We run the
//! dispatch helper directly — no network round-trip needed.

use std::sync::Arc;

use prometheus_client::registry::Registry;
use serde_json::{Value, json};
use smiths_core::{Config, EventBus, Metrics, RateLimitConfig};
use smiths_mcp::{
    ControlState, RateLimiter, ToolContext, dispatch::invoke_audited, tools::builtin_registry,
};
use tokio_util::sync::CancellationToken;

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

#[tokio::test(flavor = "multi_thread")]
async fn admitted_health_call_increments_counter_and_observes_duration() {
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (state, _task) = ControlState::spawn(&bus, cancel.clone());
    let ctx = ToolContext::new(
        state,
        Arc::new(EmptyRegistry),
        Arc::new(Config::default()),
        Arc::new(smiths_media::UdpMediaFabric::new()),
    );

    let mut registry = Registry::default();
    let metrics = Metrics::register(&mut registry);
    let tools = builtin_registry();
    let rl = Arc::new(RateLimiter::new(&RateLimitConfig::default()));

    let out = invoke_audited(&tools, &rl, &metrics, &ctx, "test", "health", json!({}))
        .await
        .unwrap();
    assert_eq!(out["status"], "ok");

    let mut out = String::new();
    prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
    assert!(
        out.contains(r#"tool_invocations_total{tool="health",outcome="ok"} 1"#),
        "counter missing or wrong:\n{out}"
    );
    assert!(
        out.contains(r#"tool_duration_seconds_count{tool="health"} 1"#),
        "histogram count missing:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limited_call_records_forbidden_without_latency() {
    let bus = EventBus::new(16);
    let cancel = CancellationToken::new();
    let (state, _task) = ControlState::spawn(&bus, cancel.clone());
    let ctx = ToolContext::new(
        state,
        Arc::new(EmptyRegistry),
        Arc::new(Config::default()),
        Arc::new(smiths_media::UdpMediaFabric::new()),
    );

    let mut registry = Registry::default();
    let metrics = Metrics::register(&mut registry);
    let tools = builtin_registry();
    let rl = Arc::new(RateLimiter::new(&RateLimitConfig {
        per_sec: 1,
        burst: 1,
    }));

    // First call admitted.
    let _ = invoke_audited(&tools, &rl, &metrics, &ctx, "t", "health", json!({})).await;
    // Second call rate-limited.
    let err = invoke_audited(&tools, &rl, &metrics, &ctx, "t", "health", json!({}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("rate limited"),
        "expected forbidden/rate-limited, got: {err}"
    );

    let mut out = String::new();
    prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
    // Both outcomes present: one `ok`, one `forbidden`.
    assert!(out.contains(r#"tool_invocations_total{tool="health",outcome="ok"} 1"#));
    assert!(out.contains(r#"tool_invocations_total{tool="health",outcome="forbidden"} 1"#));
    // Only the admitted call contributes to latency.
    assert!(
        out.contains(r#"tool_duration_seconds_count{tool="health"} 1"#),
        "latency sample count should be 1:\n{out}"
    );
    let _: Value = json!(null); // keep serde_json::Value in scope for the import
}
