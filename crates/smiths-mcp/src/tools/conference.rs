//! N-party mixer control: `create_conference`, `join_conference`,
//! `leave_conference`.
//!
//! `join_conference` attaches a live call's media leg to the mixer's
//! egress: an engine task drains the participant's mixed frames and
//! streams them to the caller's RTP endpoint through the media
//! fabric. Ingress (the caller's RTP into the mixer) is the SIP
//! layer's job — the UAS routes conference-room INVITEs through the
//! mixer fabric, which owns the receiving socket; the control plane
//! has no receive hook on [`smiths_core::MediaFabric`].

use std::net::SocketAddr;

use async_trait::async_trait;
use serde_json::{Value, json};
use smiths_core::EndpointId;
use smiths_mixer::{ConferenceId, ParticipantFrame, ParticipantId};
use tokio::sync::mpsc::Receiver;
use tracing::{debug, warn};

use super::media::{PT_PCMU, RtpStream};
use super::{live_media_leg, require_u64};
use crate::control::CallPhase;
use crate::tool::{Tool, ToolContext, ToolError};

/// Consecutive `send_packet` failures after which the egress task
/// gives up and detaches the participant.
const MAX_SEND_FAILURES: u32 = 5;

/// `create_conference` — allocate a fresh N-participant mixer.
///
/// Returns the new conference id. Subsequent `join_conference` calls
/// attach participants.
pub struct CreateConferenceTool;

#[async_trait]
impl Tool for CreateConferenceTool {
    fn name(&self) -> &'static str {
        "create_conference"
    }

    fn description(&self) -> &'static str {
        "Create a new audio conference (leave-one-out N:N mixer with \
         per-participant AGC + VAD-based dominant-speaker selection). \
         Returns the conference id to pass to `join_conference`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn call(&self, _args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let reg = ctx.conferences.as_ref().ok_or_else(|| {
            ToolError::NotFound("no conference registry wired; enable the mixer fabric".into())
        })?;
        let id = reg.create(smiths_mixer::ConferenceConfig::default()).await;
        Ok(json!({ "conference_id": id.0 }))
    }
}

/// `join_conference(conference_id, call_id?)` — attach a participant.
///
/// With `call_id`, the participant's mixed audio is streamed to that
/// live call's RTP endpoint until the participant leaves, the
/// conference shuts down, or the call ends. Without it the mixer
/// slot exists but nobody hears its output.
pub struct JoinConferenceTool;

#[async_trait]
impl Tool for JoinConferenceTool {
    fn name(&self) -> &'static str {
        "join_conference"
    }

    fn description(&self) -> &'static str {
        "Attach a participant to an existing conference. With `call_id`, \
         the mixed audio is streamed as PCMU RTP to that live call's \
         media leg until the participant leaves or the call ends. \
         Returns the participant id."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "conference_id": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Id returned by `create_conference`."
                },
                "call_id": {
                    "type": "string",
                    "description": "Live call whose media leg receives the mix."
                }
            },
            "required": ["conference_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let conf_id = require_u64(&args, "conference_id")?;
        let reg = ctx.conferences.as_ref().ok_or_else(|| {
            ToolError::NotFound("no conference registry wired; enable the mixer fabric".into())
        })?;
        // Resolve the media leg before touching the mixer so a bad
        // call id never leaves an orphaned participant behind.
        let leg = match args.get("call_id").and_then(Value::as_str) {
            Some(call_id) => {
                let (_, endpoint, remote) = live_media_leg(ctx, call_id)?;
                Some((call_id.to_owned(), endpoint, remote))
            }
            None => None,
        };
        let (pid, egress) = reg
            .join(ConferenceId(conf_id))
            .await
            .map_err(map_conference_error)?;

        let egress_mode = if let Some((call_id, endpoint, remote)) = leg {
            spawn_egress(
                ctx.clone(),
                ConferenceId(conf_id),
                pid,
                call_id,
                endpoint,
                remote,
                egress,
            );
            "rtp"
        } else {
            debug!(
                conference = conf_id,
                participant = pid.0,
                "join without call_id: no egress sink"
            );
            "none"
        };
        Ok(json!({
            "conference_id": conf_id,
            "participant_id": pid.0,
            "call_id": args.get("call_id"),
            "egress": egress_mode,
        }))
    }
}

/// Drain the participant's mixed frames and stream them to the
/// call's RTP endpoint. Exits when the mixer closes the channel
/// (leave / shutdown), when the call stops being live, or after
/// repeated send failures; in the latter two cases it also detaches
/// the participant so the mixer stops producing for a dead sink.
fn spawn_egress(
    ctx: ToolContext,
    conference: ConferenceId,
    participant: ParticipantId,
    call_id: String,
    endpoint: EndpointId,
    remote: SocketAddr,
    mut egress: Receiver<ParticipantFrame>,
) {
    tokio::spawn(async move {
        let mut stream = RtpStream::new();
        let mut failures: u32 = 0;
        let detach = loop {
            let Some(frame) = egress.recv().await else {
                break false;
            };
            let live = ctx
                .state
                .get_call(&call_id)
                .is_some_and(|c| matches!(c.phase, CallPhase::Live));
            if !live {
                debug!(%call_id, participant = participant.0, "call ended; detaching participant");
                break true;
            }
            let payload = smiths_core::pcm16_to_pcmu(&frame.samples);
            let ticks = u32::try_from(frame.samples.len()).unwrap_or(u32::MAX);
            let pkt = stream.next_packet(PT_PCMU, payload, ticks);
            match ctx.media.send_packet(endpoint, remote, &pkt.encode()).await {
                Ok(()) => failures = 0,
                Err(e) => {
                    failures += 1;
                    warn!(%call_id, error = %e, failures, "conference egress send failed");
                    stream.mark_gap();
                    if failures >= MAX_SEND_FAILURES {
                        break true;
                    }
                }
            }
        };
        if detach && let Some(reg) = ctx.conferences.as_ref() {
            let _ = reg.leave(conference, participant).await;
        }
    });
}

/// `leave_conference(conference_id, participant_id)` — detach a
/// participant; closes their ingress/egress channels, which also
/// stops any RTP egress task `join_conference` started.
pub struct LeaveConferenceTool;

#[async_trait]
impl Tool for LeaveConferenceTool {
    fn name(&self) -> &'static str {
        "leave_conference"
    }

    fn description(&self) -> &'static str {
        "Detach a participant from a conference. Unknown participant \
         ids surface as a clean `NotFound`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "conference_id": { "type": "integer", "minimum": 0 },
                "participant_id": { "type": "integer", "minimum": 0 }
            },
            "required": ["conference_id", "participant_id"],
            "additionalProperties": false
        })
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        let conf_id = require_u64(&args, "conference_id")?;
        let part_id = require_u64(&args, "participant_id")?;
        let reg = ctx.conferences.as_ref().ok_or_else(|| {
            ToolError::NotFound("no conference registry wired; enable the mixer fabric".into())
        })?;
        reg.leave(ConferenceId(conf_id), ParticipantId(part_id))
            .await
            .map_err(map_conference_error)?;
        Ok(json!({
            "conference_id": conf_id,
            "participant_id": part_id,
            "status": "left",
        }))
    }
}

fn map_conference_error(e: smiths_mixer::ConferenceRegistryError) -> ToolError {
    use smiths_mixer::ConferenceRegistryError as E;
    match e {
        E::UnknownConference(id) => ToolError::NotFound(format!("unknown conference: {id}")),
        E::Conference(c) => match c {
            smiths_mixer::ConferenceError::UnknownParticipant(p) => {
                ToolError::NotFound(format!("unknown participant: {p}"))
            }
            other => ToolError::Internal(other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_support::TestEngine;
    use smiths_core::RtpPacket;
    use smiths_mixer::{ConferenceRegistry, InMemoryConferenceRegistry};
    use std::sync::Arc;
    use std::time::Duration;

    fn leg() -> (EndpointId, SocketAddr) {
        (EndpointId(9), "127.0.0.1:4002".parse().unwrap())
    }

    fn with_mixer(engine: &TestEngine) -> (ToolContext, Arc<InMemoryConferenceRegistry>) {
        let reg = Arc::new(InMemoryConferenceRegistry::new());
        let dyn_reg: Arc<dyn ConferenceRegistry> = reg.clone();
        (engine.ctx.clone().with_conferences(dyn_reg), reg)
    }

    /// Poll the fake fabric until at least `n` packets were sent.
    async fn wait_for_packets(engine: &TestEngine, n: usize) {
        for _ in 0..200 {
            if engine.fabric.sent().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} egress packets, got {}",
            engine.fabric.sent().len()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conference_tools_without_registry_are_not_found() {
        let engine = TestEngine::new();
        assert!(matches!(
            CreateConferenceTool.call(json!({}), &engine.ctx).await,
            Err(ToolError::NotFound(_))
        ));
        assert!(matches!(
            JoinConferenceTool
                .call(json!({"conference_id": 0}), &engine.ctx)
                .await,
            Err(ToolError::NotFound(_))
        ));
        assert!(matches!(
            LeaveConferenceTool
                .call(
                    json!({"conference_id": 0, "participant_id": 0}),
                    &engine.ctx
                )
                .await,
            Err(ToolError::NotFound(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn join_streams_mixed_frames_to_the_call_leg_until_leave() {
        let engine = TestEngine::new();
        let (ctx, reg) = with_mixer(&engine);
        let (endpoint, remote) = leg();
        engine.dialog_created("c1", Some((endpoint, remote))).await;

        let conf = CreateConferenceTool.call(json!({}), &ctx).await.unwrap();
        let conf_id = conf["conference_id"].as_u64().unwrap();
        let joined = JoinConferenceTool
            .call(json!({"conference_id": conf_id, "call_id": "c1"}), &ctx)
            .await
            .unwrap();
        assert_eq!(joined["egress"], "rtp");
        assert_eq!(joined["call_id"], "c1");
        let pid = joined["participant_id"].as_u64().unwrap();

        // A second participant talks; the mixer's tick delivers the
        // leave-one-out mix to c1, which the egress task frames as RTP.
        let (talker, _talker_rx) = reg.join(ConferenceId(conf_id)).await.unwrap();
        for _ in 0..5 {
            let _ = reg
                .push_frame(ConferenceId(conf_id), talker, vec![4000; 160])
                .await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wait_for_packets(&engine, 3).await;

        let sent = engine.fabric.sent();
        let packets: Vec<RtpPacket> = sent
            .iter()
            .map(|p| {
                assert_eq!(p.src, endpoint);
                assert_eq!(p.dest, remote);
                RtpPacket::decode(&p.bytes).expect("valid RTP")
            })
            .collect();
        assert!(packets[0].marker, "first egress packet carries the marker");
        assert!(!packets[1].marker);
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(p.payload_type, PT_PCMU);
            assert_eq!(p.payload.len(), 160);
            assert_eq!(p.ssrc, packets[0].ssrc);
            let i16_seq = u16::try_from(i).unwrap();
            assert_eq!(p.sequence, packets[0].sequence.wrapping_add(i16_seq));
            assert_eq!(
                p.timestamp,
                packets[0]
                    .timestamp
                    .wrapping_add(160 * u32::try_from(i).unwrap())
            );
        }

        // Leaving closes the egress channel; the task stops sending.
        let left = LeaveConferenceTool
            .call(
                json!({"conference_id": conf_id, "participant_id": pid}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(left["status"], "left");
        tokio::time::sleep(Duration::from_millis(60)).await;
        let after_leave = engine.fabric.sent().len();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            engine.fabric.sent().len(),
            after_leave,
            "egress kept sending after leave"
        );

        let err = LeaveConferenceTool
            .call(
                json!({"conference_id": conf_id, "participant_id": pid}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        reg.shutdown(ConferenceId(conf_id)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn egress_detaches_participant_when_the_call_ends() {
        let engine = TestEngine::new();
        let (ctx, reg) = with_mixer(&engine);
        engine.dialog_created("c1", Some(leg())).await;
        let conf_id = reg.create(smiths_mixer::ConferenceConfig::default()).await;
        JoinConferenceTool
            .call(json!({"conference_id": conf_id.0, "call_id": "c1"}), &ctx)
            .await
            .unwrap();
        wait_for_packets(&engine, 1).await;

        engine.dialog_terminated("c1").await;
        // The next mixer tick sees a dead call: the participant is
        // removed and no more packets flow.
        let handle = reg.get_conference(conf_id).unwrap();
        for _ in 0..100 {
            if handle.stats().await.participants == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(handle.stats().await.participants, 0);
        let n = engine.fabric.sent().len();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(engine.fabric.sent().len(), n);
        reg.shutdown(conf_id).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn join_rejects_bad_calls_without_creating_a_participant() {
        let engine = TestEngine::new();
        let (ctx, reg) = with_mixer(&engine);
        engine.dialog_created("silent", None).await;
        let conf_id = reg.create(smiths_mixer::ConferenceConfig::default()).await;

        let err = JoinConferenceTool
            .call(json!({"conference_id": conf_id.0, "call_id": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "{err}");
        let err = JoinConferenceTool
            .call(
                json!({"conference_id": conf_id.0, "call_id": "silent"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Conflict(_)), "{err}");
        let handle = reg.get_conference(conf_id).unwrap();
        assert_eq!(handle.stats().await.participants, 0);

        let err = JoinConferenceTool
            .call(json!({"conference_id": 999}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "{err}");

        // Without a call the slot exists but has no sink.
        let joined = JoinConferenceTool
            .call(json!({"conference_id": conf_id.0}), &ctx)
            .await
            .unwrap();
        assert_eq!(joined["egress"], "none");
        assert_eq!(handle.stats().await.participants, 1);
        reg.shutdown(conf_id).await.unwrap();
    }
}
