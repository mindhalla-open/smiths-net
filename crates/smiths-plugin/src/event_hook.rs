//! Auto-invoke plugins on call-lifecycle events.
//!
//! A small bus consumer: when a SIP dialog goes live
//! (`SipEvent::DialogCreated`), it invokes the `on_dialog_created`
//! method on each configured plugin via the [`AiRegistry`]. This lets
//! a "call-control brain" plugin (e.g. `sip-client`) react to inbound
//! calls without an explicit MCP request.
//!
//! It reuses the existing broadcast bus + `AiRegistry::invoke` rather
//! than introducing a new hook-dispatch system. Which plugins to
//! notify is config-driven (`[plugins] call_event_hooks = [...]`), so
//! the engine only spawns the consumer when at least one is listed.

use std::sync::Arc;

use serde_json::json;
use smiths_core::ai::AiRegistry;
use smiths_core::{Event, EventBus, SipEvent};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Spawn the call-event consumer. On each `DialogCreated`, invokes
/// `on_dialog_created` (params `{call_id, remote_rtp}`) on every plugin
/// named in `plugins`. If `plugins` is empty the task exits immediately
/// — callers can skip the spawn entirely, but guarding here keeps the
/// call site simple.
#[must_use]
pub fn spawn_call_event_hooks(
    bus: &EventBus,
    registry: Arc<dyn AiRegistry>,
    plugins: Vec<String>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        if plugins.is_empty() {
            return;
        }
        debug!(?plugins, "call-event hook consumer started");
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                event = rx.recv() => match event {
                    Ok(Event::Sip(SipEvent::DialogCreated { call_id, remote_rtp, .. })) => {
                        let params = json!({
                            "call_id": call_id,
                            "remote_rtp": remote_rtp.map(|a| a.to_string()),
                        });
                        for name in &plugins {
                            let Some(provider) = registry.get(name) else {
                                continue;
                            };
                            match provider.invoke("on_dialog_created", params.clone()).await {
                                Ok(_) => {
                                    debug!(plugin = %name, %call_id, "on_dialog_created delivered");
                                }
                                Err(e) => {
                                    warn!(plugin = %name, %call_id, ?e, "on_dialog_created failed");
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        debug!(lagged = n, "call-event hook consumer lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        debug!("call-event hook consumer exiting");
    })
}
