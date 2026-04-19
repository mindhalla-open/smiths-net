//! Structured audit log for every tool invocation.
//!
//! One `info!` event per call (success **or** failure), tagged with a
//! dedicated `target` so operators can route it to its own sink.
//! Arguments are hashed (SHA-256) instead of logged in full: the hash
//! is enough to correlate audit lines with request traces without
//! spilling secrets.

use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::tool::ToolError;

/// Audit-event target; filter into its own sink via
/// `RUST_LOG=smiths_mcp::audit=info`.
pub const AUDIT_TARGET: &str = "smiths_mcp::audit";

/// Outcome label for the `outcome` field on the audit event.
#[must_use]
pub fn outcome_label(result: &Result<Value, ToolError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(ToolError::InvalidArguments(_)) => "invalid_arguments",
        Err(ToolError::NotFound(_)) => "not_found",
        Err(ToolError::Forbidden(_)) => "forbidden",
        Err(ToolError::Internal(_)) => "internal",
    }
}

/// SHA-256 hex of the canonical JSON form of `args`. `Value::Null` and
/// `{}` hash to distinct digests (the canonical representation differs).
#[must_use]
pub fn args_hash(args: &Value) -> String {
    let canonical = serde_json::to_string(args).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(canonical.as_bytes());
    hex::encode(h.finalize())
}

/// Emit the audit event.
pub fn emit(
    actor: &str,
    tool: &str,
    args: &Value,
    duration: Duration,
    result: &Result<Value, ToolError>,
) {
    let outcome = outcome_label(result);
    let error_msg = match result {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    };
    info!(
        target: AUDIT_TARGET,
        actor = %actor,
        tool = %tool,
        args_hash = %args_hash(args),
        outcome = %outcome,
        duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        error = %error_msg,
        "tool call",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_hash_is_stable_and_distinct() {
        let a = args_hash(&json!({"x": 1, "y": 2}));
        let b = args_hash(&json!({"x": 1, "y": 2}));
        assert_eq!(a, b);
        let c = args_hash(&json!({"x": 1, "y": 3}));
        assert_ne!(a, c);
        assert_eq!(a.len(), 64); // 32 bytes hex
    }

    #[test]
    fn outcome_labels_cover_every_variant() {
        assert_eq!(outcome_label(&Ok(Value::Null)), "ok");
        assert_eq!(
            outcome_label(&Err(ToolError::InvalidArguments("x".into()))),
            "invalid_arguments"
        );
        assert_eq!(
            outcome_label(&Err(ToolError::NotFound("x".into()))),
            "not_found"
        );
        assert_eq!(
            outcome_label(&Err(ToolError::Forbidden("x".into()))),
            "forbidden"
        );
        assert_eq!(
            outcome_label(&Err(ToolError::Internal("x".into()))),
            "internal"
        );
    }
}
