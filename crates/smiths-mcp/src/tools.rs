//! Concrete tool implementations shipped in-box.
//!
//! Keep this file small. Any tool here is served identically by the
//! MCP and A2A adapters. New tools land as their own module when they
//! grow beyond a few lines.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::{Tool, ToolContext, ToolError};

/// Build a fully-populated [`crate::ToolRegistry`] with the built-in
/// tool set.
#[must_use]
pub fn builtin_registry() -> crate::ToolRegistry {
    let mut reg = crate::ToolRegistry::new();
    reg.register(ListCallsTool);
    reg.register(GetCallStatusTool);
    reg.register(HealthTool);
    reg
}

/// `list_calls` — return every dialog the engine currently knows
/// about, live or recently terminated.
pub struct ListCallsTool;

#[async_trait]
impl Tool for ListCallsTool {
    fn name(&self) -> &'static str {
        "list_calls"
    }

    fn description(&self) -> &'static str {
        "List active and recently-terminated calls known to the engine."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "phase": {
                    "type": "string",
                    "enum": ["live", "terminated", "all"],
                    "description": "Filter by lifecycle phase. Default: all."
                }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let phase = args
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("all")
            .to_owned();
        let calls = ctx.state.list_calls();
        let filtered: Vec<_> = calls
            .into_iter()
            .filter(|c| match phase.as_str() {
                "live" => matches!(c.phase, crate::control::CallPhase::Live),
                "terminated" => matches!(c.phase, crate::control::CallPhase::Terminated),
                _ => true,
            })
            .collect();
        Ok(json!({ "calls": filtered, "count": filtered.len() }))
    }
}

/// `get_call_status` — details for one call by Call-ID.
pub struct GetCallStatusTool;

#[async_trait]
impl Tool for GetCallStatusTool {
    fn name(&self) -> &'static str {
        "get_call_status"
    }

    fn description(&self) -> &'static str {
        "Fetch the snapshot of a single call by Call-ID."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": {
                    "type": "string",
                    "description": "SIP Call-ID header value."
                }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = args
            .get("call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("call_id required".into()))?;
        let snap = ctx
            .state
            .get_call(call_id)
            .ok_or_else(|| ToolError::NotFound(format!("call {call_id}")))?;
        Ok(serde_json::to_value(&snap).unwrap_or(Value::Null))
    }
}

/// `health` — uptime plus live-call count. Cheap liveness probe
/// exposed on every adapter.
pub struct HealthTool;

#[async_trait]
impl Tool for HealthTool {
    fn name(&self) -> &'static str {
        "health"
    }

    fn description(&self) -> &'static str {
        "Return engine uptime and the current live-call count."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": false })
    }

    async fn call(&self, _args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let calls = ctx.state.list_calls();
        let live = calls
            .iter()
            .filter(|c| matches!(c.phase, crate::control::CallPhase::Live))
            .count();
        Ok(json!({
            "status": "ok",
            "uptime_secs": ctx.state.uptime_secs(),
            "live_calls": live,
            "known_calls": calls.len()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use smiths_core::EventBus;
    use tokio_util::sync::CancellationToken;

    fn ctx_with_state() -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        (ToolContext::new(state), cancel)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_calls_defaults_to_all() {
        let (ctx, _c) = ctx_with_state();
        let out = ListCallsTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out["count"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_call_status_missing_returns_not_found() {
        let (ctx, _c) = ctx_with_state();
        let err = GetCallStatusTool
            .call(json!({"call_id": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_returns_ok_schema() {
        let (ctx, _c) = ctx_with_state();
        let out = HealthTool.call(json!({}), &ctx).await.unwrap();
        assert_eq!(out["status"], "ok");
        assert!(out["uptime_secs"].is_number());
    }

    #[test]
    fn registry_contains_builtins() {
        let reg = builtin_registry();
        assert_eq!(reg.len(), 3);
        assert!(reg.get("list_calls").is_some());
        assert!(reg.get("get_call_status").is_some());
        assert!(reg.get("health").is_some());
    }
}
