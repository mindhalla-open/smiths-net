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
    /// Capabilities live in one of two namespaces today:
    ///
    /// - `ai.*` — AI providers (LLM, TTS, ASR, embed). The original
    ///   plugin tier.
    /// - `media.*` — streaming-RTP consumers. Added in slice 2.5
    ///   so a sidecar plugin can declare itself as something that
    ///   receives per-packet RTP from the engine (e.g. the deferred
    ///   `dtmf-inband` Python sidecar). See [`MEDIA_STREAMING_RTP`].
    pub fn validate(&self) -> Result<(), String> {
        if self.capability.is_empty() {
            return Err("capability is empty".into());
        }
        if !self.capability.starts_with("ai.") && !self.capability.starts_with("media.") {
            return Err(format!(
                "capability `{}` is outside the `ai.*` / `media.*` namespaces",
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
            extra: BTreeMap::new(),
        };
        assert!(d.validate().is_err());
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
}
