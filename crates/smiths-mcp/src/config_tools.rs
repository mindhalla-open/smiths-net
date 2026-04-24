//! `get_config` + `put_config` MCP tools (slice 7.3).
//!
//! Layered on top of the `ConfigReloader` substrate (slice 5.8).
//! `get_config` reads, `put_config` mutates through the same
//! `apply` path SIGHUP drives.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use smiths_core::Config;

use crate::resource::redact_secrets;
use crate::tool::{Tool, ToolContext, ToolError};

// ---------------------------------------------------------------------------
// get_config
// ---------------------------------------------------------------------------

/// `get_config(path?)` — read the live config tree (secrets redacted).
/// `path` is a dotted accessor (e.g. `"sip.rate_limit"`); absent path
/// returns the whole tree.
pub(crate) struct GetConfigTool;

#[async_trait]
impl Tool for GetConfigTool {
    fn name(&self) -> &'static str {
        "get_config"
    }

    fn description(&self) -> &'static str {
        "Read the engine's live configuration. Pass `path` (dotted, e.g. \
         `sip.rate_limit`) to read a subtree; omit for the full config \
         with secrets redacted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Dotted config path (e.g. `sip.rate_limit`). Omit for full tree."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let config = live_config(ctx);
        let mut tree = serde_json::to_value(config.as_ref())
            .map_err(|e| ToolError::Internal(format!("serialize config: {e}")))?;
        redact_secrets(&mut tree);

        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() {
            return Ok(tree);
        }

        let node = walk_path(&tree, path)
            .ok_or_else(|| ToolError::NotFound(format!("config path `{path}` does not exist")))?;
        Ok(node.clone())
    }
}

// ---------------------------------------------------------------------------
// put_config
// ---------------------------------------------------------------------------

/// `put_config(path, value, persist?, dry_run?)` — patch one config
/// field through the `ConfigReloader::apply` path.
pub(crate) struct PutConfigTool;

#[async_trait]
impl Tool for PutConfigTool {
    fn name(&self) -> &'static str {
        "put_config"
    }

    fn description(&self) -> &'static str {
        "Patch one config field on the live engine. Returns the ApplyReport \
         (reloaded / restart-required fields). Use `dry_run=true` to preview \
         without mutating; `persist=true` to write the updated config back \
         to disk."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Dotted config path (e.g. `observability.log_level`)."
                },
                "value": {
                    "description": "New value for the field at `path`."
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "Preview the diff without mutating (default false)."
                },
                "persist": {
                    "type": "boolean",
                    "description": "Write updated config back to the on-disk TOML file (default false)."
                }
            },
            "required": ["path", "value"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("path required".into()))?;
        let new_value = args
            .get("value")
            .ok_or_else(|| ToolError::InvalidArguments("value required".into()))?;
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let persist = args
            .get("persist")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let reloader = ctx.reloader.as_ref().ok_or_else(|| {
            ToolError::NotFound(
                "no ConfigReloader is wired; put_config is unavailable in this context".into(),
            )
        })?;

        // Build the candidate by patching the live config's JSON tree.
        let current = reloader.current();
        let mut tree = serde_json::to_value(current.as_ref())
            .map_err(|e| ToolError::Internal(format!("serialize config: {e}")))?;

        // Capture the before-value for history.
        let before = walk_path(&tree, path).cloned();

        set_path(&mut tree, path, new_value.clone())
            .map_err(|e| ToolError::InvalidArguments(format!("cannot set `{path}`: {e}")))?;

        let candidate: Config = serde_json::from_value(tree)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid config: {e}")))?;

        // Compute the diff/report without applying.
        let report = current.apply_report(&candidate);

        if dry_run {
            return Ok(json!({
                "dry_run":           true,
                "path":              path,
                "before":            before,
                "after":             new_value,
                "reloaded":          report.reloaded,
                "restart_required":  report.restart_required,
            }));
        }

        // Validate before apply.
        if let Err(e) = candidate.validate() {
            return Err(ToolError::InvalidArguments(format!(
                "candidate config failed validation: {e}"
            )));
        }

        // Apply through the reloader (same path SIGHUP uses).
        let canary_window = current.canary.deadline_s;
        let receipt = reloader
            .apply(candidate.clone(), canary_window)
            .await
            .map_err(|e| match e {
                smiths_core::ApplyError::RestartRequired { fields } => ToolError::InvalidArguments(
                    format!("field(s) require engine restart: {fields:?}"),
                ),
                smiths_core::ApplyError::Invalid(msg) => ToolError::InvalidArguments(msg),
            })?;

        // Record in history.
        if let Some(history) = ctx.config_history.as_ref() {
            history.record(path, &before, new_value, false);
        }

        // Persist to disk if requested.
        if persist {
            let Some(config_path) = ctx.config_path.as_ref() else {
                return Err(ToolError::InvalidArguments(
                    "persist=true but no config file path is known".into(),
                ));
            };
            let toml_str = toml::to_string_pretty(&candidate)
                .map_err(|e| ToolError::Internal(format!("serialize TOML: {e}")))?;
            std::fs::write(config_path, toml_str).map_err(|e| {
                ToolError::Internal(format!("writing {}: {e}", config_path.display()))
            })?;
        }

        Ok(json!({
            "applied":           true,
            "change_id":         receipt.id.0,
            "path":              path,
            "before":            before,
            "after":             new_value,
            "reloaded":          receipt.report.reloaded,
            "restart_required":  receipt.report.restart_required,
            "deadline_at_unix":  receipt.deadline_at_unix,
        }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the live config — from the reloader if wired, else from the
/// boot snapshot.
fn live_config(ctx: &ToolContext) -> Arc<Config> {
    ctx.reloader
        .as_ref()
        .map_or_else(|| Arc::clone(&ctx.config), |r| r.current())
}

/// Walk a dotted path into a JSON value.
fn walk_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        cur = cur.as_object()?.get(seg)?;
    }
    Some(cur)
}

/// Set a value at a dotted path in a JSON tree, creating intermediate
/// objects as needed.
fn set_path(root: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Err("empty path".into());
    }

    let mut cur = root;
    for seg in &segments[..segments.len() - 1] {
        if !cur.is_object() {
            return Err(format!("expected object at `{seg}`, found {cur}"));
        }
        cur = cur
            .as_object_mut()
            .ok_or_else(|| format!("expected object at `{seg}`"))?
            .entry(*seg)
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
    }
    let last = segments.last().ok_or("empty path")?;
    let obj = cur
        .as_object_mut()
        .ok_or_else(|| format!("expected object at `{last}`"))?;
    obj.insert((*last).to_owned(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use crate::tool::test_support::{default_config, empty_registry, null_media};
    use smiths_core::{ConfigReloader, EventBus};
    use tokio_util::sync::CancellationToken;

    fn test_ctx() -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        (
            ToolContext::new(state, empty_registry(), default_config(), null_media()),
            cancel,
        )
    }

    fn test_ctx_with_reloader() -> (ToolContext, Arc<ConfigReloader>, CancellationToken) {
        let (ctx, cancel) = test_ctx();
        let reloader = ConfigReloader::new(Config::default());
        let ctx = ctx.with_reloader(Arc::clone(&reloader));
        (ctx, reloader, cancel)
    }

    #[tokio::test]
    async fn get_config_full_returns_redacted_tree() {
        let (ctx, _cancel) = test_ctx();
        let tool = GetConfigTool;
        let result = tool.call(json!({}), &ctx).await.unwrap();
        assert!(result.is_object());
        assert!(result.get("sip").is_some());
        assert!(result.get("observability").is_some());
    }

    #[tokio::test]
    async fn get_config_path_returns_subtree() {
        let (ctx, _cancel) = test_ctx();
        let tool = GetConfigTool;
        let result = tool
            .call(json!({"path": "observability.log_level"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_string());
    }

    #[tokio::test]
    async fn get_config_invalid_path_returns_not_found() {
        let (ctx, _cancel) = test_ctx();
        let tool = GetConfigTool;
        let err = tool
            .call(json!({"path": "nonexistent.deep.path"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn put_config_reloadable_field_applies() {
        let (ctx, reloader, _cancel) = test_ctx_with_reloader();
        let tool = PutConfigTool;
        let result = tool
            .call(
                json!({
                    "path": "observability.log_level",
                    "value": "trace"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["applied"], true);
        assert!(
            result["reloaded"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "observability.log_level")
        );
        // Live config should reflect the change.
        assert_eq!(reloader.current().observability.log_level, "trace");
    }

    #[tokio::test]
    async fn put_config_restart_required_rejects() {
        let (ctx, _reloader, _cancel) = test_ctx_with_reloader();
        let tool = PutConfigTool;
        let err = tool
            .call(
                json!({
                    "path": "observability.log_format",
                    "value": "pretty"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn put_config_dry_run_does_not_mutate() {
        let (ctx, reloader, _cancel) = test_ctx_with_reloader();
        let original_level = reloader.current().observability.log_level.clone();
        let tool = PutConfigTool;
        let result = tool
            .call(
                json!({
                    "path": "observability.log_level",
                    "value": "trace",
                    "dry_run": true
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(result["dry_run"], true);
        assert!(
            result["reloaded"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "observability.log_level")
        );
        // Config should NOT have changed.
        assert_eq!(reloader.current().observability.log_level, original_level);
    }

    #[tokio::test]
    async fn put_config_without_reloader_returns_not_found() {
        let (ctx, _cancel) = test_ctx();
        let tool = PutConfigTool;
        let err = tool
            .call(
                json!({
                    "path": "observability.log_level",
                    "value": "trace"
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn walk_path_navigates_nested_json() {
        let v = json!({"a": {"b": {"c": 42}}});
        assert_eq!(walk_path(&v, "a.b.c"), Some(&json!(42)));
        assert_eq!(walk_path(&v, "a.b"), Some(&json!({"c": 42})));
        assert_eq!(walk_path(&v, "a.x"), None);
    }

    #[test]
    fn set_path_creates_intermediate_objects() {
        let mut v = json!({"a": {"b": 1}});
        set_path(&mut v, "a.c.d", json!(99)).unwrap();
        assert_eq!(v["a"]["c"]["d"], 99);
        // Original value preserved.
        assert_eq!(v["a"]["b"], 1);
    }
}
