//! Adapter-agnostic tool dispatch — look up, rate limit, validate,
//! invoke, audit, meter.
//!
//! Every adapter (MCP stdio / HTTP, A2A, webhook) funnels tool calls
//! through [`invoke_audited_as`] so the control plane has one
//! enforcement and observability point regardless of transport.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use smiths_core::Metrics;
use smiths_core::metrics::{ToolLabel, ToolOutcomeLabel};

use crate::audit;
use crate::rate_limit::RateLimiter;
use crate::schema::validate_args;
use crate::tool::{ToolContext, ToolError, ToolRegistry};

/// [`invoke_audited_as`] without a caller identity: the rate limit
/// applies on the tool's global bucket.
pub async fn invoke_audited(
    registry: &ToolRegistry,
    rate_limiter: &Arc<RateLimiter>,
    metrics: &Arc<Metrics>,
    ctx: &ToolContext,
    actor: &str,
    tool_name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    invoke_audited_as(
        registry,
        rate_limiter,
        metrics,
        ctx,
        actor,
        None,
        tool_name,
        args,
    )
    .await
}

/// Look up `tool_name` in `registry`, enforce the shared rate limit
/// on behalf of `caller`, validate `args` against the tool's declared
/// schema, invoke it, and emit an audit event plus a Prometheus
/// sample with the outcome. Returns the tool's own result so adapters
/// keep full control of response framing.
///
/// `actor` labels the adapter in audit events; `caller` is the
/// per-caller rate-limit key (peer address, session id) when the
/// adapter knows one.
#[allow(clippy::too_many_arguments)] // one call site per adapter; a builder would obscure the pipeline order
pub async fn invoke_audited_as(
    registry: &ToolRegistry,
    rate_limiter: &Arc<RateLimiter>,
    metrics: &Arc<Metrics>,
    ctx: &ToolContext,
    actor: &str,
    caller: Option<&str>,
    tool_name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    // A missing body arrives as `Null`; every tool schema is an
    // object, so normalise before validation and dispatch.
    let args = if args.is_null() {
        Value::Object(Map::default())
    } else {
        args
    };

    // Registry lookup first so an unknown name never allocates a
    // rate-limit bucket.
    let Some(tool) = registry.get(tool_name) else {
        let err = ToolError::NotFound(format!("tool {tool_name}"));
        reject(metrics, actor, tool_name, &args, &err);
        return Err(err);
    };

    if let Err(limited) = rate_limiter.try_acquire_for(caller, tool_name) {
        let err = ToolError::Forbidden(limited.to_string());
        reject(metrics, actor, tool_name, &args, &err);
        return Err(err);
    }

    if let Err(reason) = validate_args(&tool.input_schema(), &args) {
        let err = ToolError::InvalidArguments(reason);
        reject(metrics, actor, tool_name, &args, &err);
        return Err(err);
    }

    let started = Instant::now();
    let result = tool.call(args.clone(), ctx).await;
    let elapsed = started.elapsed();
    record(metrics, tool_name, &result, Some(elapsed));
    audit::emit(actor, tool_name, &args, elapsed, &result);
    result
}

/// Metrics + audit for a call that never reached the tool.
fn reject(metrics: &Metrics, actor: &str, tool_name: &str, args: &Value, err: &ToolError) {
    let outcome: Result<Value, ToolError> = Err(clone_tool_error(err));
    record(metrics, tool_name, &outcome, None);
    audit::emit(actor, tool_name, args, Duration::ZERO, &outcome);
}

/// Feed one call's outcome into the Prometheus metric set. When
/// `duration` is `None` the call never reached the tool (rate-limit,
/// not-found, or schema rejection), so we skip the latency histogram
/// — recording 0 s would skew operators' bucket readings.
fn record(
    metrics: &Metrics,
    tool: &str,
    result: &Result<Value, ToolError>,
    duration: Option<Duration>,
) {
    let outcome = audit::outcome_label(result).to_owned();
    metrics
        .tool_invocations
        .get_or_create(&ToolOutcomeLabel {
            tool: tool.to_owned(),
            outcome,
        })
        .inc();
    if let Some(d) = duration {
        metrics
            .tool_duration
            .get_or_create(&ToolLabel {
                tool: tool.to_owned(),
            })
            .observe(d.as_secs_f64());
    }
}

/// `ToolError` is not `Clone` (its inner `String` field would need an
/// explicit impl), so we clone the shape we actually care about for
/// audit emission by matching on the variant. Used for the paths that
/// emit audit *before* returning the original `err` to the caller.
fn clone_tool_error(err: &ToolError) -> ToolError {
    match err {
        ToolError::InvalidArguments(m) => ToolError::InvalidArguments(m.clone()),
        ToolError::NotFound(m) => ToolError::NotFound(m.clone()),
        ToolError::Forbidden(m) => ToolError::Forbidden(m.clone()),
        ToolError::Conflict(m) => ToolError::Conflict(m.clone()),
        ToolError::Internal(m) => ToolError::Internal(m.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ControlState;
    use crate::tool::test_support::{default_config, empty_registry, null_media};
    use serde_json::json;
    use smiths_core::{EventBus, RateLimitConfig};
    use tokio_util::sync::CancellationToken;

    fn ctx() -> (ToolContext, CancellationToken) {
        let bus = EventBus::new(8);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        (
            ToolContext::new(state, empty_registry(), default_config(), null_media()),
            cancel,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tool_does_not_allocate_a_rate_limit_bucket() {
        let reg = crate::tools::builtin_registry();
        let rl = Arc::new(RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 1,
        }));
        let (c, _cancel) = ctx();
        for i in 0..50 {
            let err = invoke_audited(
                &reg,
                &rl,
                &Metrics::noop(),
                &c,
                "test",
                &format!("no-such-tool-{i}"),
                json!({}),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ToolError::NotFound(_)));
        }
        assert_eq!(rl.bucket_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limit_is_keyed_by_caller() {
        let reg = crate::tools::builtin_registry();
        let rl = Arc::new(RateLimiter::new(&RateLimitConfig {
            per_sec: 1,
            burst: 1,
        }));
        let (c, _cancel) = ctx();
        let m = Metrics::noop();
        let a = Some("10.0.0.1");
        let b = Some("10.0.0.2");
        invoke_audited_as(&reg, &rl, &m, &c, "t", a, "health", json!({}))
            .await
            .unwrap();
        // Same caller, second call → limited.
        let err = invoke_audited_as(&reg, &rl, &m, &c, "t", a, "health", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Forbidden(_)), "{err}");
        // Different caller still admitted.
        invoke_audited_as(&reg, &rl, &m, &c, "t", b, "health", json!({}))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_violation_is_rejected_before_the_tool_runs() {
        let reg = crate::tools::builtin_registry();
        let rl = Arc::new(RateLimiter::new(&RateLimitConfig::default()));
        let (c, _cancel) = ctx();
        // `call_id` declared as string.
        let err = invoke_audited(
            &reg,
            &rl,
            &Metrics::noop(),
            &c,
            "t",
            "get_call_status",
            json!({"call_id": 12}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "{err}");
        assert!(err.to_string().contains("call_id"), "{err}");
        // Enum violation on `list_calls.phase`.
        let err = invoke_audited(
            &reg,
            &rl,
            &Metrics::noop(),
            &c,
            "t",
            "list_calls",
            json!({"phase": "ringing"}),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn null_arguments_are_treated_as_empty_object() {
        let reg = crate::tools::builtin_registry();
        let rl = Arc::new(RateLimiter::new(&RateLimitConfig::default()));
        let (c, _cancel) = ctx();
        let out = invoke_audited(&reg, &rl, &Metrics::noop(), &c, "t", "health", Value::Null)
            .await
            .unwrap();
        assert_eq!(out["status"], "ok");
    }
}
