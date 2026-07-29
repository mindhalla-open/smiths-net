//! Live engine state consumed by control-plane tools.
//!
//! Subscribes to the SIP event bus and maintains a concurrent registry
//! of dialogs. Tools receive a `ControlState` handle through
//! [`ToolContext`] and query it without caring how MCP or A2A delivered
//! the request.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use dashmap::DashMap;
use serde::Serialize;
use smiths_core::media::EndpointId;
use smiths_core::{CallLookup, Event, EventBus, SipEvent};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Coarse call lifecycle phase. Strings map 1:1 to the MCP/A2A wire.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CallPhase {
    /// Dialog created (200 OK sent), awaiting ACK or first BYE.
    Live,
    /// Dialog terminated normally.
    Terminated,
}

/// Public, serialization-friendly snapshot of one call.
///
/// This is what `list_calls` / `get_call_status` return.
#[derive(Clone, Debug, Serialize)]
pub struct CallSnapshot {
    /// SIP `Call-ID`.
    pub call_id: String,
    /// Lifecycle phase at snapshot time.
    pub phase: CallPhase,
    /// Unix seconds the dialog was created.
    pub started_at: u64,
    /// Unix seconds the dialog ended, if it has.
    pub ended_at: Option<u64>,
    /// Engine-allocated media endpoint ID for this dialog, if it
    /// carries media. `None` for signaling-only calls or any dialog
    /// where SDP negotiation didn't complete. Internal-use field; the
    /// `speak` tool uses this to locate the call's audio leg.
    #[serde(skip)]
    pub media_endpoint: Option<EndpointId>,
    /// Peer's RTP endpoint learned from the SDP offer, if any.
    /// Same caveats as `media_endpoint`.
    #[serde(skip)]
    pub remote_rtp: Option<SocketAddr>,
}

/// Shared control-plane state. Cheap to clone.
#[derive(Clone)]
pub struct ControlState {
    calls: Arc<DashMap<String, CallSnapshot>>,
    started_at: u64,
}

impl ControlState {
    /// Build an empty state and spawn a background task that drains
    /// `bus` until `cancel` fires. The returned handle is detached —
    /// cancel via `cancel`, then await if you need to join.
    #[must_use]
    pub fn spawn(bus: &EventBus, cancel: CancellationToken) -> (Self, JoinHandle<()>) {
        let state = Self {
            calls: Arc::new(DashMap::new()),
            started_at: unix_seconds(),
        };
        let task = {
            let state = state.clone();
            let mut rx = bus.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        event = rx.recv() => match event {
                            Ok(Event::Sip(SipEvent::DialogCreated {
                                call_id,
                                media_endpoint,
                                remote_rtp,
                                ..
                            })) => {
                                state.calls.insert(
                                    call_id.clone(),
                                    CallSnapshot {
                                        call_id,
                                        phase: CallPhase::Live,
                                        started_at: unix_seconds(),
                                        ended_at: None,
                                        media_endpoint,
                                        remote_rtp,
                                    },
                                );
                            }
                            Ok(Event::Sip(SipEvent::DialogTerminated { call_id })) => {
                                if let Some(mut entry) = state.calls.get_mut(&call_id) {
                                    entry.phase = CallPhase::Terminated;
                                    entry.ended_at = Some(unix_seconds());
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                debug!("control-state bus lagged: {n} events dropped");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                    }
                }
                debug!("control-state drain task exiting");
            })
        };
        (state, task)
    }

    /// Snapshot all calls, live and terminated. Terminated calls
    /// eventually age out (see [`Self::purge_terminated_older_than`]).
    #[must_use]
    pub fn list_calls(&self) -> Vec<CallSnapshot> {
        self.calls.iter().map(|e| e.value().clone()).collect()
    }

    /// Fetch one call by `Call-ID`.
    #[must_use]
    pub fn get_call(&self, call_id: &str) -> Option<CallSnapshot> {
        self.calls.get(call_id).map(|e| e.value().clone())
    }

    /// Remove terminated calls whose `ended_at` is older than `now - ttl_secs`.
    /// Live calls are never purged.
    #[must_use]
    pub fn purge_terminated_older_than(&self, ttl_secs: u64) -> usize {
        let cutoff = unix_seconds().saturating_sub(ttl_secs);
        let mut removed = 0;
        self.calls.retain(|_, v| {
            if v.phase == CallPhase::Terminated
                && let Some(ended) = v.ended_at
                && ended < cutoff
            {
                removed += 1;
                return false;
            }
            true
        });
        removed
    }

    /// Process-start timestamp, used by the `health` tool.
    #[must_use]
    pub const fn started_at(&self) -> u64 {
        self.started_at
    }

    /// Seconds since the process started, clamped to `>= 0`.
    #[must_use]
    pub fn uptime_secs(&self) -> u64 {
        unix_seconds().saturating_sub(self.started_at)
    }
}

impl CallLookup for ControlState {
    fn endpoint_for(&self, call_id: &str) -> Option<(EndpointId, SocketAddr)> {
        let snap = self.calls.get(call_id)?;
        let endpoint = snap.media_endpoint?;
        let remote = snap.remote_rtp?;
        Some((endpoint, remote))
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_core::{Event, EventBus, SipEvent};
    use tokio::time::{Duration, sleep};

    #[tokio::test(flavor = "multi_thread")]
    async fn tracks_dialog_lifecycle() {
        let bus = EventBus::new(16);
        let cancel = CancellationToken::new();
        let (state, _task) = ControlState::spawn(&bus, cancel.clone());
        sleep(Duration::from_millis(10)).await; // let subscriber arm

        bus.publish(Event::Sip(SipEvent::DialogCreated {
            call_id: "call-1".into(),
            from_uri: None,
            media_endpoint: None,
            remote_rtp: None,
        }))
        .unwrap();
        sleep(Duration::from_millis(20)).await;
        assert_eq!(state.list_calls().len(), 1);
        assert_eq!(state.get_call("call-1").unwrap().phase, CallPhase::Live);

        bus.publish(Event::Sip(SipEvent::DialogTerminated {
            call_id: "call-1".into(),
        }))
        .unwrap();
        sleep(Duration::from_millis(20)).await;
        assert_eq!(
            state.get_call("call-1").unwrap().phase,
            CallPhase::Terminated
        );

        cancel.cancel();
    }
}
