//! Plugin inventory and lifecycle: `list_ai_providers`,
//! `describe_provider`, `reload_plugin`, `put_script`.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::require_str;
use crate::tool::{Tool, ToolContext, ToolError};

/// `list_ai_providers` — every registered AI plugin with a short
/// summary. Full descriptors via `describe_provider`.
pub struct ListAiProvidersTool;

#[async_trait]
impl Tool for ListAiProvidersTool {
    fn name(&self) -> &'static str {
        "list_ai_providers"
    }

    fn description(&self) -> &'static str {
        "List every loaded AI plugin (ai.tts / ai.asr / ai.llm / ai.embed) \
         with its declared capabilities."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "capability": {
                    "type": "string",
                    "description": "Filter: only return providers for this capability (e.g. 'ai.tts')."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let filter = args
            .get("capability")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let providers: Vec<_> = ctx
            .plugins
            .snapshot()
            .into_iter()
            .filter_map(|e| {
                let capabilities: Vec<_> = e
                    .capabilities()
                    .iter()
                    .filter(|d| filter.as_ref().is_none_or(|f| &d.capability == f))
                    .collect();
                if capabilities.is_empty() {
                    return None;
                }
                Some(json!({
                    "plugin":       e.name(),
                    "version":      e.version(),
                    "description":  e.description(),
                    "abi":          e.abi(),
                    "capabilities": capabilities.iter().map(|d| json!({
                        "capability": d.capability,
                        "model_id":   d.model_id,
                    })).collect::<Vec<_>>(),
                }))
            })
            .collect();
        Ok(json!({ "providers": providers, "count": providers.len() }))
    }
}

/// `describe_provider` — full capability descriptor for one plugin.
pub struct DescribeProviderTool;

#[async_trait]
impl Tool for DescribeProviderTool {
    fn name(&self) -> &'static str {
        "describe_provider"
    }

    fn description(&self) -> &'static str {
        "Return the complete capability descriptor(s) a plugin advertised at load time."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin": {
                    "type": "string",
                    "description": "Plugin name as declared in its manifest."
                }
            },
            "required": ["plugin"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = require_str(&args, "plugin")?;
        let entry = ctx
            .plugins
            .get(name)
            .ok_or_else(|| ToolError::NotFound(format!("plugin {name}")))?;
        Ok(json!({
            "plugin":       entry.name(),
            "version":      entry.version(),
            "description":  entry.description(),
            "abi":          entry.abi(),
            "capabilities": entry.capabilities(),
        }))
    }
}

/// `reload_plugin` — drain a loaded plugin's sidecar, re-parse its
/// manifest, and re-spawn. Useful when a plugin file was edited on
/// disk without restarting the engine.
pub struct ReloadPluginTool;

#[async_trait]
impl Tool for ReloadPluginTool {
    fn name(&self) -> &'static str {
        "reload_plugin"
    }

    fn description(&self) -> &'static str {
        "Re-spawn one loaded AI plugin from disk (drains the current \
         sidecar, re-runs the describe_capabilities handshake)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "plugin": { "type": "string", "description": "Plugin name (as registered)." }
            },
            "required": ["plugin"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = require_str(&args, "plugin")?;
        match ctx.plugins.reload(name).await {
            Ok(()) => Ok(json!({ "plugin": name, "status": "reloaded" })),
            Err(e) => Err(ToolError::Internal(e.to_string())),
        }
    }
}

/// `put_script(name, source, engine)` — push a new script into a
/// loaded script-tier plugin's directory and kick a reload. `name`
/// must match a plugin already loaded; the engine writes the new body
/// to the plugin's entry file atomically (tempfile + rename) and fires
/// the standard hot-reload path — the same one file-watcher edits go
/// through, so a failing swap surfaces the ordinary rollback.
pub struct PutScriptTool;

#[async_trait]
impl Tool for PutScriptTool {
    fn name(&self) -> &'static str {
        "put_script"
    }

    fn description(&self) -> &'static str {
        "Push a new script body into a loaded script-tier plugin. \
         The engine writes atomically to the plugin's entry file and \
         reloads; the previous version is retained for auto-rollback \
         on 5 consecutive errors."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":   { "type": "string", "description": "Plugin name (must already be loaded)." },
                "source": { "type": "string", "description": "New script body." },
                "engine": {
                    "type": "string",
                    "enum": ["rhai"],
                    "description": "DSL engine. Only `rhai` today."
                }
            },
            "required": ["name", "source"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let name = require_str(&args, "name")?;
        let source = require_str(&args, "source")?;
        let engine = args.get("engine").and_then(Value::as_str).unwrap_or("rhai");
        if engine != "rhai" {
            return Err(ToolError::InvalidArguments(format!(
                "unsupported engine `{engine}`; only `rhai` is wired today"
            )));
        }
        match ctx.plugins.reload_script_source(name, source).await {
            Ok(()) => Ok(json!({
                "name":   name,
                "engine": engine,
                "status": "reloaded"
            })),
            Err(e) => {
                let msg = e.to_string();
                // The plugin crate phrases "plugin not loaded or not
                // script-backed" / "not supported by this registry"
                // — both mean the resource the caller asked for
                // isn't there, which is `NotFound` rather than an
                // internal error.
                let missing = msg.contains("not loaded")
                    || msg.contains("not script")
                    || msg.contains("not supported");
                if missing {
                    Err(ToolError::NotFound(msg))
                } else {
                    Err(ToolError::Internal(msg))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::{FakeProvider, StaticRegistry, TestEngine};
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread")]
    async fn list_ai_providers_empty_by_default() {
        let engine = TestEngine::new();
        let out = ListAiProvidersTool
            .call(json!({}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_and_describe_providers_report_capabilities() {
        let tts = FakeProvider::new(
            "tts",
            "ai.tts",
            &json!({"voices": [{"id": "alice"}]}),
            json!({}),
        );
        let asr = FakeProvider::new("asr", "ai.asr", &json!({}), json!({}));
        let engine = TestEngine::with_registry(Arc::new(StaticRegistry(vec![tts, asr])));

        let out = ListAiProvidersTool
            .call(json!({}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(out["count"], 2);
        let filtered = ListAiProvidersTool
            .call(json!({"capability": "ai.asr"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(filtered["count"], 1);
        assert_eq!(filtered["providers"][0]["plugin"], "asr");

        let described = DescribeProviderTool
            .call(json!({"plugin": "tts"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(described["plugin"], "tts");
        assert_eq!(described["capabilities"][0]["capability"], "ai.tts");
        assert_eq!(described["capabilities"][0]["voices"][0]["id"], "alice");

        let err = DescribeProviderTool
            .call(json!({"plugin": "nope"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reload_plugin_unknown_is_error() {
        let engine = TestEngine::new();
        let err = ReloadPluginTool
            .call(json!({"plugin": "ghost"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Internal(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_script_without_script_plugin_is_not_found() {
        let engine = TestEngine::new();
        let err = PutScriptTool
            .call(
                json!({"name": "route-rhai", "source": "fn describe_capabilities(){[]}", "engine": "rhai"}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn put_script_rejects_unknown_engine() {
        let engine = TestEngine::new();
        let err = PutScriptTool
            .call(
                json!({"name": "x", "source": "y", "engine": "javascript"}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
