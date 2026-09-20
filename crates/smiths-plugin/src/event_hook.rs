//! Auto-invoke plugins on call-lifecycle events.
//!
//! A small bus consumer: when a SIP dialog goes live
//! (`SipEvent::DialogCreated`) or ends (`SipEvent::DialogTerminated`)
//! it dispatches the matching hook (`on_dialog_created` /
//! `on_dialog_terminated`) through the registry's
//! [`MemoryDispatcher`], so plugins run in `priority` order and a
//! slow one is cut off by the per-event budget instead of stalling
//! the rest.
//!
//! Which plugins receive an event comes from two places, both
//! resolved by plugin name at dispatch time (so a hot reload that
//! swaps the provider is transparent):
//!
//! - manifests: `hooks = ["on_dialog_created",...]`, registered by
//!   the loader;
//! - config: `[plugins] call_event_hooks = ["name",...]` registers
//!   `on_dialog_created` for each listed plugin at the default
//!   priority, whether or not its manifest declares it.

use serde_json::{Value, json};
use smiths_core::{Event, EventBus, SipEvent};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::dispatcher::{Dispatcher, MemoryDispatcher};
use crate::registry::AiRegistry;

/// Hook dispatched on `SipEvent::DialogCreated`.
pub const HOOK_DIALOG_CREATED: &str = "on_dialog_created";
/// Hook dispatched on `SipEvent::DialogTerminated`.
pub const HOOK_DIALOG_TERMINATED: &str = "on_dialog_terminated";

/// Spawn the call-event consumer. `plugins` is the config-level list
/// of plugin names to notify on `on_dialog_created` (in addition to
/// whatever manifests declare); it may be empty. The task runs until
/// `cancel` fires or the bus closes.
#[must_use]
pub fn spawn_call_event_hooks(
    bus: &EventBus,
    registry: &AiRegistry,
    plugins: Vec<String>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    for name in &plugins {
        registry.declare_config_hook(name, HOOK_DIALOG_CREATED);
    }
    let hooks = registry.hooks().clone();
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        debug!(config_plugins = ?plugins, "call-event hook consumer started");
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
                        dispatch(&hooks, HOOK_DIALOG_CREATED, &call_id, params).await;
                    }
                    Ok(Event::Sip(SipEvent::DialogTerminated { call_id })) => {
                        let params = json!({ "call_id": call_id });
                        dispatch(&hooks, HOOK_DIALOG_TERMINATED, &call_id, params).await;
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

/// Fan `params` out to every hook registered for `event` and log
/// each outcome. Failures (plugin error, budget cut-off, plugin not
/// loaded) are logged and never abort the consumer.
async fn dispatch(hooks: &MemoryDispatcher, event: &str, call_id: &str, params: Value) {
    for report in hooks.dispatch(event, params).await {
        match report.outcome {
            Ok(_) => debug!(
                hook = %report.hook, %call_id, duration_ms = report.duration.as_millis(),
                "{event} delivered"
            ),
            Err(e) => warn!(
                hook = %report.hook, %call_id, error = %e,
                "{event} failed"
            ),
        }
    }
}
