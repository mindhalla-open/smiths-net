//! Hot-reload file watcher for the plugins directory.
//!
//! Spawns a background task that watches the configured plugin root
//! for changes (via the `notify` crate) and calls
//! [`smiths_core::ai::AiRegistry::reload`] on the affected plugin
//! when its `plugin.toml` or entry file changes.
//!
//! ## Coalescing
//!
//! Editors frequently fire a burst of events for one logical save
//! (write → rename → chmod). The watcher debounces them with a
//! small trailing delay and one in-flight reload per plugin, so a
//! noisy save doesn't pile up N reloads.
//!
//! ## Shutdown
//!
//! The returned [`WatcherHandle`] cancels the background task on
//! drop — callers don't need to manage task joins explicitly.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::{Config, Event, EventKind, PollWatcher, RecursiveMode, Watcher};
use smiths_core::ai::AiRegistry as AiRegistryTrait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// How long to wait after the last event for a plugin before firing a
/// reload. Absorbs editor save bursts.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// Filesystem polling interval. Uses [`PollWatcher`] rather than
/// platform-specific backends (`FSEvents`, inotify) because those have
/// subtle quirks with tempdirs and network filesystems; polling at
/// 300 ms is deterministic, negligible overhead for a plugin dir.
const POLL_INTERVAL: Duration = Duration::from_millis(300);

/// Handle to a running watcher task. Dropping it cancels the task
/// and tears down the underlying `notify` watcher.
pub struct WatcherHandle {
    cancel: CancellationToken,
    /// `None` after [`Self::shutdown`] awaited it — kept so `Drop`
    /// doesn't try to double-cancel.
    task: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Cancel the task and await its exit. Call this on clean
    /// shutdown; `Drop` only cancels without waiting.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Spawn the watcher task.
///
/// `root` is the plugins directory (same one `load_plugins` scanned).
/// `registry` is the live [`AiRegistry`] whose `reload` method will
/// be called on each detected change. Caller-side cancellation
/// works via [`WatcherHandle::shutdown`] or a plain `drop`.
///
/// If the directory doesn't exist, the watcher logs and returns a
/// handle that does nothing — matches `load_plugins`'s fail-open
/// behaviour.
pub fn spawn<R>(root: &Path, registry: Arc<R>) -> WatcherHandle
where
    R: AiRegistryTrait + 'static,
{
    let root = root.to_path_buf();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<Event>>();

    // Build and arm the watcher synchronously before returning — if we
    // did this inside the spawned task, there'd be a window where the
    // caller could mutate `root` before `watcher.watch()` ran, and
    // `PollWatcher` would bake those changes into its initial snapshot
    // and never emit an event for them. Flaked the hot-reload test
    // under CI load.
    //
    // `with_compare_contents(true)` detects same-second edits that
    // mtime-only polling would miss — macOS HFS+ has 1-second mtime
    // resolution, so a quick save-reload cycle can look identical
    // under plain metadata comparison.
    let watcher = match PollWatcher::new(
        move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        },
        Config::default()
            .with_poll_interval(POLL_INTERVAL)
            .with_compare_contents(true),
    ) {
        Ok(mut w) => {
            if root.exists() {
                if let Err(err) = w.watch(&root, RecursiveMode::Recursive) {
                    warn!(?err, dir = %root.display(), "plugin watcher: failed to start watch");
                    let task = tokio::spawn(async move { task_cancel.cancelled().await });
                    return WatcherHandle {
                        cancel,
                        task: Some(task),
                    };
                }
                info!(dir = %root.display(), "plugin hot-reload watcher started");
                Some(w)
            } else {
                debug!(dir = %root.display(), "plugin watcher: dir missing, idle");
                // Keep the watcher around anyway so tests / callers
                // observe consistent behaviour; we just never call
                // `watch()` on it, so no events are produced.
                Some(w)
            }
        }
        Err(err) => {
            warn!(?err, "plugin watcher: failed to construct notify backend");
            let task = tokio::spawn(async move { task_cancel.cancelled().await });
            return WatcherHandle {
                cancel,
                task: Some(task),
            };
        }
    };

    let task = tokio::spawn(async move {
        // Move the watcher into the task so its poll thread lives as
        // long as the task does; `PollWatcher`'s Drop joins it.
        let _watcher = watcher;

        // Pending debounce timers, one per plugin name.
        let mut pending: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

        loop {
            tokio::select! {
                () = task_cancel.cancelled() => {
                    debug!("plugin watcher: cancellation received, draining");
                    for (_, h) in pending.drain() {
                        h.abort();
                    }
                    break;
                }
                Some(ev) = rx.recv() => {
                    let Ok(event) = ev else {
                        continue;
                    };
                    if !is_reload_worthy(&event) {
                        continue;
                    }
                    for path in &event.paths {
                        let Some(plugin) = plugin_name_for_path(&root, path) else {
                            continue;
                        };
                        debug!(%plugin, "scheduling debounced reload");
                        if let Some(existing) = pending.remove(&plugin) {
                            existing.abort();
                        }
                        let registry = Arc::clone(&registry);
                        let plugin_for_task = plugin.clone();
                        let handle = tokio::spawn(async move {
                            tokio::time::sleep(DEBOUNCE).await;
                            match registry.reload(&plugin_for_task).await {
                                Ok(()) => info!(plugin = %plugin_for_task, "hot reload succeeded"),
                                Err(err) => warn!(plugin = %plugin_for_task, %err, "hot reload failed"),
                            }
                        });
                        pending.insert(plugin, handle);
                    }
                }
            }
        }
    });

    WatcherHandle {
        cancel,
        task: Some(task),
    }
}

/// Decide whether a `notify` event warrants a reload. We accept
/// creates, modifies, renames, and removes — anything that could
/// change what `load_plugins` would observe. Access events (read-
/// only opens) are ignored.
fn is_reload_worthy(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Map a changed file path back to its plugin name. A plugin
/// directory is a direct child of `root`; the plugin name is the
/// directory's basename.
fn plugin_name_for_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let first = rel.components().next()?;
    match first {
        std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_name_extracted_from_child_path() {
        let root = std::path::PathBuf::from("/plugins");
        let file = std::path::PathBuf::from("/plugins/ai-tts/plugin.toml");
        assert_eq!(
            plugin_name_for_path(&root, &file).as_deref(),
            Some("ai-tts")
        );
    }

    #[test]
    fn unrelated_path_yields_none() {
        let root = std::path::PathBuf::from("/plugins");
        let file = std::path::PathBuf::from("/other/ai-tts/plugin.toml");
        assert!(plugin_name_for_path(&root, &file).is_none());
    }
}
