//! Adapter-agnostic tool dispatch — rate limit, invoke, audit, meter.
//!
//! Both MCP (stdio/HTTP) and A2A funnel every tool call through
//! [`invoke_audited`] so the control plane has one enforcement and
//! observability point regardless of transport.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;
use smiths_core::Metrics;
use smiths_core::metrics::{ToolLabel, ToolOutcomeLabel};

use crate::audit;
use crate::rate_limit::RateLimiter;
use crate::tool::{ToolContext, ToolError, ToolRegistry};

/// Look up `tool` in `registry`, enforce the shared rate limit, invoke
/// it, and emit an audit event plus a Prometheus sample with the
/// outcome. Returns the tool's own result so adapters keep full
/// control of response framing.
pub async fn invoke_audited(
    registry: &ToolRegistry,
    rate_limiter: &Arc<RateLimiter>,
    metrics: &Arc<Metrics>,
    ctx: &ToolContext,
    actor: &str,
    tool_name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    // Rate-limit check first so a flood of calls never reaches the tool.
    if let Err(reject) = rate_limiter.try_acquire(tool_name) {
        let err = ToolError::Forbidden(reject.to_string());
        record(
            metrics,
            tool_name,
            &Err::<Value, _>(clone_tool_error(&err)),
            None,
        );
        audit::emit(
            actor,
            tool_name,
            &args,
            std::time::Duration::ZERO,
            &Err::<Value, _>(clone_tool_error(&err)),
        );
        return Err(err);
    }

    let Some(tool) = registry.get(tool_name) else {
        let err = ToolError::NotFound(format!("tool {tool_name}"));
        record(
            metrics,
            tool_name,
            &Err::<Value, _>(clone_tool_error(&err)),
            None,
        );
        audit::emit(
            actor,
            tool_name,
            &args,
            std::time::Duration::ZERO,
            &Err::<Value, _>(clone_tool_error(&err)),
        );
        return Err(err);
    };

    let started = Instant::now();
    let result = tool.call(args.clone(), ctx).await;
    let elapsed = started.elapsed();
    record(metrics, tool_name, &result, Some(elapsed));
    audit::emit(actor, tool_name, &args, elapsed, &result);
    result
}

/// Feed one call's outcome into the Prometheus metric set. When
/// `duration` is `None` the call never reached the tool (rate-limit
/// or not-found), so we skip the latency histogram — recording 0 s
/// would skew operators' bucket readings.
fn record(
    metrics: &Metrics,
    tool: &str,
    result: &Result<Value, ToolError>,
    duration: Option<std::time::Duration>,
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
        ToolError::Internal(m) => ToolError::Internal(m.clone()),
    }
}
