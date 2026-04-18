//! Capability descriptor shape — the wire document each plugin returns
//! from `describe_capabilities`.
//!
//! Deliberately permissive: we keep `extra` as arbitrary JSON so
//! capability-specific fields (voices, languages, controls) pass
//! through unchanged. The engine only validates the **common**
//! envelope here; per-capability validation (e.g. which controls are
//! allowed for `ai.tts`) happens at invocation time in the future
//! `speak` / `transcribe` tool implementations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One capability exposed by a plugin. Matches `05-ai-plugin-protocol.md
/// §Common fields` plus an opaque tail of capability-specific fields.
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
    /// `controls`, etc.). The MCP layer passes this straight through
    /// to the agent in `list_ai_providers` / `describe_provider`.
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
    /// How many concurrent invocations the plugin can serve before the
    /// engine should queue.
    #[serde(default)]
    pub max_in_flight: Option<u32>,
}

fn default_abi() -> String {
    "1.0".to_string()
}

impl CapabilityDescriptor {
    /// Validate common-envelope invariants. Returns a descriptive
    /// error when the plugin is confused.
    pub fn validate(&self) -> Result<(), String> {
        if self.capability.is_empty() {
            return Err("capability is empty".into());
        }
        if !self.capability.starts_with("ai.") {
            return Err(format!(
                "capability `{}` is outside the `ai.*` namespace",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
