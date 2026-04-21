//! AI-plugin capability descriptors, control-schema validation, and
//! provider/registry trait seams.
//!
//! These types and traits are the contract between the MCP/A2A control
//! plane and whichever crate actually hosts plugins (today:
//! `smiths-plugin` over sidecars; tomorrow: also WASM and script
//! tiers). Keeping them here means consumers — `smiths-mcp` — never
//! link the plugin crate. Plugin-tier concerns stay on one side of the
//! seam; wire/UX concerns stay on the other. Wiring happens in
//! `smiths-cli`.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

// ---------------------------------------------------------------------
// Capability descriptor
// ---------------------------------------------------------------------

/// One capability exposed by a plugin. Matches
/// `05-ai-plugin-protocol.md §Common fields` plus an opaque tail of
/// capability-specific fields (voices, languages, controls, ...).
///
/// The engine validates the common envelope via [`Self::validate`];
/// capability-specific validation (e.g. which controls are allowed for
/// `ai.tts`) runs at invocation time against the `controls` entry in
/// [`Self::extra`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityDescriptor {
    /// Capability string, e.g. `"ai.tts"`.
    pub capability: String,
    /// Plugin name (must match the manifest).
    pub plugin: String,
    /// Underlying model identifier, free-form.
    #[serde(default)]
    pub model_id: String,
    /// Descriptor schema version. `"1.0"` today.
    #[serde(default = "default_abi")]
    pub abi: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Advisory p50/p95 latencies.
    #[serde(default)]
    pub latency_ms: Option<LatencyHint>,
    /// Concurrency limits the plugin advertises.
    #[serde(default)]
    pub concurrency: Option<ConcurrencyHint>,
    /// Dispatch priority (slice 3.1). **Lower wins** — 0 is the
    /// strongest preference, 100 the weakest. Providers that omit
    /// the field default to [`DEFAULT_PRIORITY`]. The dispatcher
    /// sorts candidates by this value; ties broken by advertised
    /// p50 latency, then lexical plugin name for determinism.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// Catch-all for capability-specific fields (`voices`, `languages`,
    /// `controls`, etc.). Passed straight through to agents in
    /// `list_ai_providers` / `describe_provider`.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Latency hint block.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LatencyHint {
    /// p50 latency in milliseconds.
    #[serde(default)]
    pub p50: Option<u64>,
    /// p95 latency in milliseconds.
    #[serde(default)]
    pub p95: Option<u64>,
}

/// Concurrency hint block.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConcurrencyHint {
    /// Concurrent invocations the plugin can serve before the engine
    /// should queue.
    #[serde(default)]
    pub max_in_flight: Option<u32>,
}

fn default_abi() -> String {
    "1.0".to_string()
}

/// Dispatcher default when a provider doesn't set `priority`.
/// Middle of the 0–100 range so operators have head- and tail-room
/// in both directions.
pub const DEFAULT_PRIORITY: u8 = 50;

const fn default_priority() -> u8 {
    DEFAULT_PRIORITY
}

/// Parse the JSON body a plugin returned from `describe_capabilities`
/// (sidecar handshake) or the `describe()` export (WASM tier). Accepts
/// either a single descriptor object or an array of them. Rejects an
/// empty list and runs [`CapabilityDescriptor::validate`] on each entry.
///
/// Lives here (not in `smiths-plugin`) so every tier can share the same
/// parse/validate pipeline without pulling in a plugin-host dependency.
pub fn parse_descriptors(raw: Value) -> Result<Vec<CapabilityDescriptor>, String> {
    let list: Vec<CapabilityDescriptor> = if raw.is_array() {
        serde_json::from_value(raw).map_err(|e| format!("descriptor array parse: {e}"))?
    } else {
        let single: CapabilityDescriptor =
            serde_json::from_value(raw).map_err(|e| format!("descriptor parse: {e}"))?;
        vec![single]
    };
    if list.is_empty() {
        return Err("plugin returned no capabilities".into());
    }
    for d in &list {
        d.validate()?;
    }
    Ok(list)
}

impl CapabilityDescriptor {
    /// Validate common-envelope invariants. Returns a descriptive
    /// error when the plugin is confused.
    ///
    /// Capabilities live in one of three namespaces today:
    ///
    /// - `ai.*` — AI providers (LLM, TTS, ASR, embed). The original
    ///   plugin tier.
    /// - `media.*` — streaming-RTP consumers. Added in slice 2.5
    ///   so a sidecar plugin can declare itself as something that
    ///   receives per-packet RTP from the engine (e.g. the deferred
    ///   `dtmf-inband` Python sidecar). See [`MEDIA_STREAMING_RTP`].
    /// - `storage.*` — pluggable backends for the storage traits
    ///   (slice 3.4). `storage.vector` backs
    ///   `search_calls_semantic`; `storage.recording` backs audio
    ///   retention. See [`STORAGE_VECTOR`] and
    ///   [`STORAGE_RECORDING`].
    pub fn validate(&self) -> Result<(), String> {
        if self.capability.is_empty() {
            return Err("capability is empty".into());
        }
        if !self.capability.starts_with("ai.")
            && !self.capability.starts_with("media.")
            && !self.capability.starts_with("storage.")
        {
            return Err(format!(
                "capability `{}` is outside the `ai.*` / `media.*` / `storage.*` namespaces",
                self.capability
            ));
        }
        if self.plugin.is_empty() {
            return Err("plugin name is empty".into());
        }
        if !self.abi.starts_with("1.") {
            return Err(format!("descriptor abi `{}` is not 1.x", self.abi));
        }
        Ok(())
    }
}

/// Capability token declaring a plugin that consumes streaming RTP
/// from the engine's media fabric (slice 2.5). The engine wires the
/// bridge's plaintext RTP feed to the plugin's `on_rtp_frame` host
/// function; the plugin decides what to do with it (DTMF detect,
/// recording, RAG ingestion).
///
/// A plugin with this capability in its manifest opts into the
/// higher-cost hot-path call cadence — ~50 calls/sec/leg at the
/// standard 20 ms packetization. Reference consumers:
///
/// - **`dtmf-inband`** — Python sidecar wrapping scipy / the
///   engine's own Goertzel detector. Deferred to a follow-on;
///   v0.37.0 ships the engine-side detector instead.
/// - **Recording sidecars** — write each frame to a rolling file
///   or object store (P23 `storage.recording` follow-on).
pub const MEDIA_STREAMING_RTP: &str = "media.streaming_rtp";

/// Capability token declaring a plugin that serves an embedding-
/// indexed vector store (slice 3.4). The MCP `search_calls_semantic`
/// tool routes through this seam to back its top-k queries. The
/// reference sidecar is `store-qdrant` (Qdrant HTTP API wrapper).
pub const STORAGE_VECTOR: &str = "storage.vector";

/// Capability token declaring a plugin that serves per-call audio
/// retention (slice 3.4). The filesystem default ships in-tree;
/// operators pointing `[storage.recording] backend = "sidecar"` at
/// an S3-compatible sidecar route through this seam. The reference
/// sidecar is `store-s3-recording`.
pub const STORAGE_RECORDING: &str = "storage.recording";

// ---------------------------------------------------------------------
// Control-schema validation
// ---------------------------------------------------------------------

/// Structured validation failure. Serializable to JSON so MCP tools
/// can put it straight into the `error.data` field.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// `controls.rate`, `controls.stability`, etc.
    pub field: String,
    /// Human-readable reason.
    pub reason: String,
    /// Caller-corrective hint — e.g. `supported: [...]` for enum,
    /// `maximum: 2.0` for range. Empty when there is nothing useful
    /// to attach.
    pub hint: Option<Value>,
}

impl ValidationError {
    /// Render as the MCP `error.data` payload. `field` and `reason`
    /// are always present; `hint` fields are merged in when set.
    #[must_use]
    pub fn into_json(self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("field".into(), Value::String(self.field));
        obj.insert("reason".into(), Value::String(self.reason));
        if let Some(h) = self.hint
            && let Value::Object(hint) = h
        {
            for (k, v) in hint {
                obj.insert(k, v);
            }
        }
        Value::Object(obj)
    }
}

/// Validate `submitted` controls against the `declared` schema. Unknown
/// keys are rejected — this is the whole point.
///
/// `declared` is the `controls` map from a [`CapabilityDescriptor`]
/// (i.e. `descriptor.extra.get("controls")`).
pub fn validate_controls(
    declared: &BTreeMap<String, Value>,
    submitted: &Value,
) -> Result<(), ValidationError> {
    let Some(obj) = submitted.as_object() else {
        if submitted.is_null() {
            return Ok(());
        }
        return Err(ValidationError {
            field: "controls".into(),
            reason: "must be an object".into(),
            hint: None,
        });
    };

    for (key, value) in obj {
        let Some(schema) = declared.get(key) else {
            let supported: Vec<&String> = declared.keys().collect();
            return Err(ValidationError {
                field: format!("controls.{key}"),
                reason: "not supported by provider".into(),
                hint: Some(json!({ "supported": supported })),
            });
        };
        check_one(&format!("controls.{key}"), schema, value)?;
    }
    Ok(())
}

fn check_one(field: &str, schema: &Value, value: &Value) -> Result<(), ValidationError> {
    let expected_type = schema.get("type").and_then(Value::as_str);
    if let Some(t) = expected_type
        && !type_matches(t, value)
    {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: format!("expected type `{t}`, got {}", json_type_name(value)),
            hint: None,
        });
    }

    match expected_type {
        Some("number" | "integer") => check_range(field, schema, value)?,
        Some("string") => check_enum(field, schema, value)?,
        _ => {}
    }
    Ok(())
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn check_range(field: &str, schema: &Value, value: &Value) -> Result<(), ValidationError> {
    let Some(n) = value.as_f64() else {
        return Ok(());
    };
    if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
        && n < min
    {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: "below minimum".into(),
            hint: Some(json!({ "minimum": min, "got": n })),
        });
    }
    if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
        && n > max
    {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: "above maximum".into(),
            hint: Some(json!({ "maximum": max, "got": n })),
        });
    }
    Ok(())
}

fn check_enum(field: &str, schema: &Value, value: &Value) -> Result<(), ValidationError> {
    let Some(list) = schema.get("enum").and_then(Value::as_array) else {
        return Ok(());
    };
    if !list.iter().any(|v| v == value) {
        return Err(ValidationError {
            field: field.to_owned(),
            reason: "not in allowed enum".into(),
            hint: Some(json!({ "allowed": list })),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Provider / Registry traits
// ---------------------------------------------------------------------

/// Error surface for [`AiProvider::invoke`]. Kept loose on purpose:
/// the consuming tool layer maps this into its own `ToolError`
/// vocabulary and the hosting crate decides which string to pass up.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ProviderError(pub String);

impl From<String> for ProviderError {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ProviderError {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// One registered AI plugin, abstracted over its host tier (sidecar,
/// WASM, script, ...). Implementations live in the host crates; the
/// consumer only needs the metadata plus `invoke`.
#[async_trait]
pub trait AiProvider: Send + Sync {
    /// Plugin name (matches the manifest).
    fn name(&self) -> &str;
    /// Plugin version string.
    fn version(&self) -> &str;
    /// Human-readable description.
    fn description(&self) -> &str;
    /// ABI revision the plugin targets (e.g. `"1.0"`).
    fn abi(&self) -> &str;
    /// Capabilities the plugin advertised at load time.
    fn capabilities(&self) -> &[CapabilityDescriptor];
    /// Invoke a plugin method (`synthesize`, `transcribe`, `chat`, ...)
    /// with a JSON params object. Returns the plugin's JSON result.
    async fn invoke(&self, method: &str, params: Value) -> Result<Value, ProviderError>;
}

/// Registry of loaded AI plugins. Cheaply cloneable via `Arc<dyn>`.
#[async_trait]
pub trait AiRegistry: Send + Sync {
    /// Fetch one provider by name.
    fn get(&self, name: &str) -> Option<Arc<dyn AiProvider>>;
    /// Snapshot every registered provider.
    fn snapshot(&self) -> Vec<Arc<dyn AiProvider>>;
    /// Flatten capability descriptors across every provider.
    fn capabilities(&self) -> Vec<CapabilityDescriptor> {
        self.snapshot()
            .into_iter()
            .flat_map(|p| p.capabilities().to_vec())
            .collect()
    }
    /// Number of registered providers.
    fn len(&self) -> usize {
        self.snapshot().len()
    }
    /// `true` if no providers are registered.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Shut down every provider. Typically called on engine shutdown.
    async fn shutdown_all(&self);
    /// Re-spawn one plugin by name, replacing the current entry. Used
    /// by the `reload_plugin` control-plane tool. Default impl
    /// responds "not supported" so registries built from a static
    /// source (tests, embedded defaults) don't need to implement this.
    async fn reload(&self, _name: &str) -> Result<(), ProviderError> {
        Err(ProviderError(
            "reload not supported by this registry".into(),
        ))
    }
}

// ---------------------------------------------------------------------
// Dispatcher — slice 3.1
// ---------------------------------------------------------------------

/// Errors surfaced by [`AiDispatcher::invoke`].
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// No plugin in the registry claims `capability`. Either the
    /// operator didn't load one, or none survived validation.
    #[error("no provider for capability `{0}`")]
    NoProvider(String),
    /// Every candidate the dispatcher tried failed or timed out.
    /// The last error is surfaced; the dispatcher logs each prior
    /// failure at `warn` before proceeding to the next candidate.
    #[error("all {tried} providers failed; last: {last}")]
    AllFailed {
        /// How many providers the dispatcher tried before giving up.
        tried: usize,
        /// Error from the final attempt — most useful single signal.
        last: ProviderError,
    },
}

/// Policy knobs for [`AiDispatcher::invoke`].
#[derive(Clone, Debug)]
pub struct DispatchPolicy {
    /// Per-attempt timeout. The dispatcher fails over when this
    /// expires, not when the underlying transport times out — that's
    /// a floor, not a ceiling.
    pub per_attempt_timeout: std::time::Duration,
    /// Maximum number of candidates to try before returning
    /// [`DispatchError::AllFailed`]. Safety-valve against a
    /// pathological registry with dozens of broken providers.
    pub max_attempts: usize,
}

impl Default for DispatchPolicy {
    fn default() -> Self {
        Self {
            // LLMs can take ~tens of seconds for long generations;
            // 30 s is a generous default that still lets the
            // dispatcher fail over before the MCP call-level
            // timeout triggers.
            per_attempt_timeout: std::time::Duration::from_secs(30),
            max_attempts: 4,
        }
    }
}

/// Breaker state for one provider, tracked by the dispatcher so a
/// flapping plugin doesn't keep getting picked first.
///
/// Simple count-with-cooldown: after `OPEN_AFTER` consecutive
/// failures the provider is marked Open for `OPEN_COOLDOWN`; during
/// that window the dispatcher skips it. Any success closes it.
#[derive(Debug, Default)]
struct ProviderHealth {
    consecutive_failures: std::sync::atomic::AtomicU32,
    /// Unix-seconds at which the breaker last tripped. `0` means Closed.
    tripped_at_unix: std::sync::atomic::AtomicU64,
}

impl ProviderHealth {
    const OPEN_AFTER: u32 = 3;
    const OPEN_COOLDOWN_SECS: u64 = 30;

    fn is_open(&self) -> bool {
        use std::sync::atomic::Ordering;
        let tripped = self.tripped_at_unix.load(Ordering::Acquire);
        if tripped == 0 {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        now.saturating_sub(tripped) < Self::OPEN_COOLDOWN_SECS
    }

    fn record_success(&self) {
        use std::sync::atomic::Ordering;
        self.consecutive_failures.store(0, Ordering::Release);
        self.tripped_at_unix.store(0, Ordering::Release);
    }

    fn record_failure(&self) {
        use std::sync::atomic::Ordering;
        let n = self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1;
        if n >= Self::OPEN_AFTER {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            self.tripped_at_unix.store(now, Ordering::Release);
        }
    }
}

/// Routes `ai_invoke(capability, ...)` calls to the best available
/// provider, handling priority ordering, per-plugin health, and
/// fail-over to the next candidate on timeout / error.
///
/// One dispatcher per engine is enough — cheap to `Arc`-clone.
///
/// ## Selection rule
///
/// 1. Every provider whose `capabilities()` includes a descriptor
///    with matching `capability` string is a candidate.
/// 2. Breaker-open providers are skipped unless every candidate is
///    Open (in which case the dispatcher still tries them — fail
///    late rather than refuse service).
/// 3. Remaining candidates are sorted by the descriptor's `priority`
///    (lower wins). Ties broken by `latency_ms.p50`, then the
///    plugin name lexically for determinism.
pub struct AiDispatcher {
    registry: Arc<dyn AiRegistry>,
    policy: DispatchPolicy,
    metrics: Option<Arc<crate::Metrics>>,
    /// Per-plugin health. Populated lazily on first failure.
    health: std::sync::Mutex<std::collections::BTreeMap<String, Arc<ProviderHealth>>>,
}

impl AiDispatcher {
    /// Build a dispatcher against `registry` with default policy.
    #[must_use]
    pub fn new(registry: Arc<dyn AiRegistry>) -> Self {
        Self {
            registry,
            policy: DispatchPolicy::default(),
            metrics: None,
            health: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Builder: override the default dispatch policy.
    #[must_use]
    pub fn with_policy(mut self, policy: DispatchPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Builder: attach a metrics handle. When set, the dispatcher
    /// increments `smiths_ai_invocations` on each call and
    /// `smiths_ai_failovers` on each fail-over.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<crate::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Return the ranked provider list for `capability` — highest-
    /// priority first. Public so the `describe_provider` MCP tool
    /// can show the dispatcher's current view without invoking.
    #[must_use]
    pub fn candidates(&self, capability: &str) -> Vec<Arc<dyn AiProvider>> {
        let mut ranked: Vec<(Arc<dyn AiProvider>, u8, u64, String)> = self
            .registry
            .snapshot()
            .into_iter()
            .filter_map(|p| {
                let desc = p
                    .capabilities()
                    .iter()
                    .find(|d| d.capability == capability)?
                    .clone();
                let p50 = desc
                    .latency_ms
                    .as_ref()
                    .and_then(|l| l.p50)
                    .unwrap_or(u64::MAX);
                let name = p.name().to_owned();
                Some((p, desc.priority, p50, name))
            })
            .collect();
        ranked.sort_by_key(|(_, pri, p50, name)| (*pri, *p50, name.clone()));
        ranked.into_iter().map(|(p, _, _, _)| p).collect()
    }

    /// Invoke `method` on the best-fit provider for `capability`,
    /// failing over on timeout / error. Returns the first success
    /// or [`DispatchError::AllFailed`].
    pub async fn invoke(
        &self,
        capability: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, DispatchError> {
        if let Some(m) = &self.metrics {
            m.ai_invocations
                .get_or_create(&crate::metrics::AiCapabilityLabel {
                    capability: capability.to_owned(),
                })
                .inc();
        }
        let candidates = self.candidates(capability);
        if candidates.is_empty() {
            return Err(DispatchError::NoProvider(capability.to_owned()));
        }

        // Partition by breaker state — healthy first, Open providers
        // last. Within each partition preserve the priority order.
        let (healthy, open): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|p| !self.health_for(p.name()).is_open());
        let ordered: Vec<_> = healthy.into_iter().chain(open).collect();

        let mut last_err: Option<ProviderError> = None;
        let mut tried = 0usize;
        for (idx, provider) in ordered
            .into_iter()
            .take(self.policy.max_attempts)
            .enumerate()
        {
            if idx > 0 {
                // Any attempt after the first is a fail-over. The
                // prior provider must have returned error or timeout
                // (the `return Ok(v)` below short-circuits on
                // success). Increment BEFORE the call so we don't
                // lose the tick when this attempt also returns Ok.
                if let Some(m) = &self.metrics {
                    m.ai_failovers
                        .get_or_create(&crate::metrics::AiCapabilityLabel {
                            capability: capability.to_owned(),
                        })
                        .inc();
                }
            }
            tried += 1;
            let health = self.health_for(provider.name());
            let attempt = tokio::time::timeout(
                self.policy.per_attempt_timeout,
                provider.invoke(method, params.clone()),
            )
            .await;
            match attempt {
                Ok(Ok(v)) => {
                    health.record_success();
                    self.credit_tokens(provider.name(), &v);
                    return Ok(v);
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        plugin = %provider.name(), capability, ?e,
                        "ai dispatcher: provider failed; falling over"
                    );
                    health.record_failure();
                    last_err = Some(e);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        plugin = %provider.name(), capability,
                        timeout_ms = u64::try_from(self.policy.per_attempt_timeout.as_millis()).unwrap_or(u64::MAX),
                        "ai dispatcher: provider timed out; falling over"
                    );
                    health.record_failure();
                    last_err = Some(ProviderError(format!(
                        "timeout after {} ms",
                        self.policy.per_attempt_timeout.as_millis()
                    )));
                }
            }
        }
        Err(DispatchError::AllFailed {
            tried,
            last: last_err.unwrap_or_else(|| ProviderError("no attempts recorded".into())),
        })
    }

    /// Scrape `usage.{input,output}_tokens` off a successful response
    /// and credit `smiths_ai_tokens_total`. Accepts both the flat shape
    /// (`{"usage": {"input_tokens": N, "output_tokens": M}}`, which is
    /// what the reference sidecars emit) and a nested `message.usage`
    /// shape for providers that wrap the assistant reply. Silently
    /// no-ops when no metrics handle is attached or the shape doesn't
    /// match — the metric is best-effort, not load-bearing.
    fn credit_tokens(&self, provider: &str, response: &Value) {
        let Some(metrics) = &self.metrics else {
            return;
        };
        let usage = response
            .get("usage")
            .or_else(|| response.get("message").and_then(|m| m.get("usage")));
        let Some(usage) = usage else { return };
        for (key, dir) in [("input_tokens", "input"), ("output_tokens", "output")] {
            if let Some(n) = usage.get(key).and_then(Value::as_u64)
                && n > 0
            {
                metrics
                    .ai_tokens
                    .get_or_create(&crate::metrics::AiTokensLabel {
                        provider: provider.to_owned(),
                        dir: dir.to_owned(),
                    })
                    .inc_by(n);
            }
        }
    }

    fn health_for(&self, plugin: &str) -> Arc<ProviderHealth> {
        // Mutex can only poison if a prior holder panicked mid-update;
        // the entries are plain atomics, so recovery is just "clear
        // any half-written state" — which in our case is nothing. Treat
        // poison as benign and reuse the inner map.
        let mut guard = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .entry(plugin.to_owned())
            .or_insert_with(|| Arc::new(ProviderHealth::default()))
            .clone()
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tts_descriptor_with_extras() {
        let v = json!({
            "capability": "ai.tts",
            "plugin":     "ai-tts-mock",
            "model_id":   "mock-voice-v1",
            "abi":        "1.0",
            "voices":     [{"id": "a", "lang": "ru", "gender": "female"}],
            "controls":   {"rate": {"type": "number"}}
        });
        let d: CapabilityDescriptor = serde_json::from_value(v).unwrap();
        d.validate().unwrap();
        assert!(d.extra.contains_key("voices"));
        assert!(d.extra.contains_key("controls"));
    }

    #[test]
    fn rejects_unknown_namespace() {
        let d = CapabilityDescriptor {
            capability: "telephony.dial".into(),
            plugin: "x".into(),
            model_id: String::new(),
            abi: "1.0".into(),
            description: String::new(),
            latency_ms: None,
            concurrency: None,
            priority: DEFAULT_PRIORITY,
            extra: BTreeMap::new(),
        };
        assert!(d.validate().is_err());
    }

    #[test]
    fn accepts_storage_vector_capability() {
        // Slice 3.4: `storage.*` joins `ai.*` / `media.*` as a
        // recognized plugin capability namespace so vector /
        // recording sidecars validate.
        let d = CapabilityDescriptor {
            capability: STORAGE_VECTOR.into(),
            plugin: "store-qdrant".into(),
            model_id: String::new(),
            abi: "1.0".into(),
            description: "Qdrant vector store".into(),
            latency_ms: None,
            concurrency: None,
            priority: DEFAULT_PRIORITY,
            extra: BTreeMap::new(),
        };
        d.validate().expect("storage.vector must validate");
    }

    #[test]
    fn accepts_storage_recording_capability() {
        let d = CapabilityDescriptor {
            capability: STORAGE_RECORDING.into(),
            plugin: "store-s3-recording".into(),
            model_id: String::new(),
            abi: "1.0".into(),
            description: "S3-compatible recording store".into(),
            latency_ms: None,
            concurrency: None,
            priority: DEFAULT_PRIORITY,
            extra: BTreeMap::new(),
        };
        d.validate().expect("storage.recording must validate");
    }

    #[test]
    fn accepts_media_streaming_rtp_capability() {
        // Slice 2.5: `media.*` joins `ai.*` as a recognized plugin
        // capability namespace so streaming-RTP consumers (DTMF
        // sidecars, recording sidecars) validate without hacking
        // around the namespace check.
        let d = CapabilityDescriptor {
            capability: MEDIA_STREAMING_RTP.into(),
            plugin: "dtmf-inband".into(),
            model_id: String::new(),
            abi: "1.0".into(),
            description: "inband DTMF detector".into(),
            latency_ms: None,
            concurrency: None,
            priority: DEFAULT_PRIORITY,
            extra: BTreeMap::new(),
        };
        d.validate().expect("media.streaming_rtp must validate");
    }

    fn declared() -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        m.insert(
            "rate".into(),
            json!({"type": "number", "minimum": 0.5, "maximum": 2.0, "default": 1.0}),
        );
        m.insert(
            "voice".into(),
            json!({"type": "string", "enum": ["a", "b"]}),
        );
        m.insert(
            "beam".into(),
            json!({"type": "integer", "minimum": 1, "maximum": 10}),
        );
        m
    }

    #[test]
    fn null_submitted_is_ok() {
        validate_controls(&declared(), &Value::Null).unwrap();
    }

    #[test]
    fn empty_object_is_ok() {
        validate_controls(&declared(), &json!({})).unwrap();
    }

    #[test]
    fn valid_values_pass() {
        validate_controls(&declared(), &json!({"rate": 1.1, "voice": "a", "beam": 3})).unwrap();
    }

    #[test]
    fn unknown_key_rejected_with_supported_list() {
        let err = validate_controls(&declared(), &json!({"stability": 0.7})).unwrap_err();
        assert_eq!(err.field, "controls.stability");
        assert_eq!(err.reason, "not supported by provider");
        let hint = err.hint.unwrap();
        let supported: Vec<String> = serde_json::from_value(hint["supported"].clone()).unwrap();
        assert_eq!(supported, vec!["beam", "rate", "voice"]);
    }

    #[test]
    fn above_maximum_rejected() {
        let err = validate_controls(&declared(), &json!({"rate": 3.0})).unwrap_err();
        assert_eq!(err.field, "controls.rate");
        assert_eq!(err.reason, "above maximum");
    }

    #[test]
    fn below_minimum_rejected() {
        let err = validate_controls(&declared(), &json!({"beam": 0})).unwrap_err();
        assert_eq!(err.field, "controls.beam");
        assert_eq!(err.reason, "below minimum");
    }

    #[test]
    fn wrong_type_rejected() {
        let err = validate_controls(&declared(), &json!({"rate": "fast"})).unwrap_err();
        assert_eq!(err.field, "controls.rate");
        assert!(err.reason.starts_with("expected type"));
    }

    #[test]
    fn enum_miss_rejected() {
        let err = validate_controls(&declared(), &json!({"voice": "c"})).unwrap_err();
        assert_eq!(err.field, "controls.voice");
        assert_eq!(err.reason, "not in allowed enum");
    }

    // -----------------------------------------------------------------
    // Dispatcher tests (slice 3.1)
    // -----------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering as AOrd};

    /// Scripted provider used to verify dispatcher ordering + failover.
    /// Each invoke returns the next entry from `script`; `"ok"` →
    /// success with the plugin name echoed back, `"err"` → error,
    /// `"slow"` → sleep 200 ms then error (for timeout tests).
    struct MockProvider {
        name: String,
        cap: CapabilityDescriptor,
        script: std::sync::Mutex<Vec<&'static str>>,
        calls: AtomicUsize,
    }

    impl MockProvider {
        fn new(name: &str, capability: &str, priority: u8) -> Arc<Self> {
            let desc = CapabilityDescriptor {
                capability: capability.into(),
                plugin: name.into(),
                model_id: String::new(),
                abi: "1.0".into(),
                description: format!("mock {name}"),
                latency_ms: None,
                concurrency: None,
                priority,
                extra: BTreeMap::new(),
            };
            Arc::new(Self {
                name: name.into(),
                cap: desc,
                script: std::sync::Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            })
        }

        fn script(self: &Arc<Self>, verdicts: &[&'static str]) {
            *self.script.lock().unwrap() = verdicts.iter().rev().copied().collect();
        }
    }

    #[async_trait]
    impl AiProvider for MockProvider {
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
        async fn invoke(&self, _method: &str, _params: Value) -> Result<Value, ProviderError> {
            self.calls.fetch_add(1, AOrd::Relaxed);
            let verdict = self.script.lock().unwrap().pop().unwrap_or("ok");
            match verdict {
                "ok" => Ok(json!({ "who": self.name })),
                "err" => Err(ProviderError(format!("{} boom", self.name))),
                "slow" => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    Err(ProviderError("still slow".into()))
                }
                "usage" => Ok(json!({
                    "who": self.name,
                    "usage": { "input_tokens": 12, "output_tokens": 34 },
                })),
                other => Err(ProviderError(format!("unknown verdict `{other}`"))),
            }
        }
    }

    struct MockRegistry(Vec<Arc<dyn AiProvider>>);

    #[async_trait]
    impl AiRegistry for MockRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn AiProvider>> {
            self.0.iter().find(|p| p.name() == name).cloned()
        }
        fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
            self.0.clone()
        }
        async fn shutdown_all(&self) {}
    }

    fn registry(providers: Vec<Arc<dyn AiProvider>>) -> Arc<dyn AiRegistry> {
        Arc::new(MockRegistry(providers))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn candidates_order_lowest_priority_first() {
        let a: Arc<dyn AiProvider> = MockProvider::new("a", "ai.llm.chat", 30);
        let b: Arc<dyn AiProvider> = MockProvider::new("b", "ai.llm.chat", 10);
        let c: Arc<dyn AiProvider> = MockProvider::new("c", "ai.llm.chat", 50);
        let d = AiDispatcher::new(registry(vec![a, b, c]));
        let names: Vec<String> = d
            .candidates("ai.llm.chat")
            .into_iter()
            .map(|p| p.name().to_owned())
            .collect();
        assert_eq!(names, vec!["b", "a", "c"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_provider_for_unknown_capability() {
        let d = AiDispatcher::new(registry(vec![MockProvider::new("a", "ai.llm.chat", 50)]));
        match d.invoke("ai.asr", "transcribe", json!({})).await {
            Err(DispatchError::NoProvider(cap)) => assert_eq!(cap, "ai.asr"),
            other => panic!("expected NoProvider, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failover_picks_second_candidate_on_first_error() {
        let a: Arc<MockProvider> = MockProvider::new("a", "ai.llm.chat", 10);
        let b: Arc<MockProvider> = MockProvider::new("b", "ai.llm.chat", 20);
        a.script(&["err"]);
        b.script(&["ok"]);
        let dp: Arc<dyn AiProvider> = a.clone();
        let dp2: Arc<dyn AiProvider> = b.clone();
        let d = AiDispatcher::new(registry(vec![dp, dp2]));
        let v = d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        assert_eq!(v["who"], "b", "dispatcher must fail over to b");
        assert_eq!(a.calls.load(AOrd::Relaxed), 1);
        assert_eq!(b.calls.load(AOrd::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn all_failed_returns_last_error() {
        let a: Arc<MockProvider> = MockProvider::new("a", "ai.llm.chat", 10);
        let b: Arc<MockProvider> = MockProvider::new("b", "ai.llm.chat", 20);
        a.script(&["err"]);
        b.script(&["err"]);
        let dp: Arc<dyn AiProvider> = a;
        let dp2: Arc<dyn AiProvider> = b;
        let d = AiDispatcher::new(registry(vec![dp, dp2]));
        match d.invoke("ai.llm.chat", "chat", json!({})).await {
            Err(DispatchError::AllFailed { tried, last }) => {
                assert_eq!(tried, 2);
                assert!(last.0.contains("b boom"));
            }
            other => panic!("expected AllFailed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_trips_failover() {
        let slow: Arc<MockProvider> = MockProvider::new("slow", "ai.llm.chat", 10);
        let fast: Arc<MockProvider> = MockProvider::new("fast", "ai.llm.chat", 20);
        slow.script(&["slow"]);
        fast.script(&["ok"]);
        let dp: Arc<dyn AiProvider> = slow;
        let dp2: Arc<dyn AiProvider> = fast;
        let d = AiDispatcher::new(registry(vec![dp, dp2])).with_policy(DispatchPolicy {
            per_attempt_timeout: std::time::Duration::from_millis(30),
            max_attempts: 4,
        });
        let v = d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        assert_eq!(v["who"], "fast");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn breaker_opens_after_three_consecutive_failures() {
        let flaky: Arc<MockProvider> = MockProvider::new("flaky", "ai.llm.chat", 10);
        let stable: Arc<MockProvider> = MockProvider::new("stable", "ai.llm.chat", 20);
        flaky.script(&["err", "err", "err", "ok", "ok"]);
        stable.script(&["ok", "ok", "ok", "ok"]);
        let dp: Arc<dyn AiProvider> = flaky.clone();
        let dp2: Arc<dyn AiProvider> = stable.clone();
        let d = AiDispatcher::new(registry(vec![dp, dp2]));

        // First three calls each fail through flaky → failover to stable.
        for _ in 0..3 {
            let _ = d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        }
        // Fourth call — flaky's breaker is Open; dispatcher skips it.
        let before = flaky.calls.load(AOrd::Relaxed);
        let _ = d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        let after = flaky.calls.load(AOrd::Relaxed);
        assert_eq!(
            before, after,
            "open breaker must skip flaky entirely (calls: {before} → {after})"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_count_invocations_and_failovers() {
        let a: Arc<MockProvider> = MockProvider::new("a", "ai.llm.chat", 10);
        let b: Arc<MockProvider> = MockProvider::new("b", "ai.llm.chat", 20);
        a.script(&["err", "ok"]);
        b.script(&["ok", "ok"]);
        let metrics = crate::Metrics::noop();
        let dp: Arc<dyn AiProvider> = a;
        let dp2: Arc<dyn AiProvider> = b;
        let d = AiDispatcher::new(registry(vec![dp, dp2])).with_metrics(metrics.clone());

        // Call 1 — a errors, b succeeds → 1 invocation + 1 failover.
        d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        // Call 2 — a succeeds first try → 1 more invocation, no failover.
        d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();

        let lbl = crate::metrics::AiCapabilityLabel {
            capability: "ai.llm.chat".into(),
        };
        assert_eq!(metrics.ai_invocations.get_or_create(&lbl).get(), 2);
        assert_eq!(metrics.ai_failovers.get_or_create(&lbl).get(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tokens_metric_credits_input_and_output_on_success() {
        let a: Arc<MockProvider> = MockProvider::new("a", "ai.llm.chat", 10);
        a.script(&["usage", "usage"]);
        let metrics = crate::Metrics::noop();
        let dp: Arc<dyn AiProvider> = a;
        let d = AiDispatcher::new(registry(vec![dp])).with_metrics(metrics.clone());

        // Two calls, each credits 12 input + 34 output.
        d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();

        let input_label = crate::metrics::AiTokensLabel {
            provider: "a".into(),
            dir: "input".into(),
        };
        let output_label = crate::metrics::AiTokensLabel {
            provider: "a".into(),
            dir: "output".into(),
        };
        assert_eq!(metrics.ai_tokens.get_or_create(&input_label).get(), 24);
        assert_eq!(metrics.ai_tokens.get_or_create(&output_label).get(), 68);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tokens_metric_silent_when_usage_absent() {
        let a: Arc<MockProvider> = MockProvider::new("a", "ai.llm.chat", 10);
        a.script(&["ok"]);
        let metrics = crate::Metrics::noop();
        let dp: Arc<dyn AiProvider> = a;
        let d = AiDispatcher::new(registry(vec![dp])).with_metrics(metrics.clone());
        d.invoke("ai.llm.chat", "chat", json!({})).await.unwrap();
        // No usage block → no credit, no fresh label entry observed.
        let lbl = crate::metrics::AiTokensLabel {
            provider: "a".into(),
            dir: "input".into(),
        };
        assert_eq!(metrics.ai_tokens.get_or_create(&lbl).get(), 0);
    }
}
