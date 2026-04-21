//! Slice 3.4 acceptance: end-to-end pipeline proving that a
//! transcribed call can be embedded, upserted into the vector
//! store, and retrieved via `search_calls_semantic`.
//!
//! We fake the ASR + embed steps with deterministic stubs so CI
//! doesn't need a whisper.cpp binary or an LLM API key on disk —
//! the dispatcher + tool contracts are what's under test. The
//! real pipeline (ASR → embed → upsert → search) runs the same
//! wire shapes with real models substituted in.

use std::sync::Arc;

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{Value, json};
use smiths_core::{
    AiProvider, AiRegistry, CapabilityDescriptor, Config, DEFAULT_PRIORITY, EventBus,
    MemoryVectorStore, ProviderError, VectorRecord, VectorStore,
};
use smiths_mcp::tool::Tool;
use smiths_mcp::{ToolContext, tools::SearchCallsSemanticTool};

/// Embed provider that returns a canned 3-dim vector per input.
/// "billing" → (1, 0, 0); "hardware" → (0, 1, 0); "other" → (0, 0, 1).
/// That's enough to make cosine similarity deterministic without
/// pulling a real model into the test binary.
struct CannedEmbed {
    name: String,
    cap: CapabilityDescriptor,
}

impl CannedEmbed {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            name: "embed-canned".into(),
            cap: CapabilityDescriptor {
                capability: "ai.embed".into(),
                plugin: "embed-canned".into(),
                model_id: "canned-3d".into(),
                abi: "1.0".into(),
                description: "3-dim canned embeddings for tests".into(),
                latency_ms: None,
                concurrency: None,
                priority: DEFAULT_PRIORITY,
                extra: BTreeMap::default(),
            },
        })
    }

    fn pick(text: &str) -> [f32; 3] {
        let lower = text.to_lowercase();
        if lower.contains("billing") || lower.contains("charge") || lower.contains("invoice") {
            [1.0, 0.0, 0.0]
        } else if lower.contains("hardware") || lower.contains("device") || lower.contains("phone")
        {
            [0.0, 1.0, 0.0]
        } else {
            [0.0, 0.0, 1.0]
        }
    }
}

#[async_trait]
impl AiProvider for CannedEmbed {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &'static str {
        "0.0.0"
    }
    fn description(&self) -> &str {
        &self.cap.description
    }
    fn abi(&self) -> &str {
        &self.cap.abi
    }
    fn capabilities(&self) -> &[CapabilityDescriptor] {
        std::slice::from_ref(&self.cap)
    }
    async fn invoke(&self, method: &str, params: Value) -> Result<Value, ProviderError> {
        if method != "embed" {
            return Err(ProviderError(format!("unknown method `{method}`")));
        }
        let inputs = params
            .get("inputs")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError("missing inputs".into()))?;
        let vectors: Vec<Vec<f32>> = inputs
            .iter()
            .map(|v| Self::pick(v.as_str().unwrap_or("")).to_vec())
            .collect();
        Ok(json!({ "vectors": vectors }))
    }
}

struct OneProviderRegistry(Vec<Arc<dyn AiProvider>>);

#[async_trait]
impl AiRegistry for OneProviderRegistry {
    fn get(&self, name: &str) -> Option<Arc<dyn AiProvider>> {
        self.0.iter().find(|p| p.name() == name).cloned()
    }
    fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
        self.0.clone()
    }
    async fn shutdown_all(&self) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn record_transcribe_embed_search_round_trip() {
    // --- Build a tool context with memory vector + canned embed. ---
    let bus = EventBus::new(8);
    let cancel = tokio_util::sync::CancellationToken::new();
    let (state, _task) = smiths_mcp::control::ControlState::spawn(&bus, cancel.clone());

    let embed: Arc<dyn AiProvider> = CannedEmbed::new();
    let registry: Arc<dyn AiRegistry> = Arc::new(OneProviderRegistry(vec![embed.clone()]));
    let media: Arc<dyn smiths_core::MediaFabric> = Arc::new(NullMedia);
    let vector: Arc<dyn VectorStore> = Arc::new(MemoryVectorStore::new());

    let ctx = ToolContext::new(state, registry, Arc::new(Config::default()), media)
        .with_vector(Arc::clone(&vector));

    // --- Simulate three calls' worth of transcripts getting ---
    // --- embedded + upserted with metadata. In production this is ---
    // --- what the agent pipeline does after `transcribe_call`.    ---
    let samples = [
        (
            "call-a",
            "Customer complained about a duplicate billing charge.",
        ),
        (
            "call-b",
            "Caller asked for help configuring their office phone.",
        ),
        ("call-c", "Weather chat with the receptionist — no issue."),
    ];
    for (call_id, transcript) in samples {
        let vec = CannedEmbed::pick(transcript).to_vec();
        vector
            .upsert(&VectorRecord {
                id: call_id.into(),
                vector: vec,
                metadata: json!({ "call_id": call_id, "transcript": transcript }),
            })
            .unwrap();
    }
    assert_eq!(vector.len().unwrap(), 3);

    // --- Run the MCP tool. The search query "invoice dispute"  ---
    // --- maps to the billing vector; the top hit must be call-a.---
    let result = SearchCallsSemanticTool
        .call(json!({"query": "invoice dispute", "k": 3}), &ctx)
        .await
        .expect("search ok");

    assert_eq!(result["count"], 3);
    let hits = result["hits"].as_array().expect("hits array");
    assert_eq!(hits[0]["id"], "call-a", "billing call must rank first");
    // Metadata rides back.
    assert_eq!(
        hits[0]["metadata"]["transcript"],
        "Customer complained about a duplicate billing charge."
    );
    // Second hit is the "other" bucket — cosine 0 against the query
    // direction, so it's fine either way, but we expect "call-c" OR
    // "call-b" — NOT a duplicate of call-a.
    assert_ne!(hits[1]["id"], "call-a");
}

struct NullMedia;

#[async_trait]
impl smiths_core::MediaFabric for NullMedia {
    async fn allocate(
        &self,
        _: std::net::IpAddr,
    ) -> Result<Arc<dyn smiths_core::MediaEndpoint>, smiths_core::MediaError> {
        Err(smiths_core::MediaError::PortExhausted("null".into()))
    }
    async fn bridge(
        &self,
        _: smiths_core::BridgeLeg,
        _: smiths_core::BridgeLeg,
    ) -> Result<smiths_core::BridgeId, smiths_core::MediaError> {
        Err(smiths_core::MediaError::PortExhausted("null".into()))
    }
    async fn release_bridge(&self, _: smiths_core::BridgeId) {}
    async fn release_endpoint(&self, _: smiths_core::EndpointId) {}
    async fn send_packet(
        &self,
        _: smiths_core::EndpointId,
        _: std::net::SocketAddr,
        _: &[u8],
    ) -> Result<(), smiths_core::MediaError> {
        Err(smiths_core::MediaError::PortExhausted("null".into()))
    }
}
