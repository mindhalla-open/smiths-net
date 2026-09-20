//! Concrete tool implementations shipped in-box.
//!
//! Every tool here is served identically by the MCP, A2A, and
//! webhook adapters. Tools are grouped by the engine subsystem they
//! drive; new tools land in the matching module (or a new one) so no
//! single file grows past a screenful of tools.
//!
//! * [`call`] — dialog inventory, outbound calls, media bridging.
//! * [`media`] — audio injection into a live leg (`speak`, DTMF,
//!   prompt recording) plus the RTP framing they share.
//! * [`conference`] — N-party mixer create / join / leave.
//! * [`ai`] — direct invocation of one named AI plugin.
//! * [`pipeline`] — composite AI tools routed through the shared
//!   [`smiths_core::AiDispatcher`].
//! * [`plugin`] — plugin inventory and lifecycle.
//! * [`metrics`] — Prometheus registry introspection.
//! * [`config`] — live config read / patch.

use std::net::SocketAddr;

use serde_json::Value;
use smiths_core::EndpointId;

use crate::control::{CallPhase, CallSnapshot};
use crate::tool::{ToolContext, ToolError};

pub mod ai;
pub mod call;
pub mod conference;
pub mod config;
pub mod media;
pub mod metrics;
pub mod pipeline;
pub mod plugin;

pub use ai::{EmbedTool, LlmChatTool, SynthesizeTool, TranscribeTool};
pub use call::{
    BridgeCallsTool, EndCallTool, GetCallStatusTool, HealthTool, ListCallsTool, ListCdrTool,
    MakeCallTool, UnbridgeCallTool,
};
pub use conference::{CreateConferenceTool, JoinConferenceTool, LeaveConferenceTool};
pub use media::{RecordPromptTool, SendDtmfTool, SpeakTool};
pub use metrics::{GetMetricTool, ListMetricsTool};
pub use pipeline::{
    ListStylePresetsTool, RestyleCallTool, SearchCallsSemanticTool, SummarizeCallTool,
    TranscribeCallTool, TranslateTool,
};
pub use plugin::{DescribeProviderTool, ListAiProvidersTool, PutScriptTool, ReloadPluginTool};

/// Build a fully-populated [`crate::ToolRegistry`] with the built-in
/// tool set.
#[must_use]
pub fn builtin_registry() -> crate::ToolRegistry {
    let mut reg = crate::ToolRegistry::new();
    reg.register(ListCallsTool);
    reg.register(GetCallStatusTool);
    reg.register(HealthTool);
    reg.register(MakeCallTool);
    reg.register(EndCallTool);
    reg.register(BridgeCallsTool);
    reg.register(UnbridgeCallTool);
    reg.register(ListCdrTool);
    reg.register(SpeakTool);
    reg.register(SendDtmfTool);
    reg.register(RecordPromptTool);
    reg.register(CreateConferenceTool);
    reg.register(JoinConferenceTool);
    reg.register(LeaveConferenceTool);
    reg.register(ListAiProvidersTool);
    reg.register(DescribeProviderTool);
    reg.register(ReloadPluginTool);
    reg.register(PutScriptTool);
    reg.register(SynthesizeTool);
    reg.register(TranscribeTool);
    reg.register(LlmChatTool);
    reg.register(EmbedTool);
    reg.register(TranslateTool);
    reg.register(TranscribeCallTool);
    reg.register(SummarizeCallTool);
    reg.register(ListStylePresetsTool);
    reg.register(RestyleCallTool);
    reg.register(SearchCallsSemanticTool);
    reg.register(ListMetricsTool);
    reg.register(GetMetricTool);
    reg.register(config::GetConfigTool);
    reg.register(config::PutConfigTool);
    reg
}

/// Pull a required string argument.
pub(crate) fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments(format!("{key} required")))
}

/// Pull a required unsigned-integer argument.
pub(crate) fn require_u64(args: &Value, key: &str) -> Result<u64, ToolError> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError::InvalidArguments(format!("{key} required")))
}

/// Resolve `call_id` to a live dialog with a media leg. `NotFound`
/// when the engine doesn't know the call; `Conflict` when it does
/// but the call is over or never negotiated media.
pub(crate) fn live_media_leg(
    ctx: &ToolContext,
    call_id: &str,
) -> Result<(CallSnapshot, EndpointId, SocketAddr), ToolError> {
    let snap = ctx
        .state
        .get_call(call_id)
        .ok_or_else(|| ToolError::NotFound(format!("call {call_id}")))?;
    if !matches!(snap.phase, CallPhase::Live) {
        return Err(ToolError::Conflict(format!(
            "call {call_id} is not live (phase = {:?})",
            snap.phase
        )));
    }
    let endpoint = snap
        .media_endpoint
        .ok_or_else(|| ToolError::Conflict(format!("call {call_id} has no media endpoint")))?;
    let remote = snap
        .remote_rtp
        .ok_or_else(|| ToolError::Conflict(format!("call {call_id} has no remote RTP address")))?;
    Ok((snap, endpoint, remote))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::TestEngine;

    #[test]
    fn registry_contains_builtins() {
        let reg = builtin_registry();
        let expected = [
            "list_calls",
            "get_call_status",
            "health",
            "make_call",
            "end_call",
            "bridge_calls",
            "unbridge_call",
            "list_cdr",
            "speak",
            "send_dtmf",
            "record_prompt",
            "create_conference",
            "join_conference",
            "leave_conference",
            "list_ai_providers",
            "describe_provider",
            "reload_plugin",
            "put_script",
            "synthesize",
            "transcribe",
            "llm_chat",
            "embed",
            "translate",
            "transcribe_call",
            "summarize_call",
            "list_style_presets",
            "restyle_call",
            "search_calls_semantic",
            "list_metrics",
            "get_metric",
            "get_config",
            "put_config",
        ];
        assert_eq!(reg.len(), expected.len());
        for name in expected {
            assert!(reg.get(name).is_some(), "missing tool: {name}");
        }
    }

    #[test]
    fn every_tool_declares_an_object_schema() {
        for tool in builtin_registry().iter() {
            let schema = tool.input_schema();
            assert_eq!(schema["type"], "object", "{}: {schema}", tool.name());
            assert!(!tool.description().is_empty(), "{}", tool.name());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_media_leg_distinguishes_unknown_dead_and_media_less_calls() {
        let engine = TestEngine::new();
        let leg = (EndpointId(1), "127.0.0.1:4000".parse().unwrap());
        engine.dialog_created("with-media", Some(leg)).await;
        engine.dialog_created("no-media", None).await;
        engine.dialog_created("gone", Some(leg)).await;
        engine.dialog_terminated("gone").await;

        assert!(matches!(
            live_media_leg(&engine.ctx, "unknown"),
            Err(ToolError::NotFound(_))
        ));
        assert!(matches!(
            live_media_leg(&engine.ctx, "no-media"),
            Err(ToolError::Conflict(_))
        ));
        assert!(matches!(
            live_media_leg(&engine.ctx, "gone"),
            Err(ToolError::Conflict(_))
        ));
        let (_, ep, remote) = live_media_leg(&engine.ctx, "with-media").unwrap();
        assert_eq!((ep, remote), leg);
    }
}
