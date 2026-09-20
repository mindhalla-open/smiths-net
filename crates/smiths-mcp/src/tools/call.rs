//! Dialog inventory, outbound calls, and media bridging.

use async_trait::async_trait;
use serde_json::{Value, json};
use smiths_core::BridgeLeg;
use smiths_core::media::BridgeId;

use super::{live_media_leg, require_str};
use crate::control::CallPhase;
use crate::tool::{Tool, ToolContext, ToolError};

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
        let phase = args.get("phase").and_then(Value::as_str).unwrap_or("all");
        let filtered: Vec<_> = ctx
            .state
            .list_calls()
            .into_iter()
            .filter(|c| match phase {
                "live" => matches!(c.phase, CallPhase::Live),
                "terminated" => matches!(c.phase, CallPhase::Terminated),
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
        let call_id = require_str(&args, "call_id")?;
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
            .filter(|c| matches!(c.phase, CallPhase::Live))
            .count();
        Ok(json!({
            "status": "ok",
            "uptime_secs": ctx.state.uptime_secs(),
            "live_calls": live,
            "known_calls": calls.len()
        }))
    }
}

/// `make_call(target)` — place an outbound SIP INVITE to a remote URI
/// via the engine's UAC. Returns `{call_id}` once the dialog is
/// established (200 OK + ACK).
pub struct MakeCallTool;

#[async_trait]
impl Tool for MakeCallTool {
    fn name(&self) -> &'static str {
        "make_call"
    }

    fn description(&self) -> &'static str {
        "Place an outbound SIP INVITE to `target` (a `sip:user@host[:port]` URI) \
         and return the Call-ID of the established dialog."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "SIP URI to dial (e.g. `sip:alice@10.0.0.1:5060`)."
                }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let target = require_str(&args, "target")?;
        let originator = ctx.originator.as_ref().ok_or_else(|| {
            ToolError::NotFound("no outbound-call originator configured; enable the SIP UAC".into())
        })?;
        let call_id = originator
            .place_call(target)
            .await
            .map_err(map_call_error)?;
        Ok(json!({ "call_id": call_id, "target": target }))
    }
}

/// `end_call(call_id)` — tear down an outbound dialog previously
/// established by `make_call`.
pub struct EndCallTool;

#[async_trait]
impl Tool for EndCallTool {
    fn name(&self) -> &'static str {
        "end_call"
    }

    fn description(&self) -> &'static str {
        "Send BYE on an outbound dialog previously created by `make_call`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": {
                    "type": "string",
                    "description": "Call-ID returned from `make_call`."
                }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = require_str(&args, "call_id")?;
        let originator = ctx.originator.as_ref().ok_or_else(|| {
            ToolError::NotFound("no outbound-call originator configured; enable the SIP UAC".into())
        })?;
        originator.hangup(call_id).await.map_err(map_call_error)?;
        Ok(json!({ "call_id": call_id, "status": "ended" }))
    }
}

/// `bridge_calls(call_id_a, call_id_b)` — connect the media legs of
/// two live dialogs through the media fabric. Any bridge either leg
/// already belongs to is released first, so re-bridging a leg to a
/// new partner is a single call.
///
/// Legs are bridged as plain RTP: the control-plane call snapshot
/// carries the endpoint and peer address but not SRTP keying
/// material, so an SRTP leg would need the SIP layer's own bridge.
pub struct BridgeCallsTool;

#[async_trait]
impl Tool for BridgeCallsTool {
    fn name(&self) -> &'static str {
        "bridge_calls"
    }

    fn description(&self) -> &'static str {
        "Bridge the media of two live calls so each hears the other. \
         Releases any bridge either leg already has. Returns the new \
         bridge id; `unbridge_call` tears it down."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id_a": { "type": "string", "description": "Call-ID of the first live dialog." },
                "call_id_b": { "type": "string", "description": "Call-ID of the second live dialog." }
            },
            "required": ["call_id_a", "call_id_b"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let a = require_str(&args, "call_id_a")?;
        let b = require_str(&args, "call_id_b")?;
        if a == b {
            return Err(ToolError::InvalidArguments(
                "call_id_a and call_id_b must differ".into(),
            ));
        }
        let (snap_a, endpoint_a, remote_a) = live_media_leg(ctx, a)?;
        let (snap_b, endpoint_b, remote_b) = live_media_leg(ctx, b)?;

        let mut released = Vec::new();
        for old in [snap_a.bridge_id, snap_b.bridge_id].into_iter().flatten() {
            if released.contains(&old.0) {
                continue;
            }
            release_bridge(ctx, old).await;
            released.push(old.0);
        }

        let bridge = ctx
            .media
            .bridge(
                BridgeLeg::plain(endpoint_a, remote_a),
                BridgeLeg::plain(endpoint_b, remote_b),
            )
            .await
            .map_err(|e| ToolError::Internal(format!("bridge: {e}")))?;
        // A `false` here means the leg was purged between lookup and
        // record; the bridge still stands and `unbridge_call` on the
        // other leg releases it.
        let _ = ctx.state.set_bridge(a, Some(bridge));
        let _ = ctx.state.set_bridge(b, Some(bridge));
        Ok(json!({
            "bridge_id": bridge.0,
            "call_id_a": a,
            "call_id_b": b,
            "released_bridges": released,
            "status": "bridged",
        }))
    }
}

/// `unbridge_call(call_id)` — release the bridge a call belongs to.
/// Both legs of the bridge are cleared.
pub struct UnbridgeCallTool;

#[async_trait]
impl Tool for UnbridgeCallTool {
    fn name(&self) -> &'static str {
        "unbridge_call"
    }

    fn description(&self) -> &'static str {
        "Release the media bridge a call belongs to (both legs stop \
         hearing each other). Conflict when the call is not bridged."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "call_id": { "type": "string", "description": "Call-ID of either bridged leg." }
            },
            "required": ["call_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let call_id = require_str(&args, "call_id")?;
        let snap = ctx
            .state
            .get_call(call_id)
            .ok_or_else(|| ToolError::NotFound(format!("call {call_id}")))?;
        let Some(bridge) = snap.bridge_id else {
            return Err(ToolError::Conflict(format!(
                "call {call_id} is not bridged"
            )));
        };
        let legs = release_bridge(ctx, bridge).await;
        Ok(json!({
            "bridge_id": bridge.0,
            "call_id": call_id,
            "released_legs": legs,
            "status": "released",
        }))
    }
}

/// Release `bridge` on the fabric and clear it from every leg that
/// recorded it. Returns the cleared legs.
async fn release_bridge(ctx: &ToolContext, bridge: BridgeId) -> Vec<String> {
    ctx.media.release_bridge(bridge).await;
    let legs = ctx.state.calls_on_bridge(bridge);
    for leg in &legs {
        let _ = ctx.state.set_bridge(leg, None);
    }
    legs
}

/// `list_cdr` — bounded query over the CDR store.
///
/// Returns `{count, rows: [...]}` where each row is a
/// [`smiths_core::storage::CallDetailRecord`]. When no backend is
/// wired, `count` is 0 and `rows` is empty — operators tell the
/// difference from "genuinely no calls yet" by reading
/// `[storage] backend` off `config://current`.
pub struct ListCdrTool;

#[async_trait]
impl Tool for ListCdrTool {
    fn name(&self) -> &'static str {
        "list_cdr"
    }

    fn description(&self) -> &'static str {
        "List call-detail records. Optional filters: time range, From/To substring, result."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "since_unix":  { "type": "integer", "description": "Only CDRs where started_at_unix >= this." },
                "until_unix":  { "type": "integer", "description": "Only CDRs where started_at_unix <= this." },
                "from_like":   { "type": "string",  "description": "Case-insensitive substring match on From URI." },
                "to_like":     { "type": "string",  "description": "Case-insensitive substring match on To URI." },
                "result":      { "type": "string",  "description": "Exact match on result (e.g. 'answered')." },
                "limit":       { "type": "integer", "minimum": 1, "maximum": 1000,
                                  "description": "Max rows (default 100)." }
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let Some(store) = ctx.cdr.as_ref() else {
            return Ok(json!({"count": 0, "rows": []}));
        };
        let mut filter = smiths_core::storage::CdrFilter::new();
        filter.since_unix = args.get("since_unix").and_then(Value::as_i64);
        filter.until_unix = args.get("until_unix").and_then(Value::as_i64);
        filter.from_like = args
            .get("from_like")
            .and_then(Value::as_str)
            .map(str::to_owned);
        filter.to_like = args
            .get("to_like")
            .and_then(Value::as_str)
            .map(str::to_owned);
        filter.result = args
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(v) = args.get("limit").and_then(Value::as_u64) {
            filter.limit = u32::try_from(v)
                .map_err(|_| ToolError::InvalidArguments("`limit` must fit in u32".into()))?;
        }
        let rows = store
            .list(&filter)
            .map_err(|e| ToolError::Internal(format!("cdr list: {e}")))?;
        Ok(json!({ "count": rows.len(), "rows": rows }))
    }
}

/// Translate a `CallError` into a `ToolError` the adapters already
/// know how to wire.
fn map_call_error(e: smiths_core::call::CallError) -> ToolError {
    use smiths_core::call::CallError;
    match e {
        CallError::InvalidTarget(m) => ToolError::InvalidArguments(m),
        CallError::NotFound(m) => ToolError::NotFound(m),
        CallError::Rejected { status, reason } => {
            ToolError::Internal(format!("peer rejected: {status} {reason}"))
        }
        CallError::Timeout { millis } => ToolError::Internal(format!("timeout after {millis} ms")),
        CallError::Internal(m) => ToolError::Internal(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::TestEngine;
    use smiths_core::EndpointId;
    use smiths_core::call::{CallError, CallOriginator};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    fn leg(n: u64) -> (EndpointId, SocketAddr) {
        (
            EndpointId(n),
            format!("10.0.0.{n}:4000").parse().expect("addr"),
        )
    }

    struct StubOriginator {
        placed: Mutex<Vec<String>>,
        hung_up: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl CallOriginator for StubOriginator {
        async fn place_call(&self, target: &str) -> Result<String, CallError> {
            if target.starts_with("sip:busy@") {
                return Err(CallError::Rejected {
                    status: 486,
                    reason: "Busy Here".into(),
                });
            }
            self.placed.lock().unwrap().push(target.to_owned());
            Ok(format!("out-{}", self.placed.lock().unwrap().len()))
        }
        async fn hangup(&self, call_id: &str) -> Result<(), CallError> {
            if call_id == "ghost" {
                return Err(CallError::NotFound(call_id.into()));
            }
            self.hung_up.lock().unwrap().push(call_id.to_owned());
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_calls_filters_by_phase() {
        let engine = TestEngine::new();
        assert_eq!(
            ListCallsTool.call(json!({}), &engine.ctx).await.unwrap()["count"],
            0
        );
        engine.dialog_created("live-1", None).await;
        engine.dialog_created("done-1", None).await;
        engine.dialog_terminated("done-1").await;

        let all = ListCallsTool.call(json!({}), &engine.ctx).await.unwrap();
        assert_eq!(all["count"], 2);
        let live = ListCallsTool
            .call(json!({"phase": "live"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(live["count"], 1);
        assert_eq!(live["calls"][0]["call_id"], "live-1");
        let done = ListCallsTool
            .call(json!({"phase": "terminated"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(done["count"], 1);
        assert_eq!(done["calls"][0]["call_id"], "done-1");
        assert_eq!(done["calls"][0]["phase"], "terminated");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_call_status_returns_snapshot_or_not_found() {
        let engine = TestEngine::new();
        let err = GetCallStatusTool
            .call(json!({"call_id": "nope"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        engine.dialog_created("c1", Some(leg(1))).await;
        let out = GetCallStatusTool
            .call(json!({"call_id": "c1"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(out["call_id"], "c1");
        assert_eq!(out["phase"], "live");
        assert!(out["started_at"].is_number());
        // Internal media fields never leak onto the wire.
        assert!(out.get("media_endpoint").is_none());
        assert!(out.get("bridge_id").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_counts_live_calls() {
        let engine = TestEngine::new();
        engine.dialog_created("a", None).await;
        engine.dialog_created("b", None).await;
        engine.dialog_terminated("b").await;
        let out = HealthTool.call(json!({}), &engine.ctx).await.unwrap();
        assert_eq!(out["status"], "ok");
        assert_eq!(out["live_calls"], 1);
        assert_eq!(out["known_calls"], 2);
        assert!(out["uptime_secs"].is_number());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn make_and_end_call_without_originator_is_not_found() {
        let engine = TestEngine::new();
        let err = MakeCallTool
            .call(json!({"target": "sip:a@127.0.0.1"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        let err = EndCallTool
            .call(json!({"call_id": "x"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn make_and_end_call_drive_the_originator() {
        let engine = TestEngine::new();
        let originator = Arc::new(StubOriginator {
            placed: Mutex::new(Vec::new()),
            hung_up: Mutex::new(Vec::new()),
        });
        let ctx = engine.ctx.clone().with_originator(originator.clone());

        let out = MakeCallTool
            .call(json!({"target": "sip:alice@10.0.0.2:5060"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out["call_id"], "out-1");
        assert_eq!(out["target"], "sip:alice@10.0.0.2:5060");
        assert_eq!(
            originator.placed.lock().unwrap().as_slice(),
            ["sip:alice@10.0.0.2:5060"]
        );

        let rejected = MakeCallTool
            .call(json!({"target": "sip:busy@10.0.0.3"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(rejected, ToolError::Internal(m) if m.contains("486")));

        let ended = EndCallTool
            .call(json!({"call_id": "out-1"}), &ctx)
            .await
            .unwrap();
        assert_eq!(ended["status"], "ended");
        assert_eq!(originator.hung_up.lock().unwrap().as_slice(), ["out-1"]);

        let ghost = EndCallTool
            .call(json!({"call_id": "ghost"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(ghost, ToolError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_calls_installs_a_bridge_and_records_it_on_both_legs() {
        let engine = TestEngine::new();
        engine.dialog_created("a", Some(leg(1))).await;
        engine.dialog_created("b", Some(leg(2))).await;

        let out = BridgeCallsTool
            .call(json!({"call_id_a": "a", "call_id_b": "b"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(out["status"], "bridged");
        let bridge_id = out["bridge_id"].as_u64().unwrap();
        assert_eq!(out["released_bridges"], json!([]));

        let bridges = engine.fabric.bridges();
        assert_eq!(bridges.len(), 1);
        let (id, first_leg, second_leg) = &bridges[0];
        assert_eq!(id.0, bridge_id);
        assert_eq!((first_leg.endpoint, first_leg.peer), leg(1));
        assert_eq!((second_leg.endpoint, second_leg.peer), leg(2));
        assert!(first_leg.srtp.is_none() && second_leg.srtp.is_none());

        for call in ["a", "b"] {
            assert_eq!(
                engine.ctx.state.get_call(call).unwrap().bridge_id,
                Some(BridgeId(bridge_id))
            );
        }
        let status = GetCallStatusTool
            .call(json!({"call_id": "a"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(status["bridge_id"], bridge_id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rebridging_releases_the_previous_bridge_on_either_leg() {
        let engine = TestEngine::new();
        engine.dialog_created("a", Some(leg(1))).await;
        engine.dialog_created("b", Some(leg(2))).await;
        engine.dialog_created("c", Some(leg(3))).await;

        let first = BridgeCallsTool
            .call(json!({"call_id_a": "a", "call_id_b": "b"}), &engine.ctx)
            .await
            .unwrap();
        let first_id = first["bridge_id"].as_u64().unwrap();

        // Move `a` over to `c`: the a↔b bridge must go first.
        let second = BridgeCallsTool
            .call(json!({"call_id_a": "c", "call_id_b": "a"}), &engine.ctx)
            .await
            .unwrap();
        let second_id = second["bridge_id"].as_u64().unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(second["released_bridges"], json!([first_id]));
        assert_eq!(engine.fabric.released(), vec![BridgeId(first_id)]);
        assert_eq!(engine.ctx.state.get_call("b").unwrap().bridge_id, None);
        assert_eq!(
            engine.ctx.state.get_call("a").unwrap().bridge_id,
            Some(BridgeId(second_id))
        );
        assert_eq!(
            engine.ctx.state.get_call("c").unwrap().bridge_id,
            Some(BridgeId(second_id))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bridge_calls_rejects_unknown_media_less_and_identical_legs() {
        let engine = TestEngine::new();
        engine.dialog_created("a", Some(leg(1))).await;
        engine.dialog_created("silent", None).await;

        let err = BridgeCallsTool
            .call(json!({"call_id_a": "a", "call_id_b": "zzz"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "{err}");
        let err = BridgeCallsTool
            .call(
                json!({"call_id_a": "a", "call_id_b": "silent"}),
                &engine.ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Conflict(_)), "{err}");
        let err = BridgeCallsTool
            .call(json!({"call_id_a": "a", "call_id_b": "a"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)), "{err}");
        assert!(engine.fabric.bridges().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unbridge_call_releases_both_legs() {
        let engine = TestEngine::new();
        engine.dialog_created("a", Some(leg(1))).await;
        engine.dialog_created("b", Some(leg(2))).await;
        let bridged = BridgeCallsTool
            .call(json!({"call_id_a": "a", "call_id_b": "b"}), &engine.ctx)
            .await
            .unwrap();
        let id = bridged["bridge_id"].as_u64().unwrap();

        let out = UnbridgeCallTool
            .call(json!({"call_id": "b"}), &engine.ctx)
            .await
            .unwrap();
        assert_eq!(out["status"], "released");
        assert_eq!(out["bridge_id"], id);
        let mut legs: Vec<&str> = out["released_legs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        legs.sort_unstable();
        assert_eq!(legs, ["a", "b"]);
        assert_eq!(engine.fabric.released(), vec![BridgeId(id)]);
        assert_eq!(engine.ctx.state.get_call("a").unwrap().bridge_id, None);

        // Second release is a conflict, unknown call is not found.
        let err = UnbridgeCallTool
            .call(json!({"call_id": "a"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Conflict(_)), "{err}");
        let err = UnbridgeCallTool
            .call(json!({"call_id": "nope"}), &engine.ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_cdr_without_store_is_empty() {
        let engine = TestEngine::new();
        let out = ListCdrTool.call(json!({}), &engine.ctx).await.unwrap();
        assert_eq!(out["count"], 0);
    }
}
