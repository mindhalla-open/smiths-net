//! Graceful shutdown: drain, hang up, bounded teardown.
//!
//! Order on SIGTERM / SIGINT / MCP stdin EOF:
//!
//! 1. Flip the [`Drain`](smiths_core::Drain) flag — new INVITEs get
//!    `503 Service Unavailable` + `Retry-After: 0`.
//! 2. Hang up every live dialog: UAC-originated calls through
//!    [`CallOriginator::hangup`], UAS dialogs through each
//!    listener's [`DialogHangup`] (in-dialog BYE + media release).
//! 3. Wait up to the drain window (`SMITHS_DRAIN_SECS`, else
//!    `sip.drain_timeout_secs` from the live config) for the dialog
//!    tables to empty.
//! 4. Cancel every task and join each one under a bound so a stuck
//!    task can never hold the process open.

use std::sync::Arc;
use std::time::Duration;

use smiths_core::call::CallOriginator;
use smiths_mcp::ControlState;
use smiths_mcp::control::CallPhase;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::sip_spawn::DialogHangup;

/// Longest the shutdown driver waits on any single task.
pub(crate) const JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Drain window: the `SMITHS_DRAIN_SECS` environment variable wins
/// over the configured `sip.drain_timeout_secs`; either may be `0`.
pub(crate) fn drain_window(env_override: Option<&str>, configured_secs: u64) -> Duration {
    let secs = env_override
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(configured_secs);
    Duration::from_secs(secs)
}

/// Await `handle` for at most [`JOIN_TIMEOUT`], aborting it on
/// expiry so teardown always makes progress.
pub(crate) async fn join_bounded<T>(what: &'static str, handle: JoinHandle<T>) {
    let abort = handle.abort_handle();
    match tokio::time::timeout(JOIN_TIMEOUT, handle).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) if err.is_cancelled() => {}
        Ok(Err(err)) => warn!(?err, what, "task panicked during shutdown"),
        Err(_) => {
            warn!(
                what,
                timeout_secs = JOIN_TIMEOUT.as_secs(),
                "task did not stop in time; aborting"
            );
            abort.abort();
        }
    }
}

/// Live-call handles the drain step hangs up.
pub(crate) struct DrainTargets {
    pub originator: Option<Arc<dyn CallOriginator>>,
    pub control: ControlState,
    pub listeners: Vec<Arc<dyn DialogHangup>>,
}

/// BYE every live dialog. Returns how many hang-ups were issued.
pub(crate) async fn hang_up_everything(targets: &DrainTargets) -> usize {
    let mut hung_up = 0;
    if let Some(originator) = &targets.originator {
        for call in targets.control.list_calls() {
            if call.phase != CallPhase::Live {
                continue;
            }
            // UAS dialogs return `NotFound` here and are handled by
            // their listener below.
            if originator.hangup(&call.call_id).await.is_ok() {
                hung_up += 1;
            }
        }
    }
    for listener in &targets.listeners {
        hung_up += listener.hangup_all().await;
    }
    hung_up
}

/// Poll the listeners until no dialog remains or `window` elapses.
/// Returns `true` when everything cleared.
pub(crate) async fn wait_for_dialogs_to_clear(
    listeners: &[Arc<dyn DialogHangup>],
    window: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining: usize = listeners.iter().map(|l| l.active_dialogs()).sum();
        if remaining == 0 {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            warn!(
                remaining,
                "drain window elapsed with dialogs still open; cancelling"
            );
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Run the drain step: hang up, then wait for the tables to empty.
pub(crate) async fn drain(targets: &DrainTargets, window: Duration) {
    let hung_up = hang_up_everything(targets).await;
    info!(
        hung_up,
        drain_secs = window.as_secs(),
        "drain: live dialogs hung up"
    );
    if window.is_zero() {
        return;
    }
    wait_for_dialogs_to_clear(&targets.listeners, window).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeListener {
        active: AtomicUsize,
    }

    #[async_trait]
    impl DialogHangup for FakeListener {
        async fn hangup_all(&self) -> usize {
            self.active.swap(0, Ordering::AcqRel)
        }
        fn active_dialogs(&self) -> usize {
            self.active.load(Ordering::Acquire)
        }
    }

    #[test]
    fn env_override_wins_over_config() {
        assert_eq!(drain_window(Some("0"), 10), Duration::ZERO);
        assert_eq!(drain_window(Some("3"), 10), Duration::from_secs(3));
        assert_eq!(drain_window(Some("junk"), 10), Duration::from_secs(10));
        assert_eq!(drain_window(None, 7), Duration::from_secs(7));
    }

    #[tokio::test]
    async fn wait_returns_immediately_when_nothing_is_open() {
        let l: Arc<dyn DialogHangup> = Arc::new(FakeListener {
            active: AtomicUsize::new(0),
        });
        let started = tokio::time::Instant::now();
        assert!(wait_for_dialogs_to_clear(&[l], Duration::from_secs(5)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn wait_gives_up_at_the_window() {
        let l: Arc<dyn DialogHangup> = Arc::new(FakeListener {
            active: AtomicUsize::new(2),
        });
        let started = tokio::time::Instant::now();
        assert!(!wait_for_dialogs_to_clear(&[l], Duration::from_millis(250)).await);
        assert!(started.elapsed() >= Duration::from_millis(250));
    }

    #[tokio::test]
    async fn join_bounded_aborts_a_stuck_task() {
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        let started = tokio::time::Instant::now();
        join_bounded("stuck", handle).await;
        assert!(started.elapsed() >= JOIN_TIMEOUT);
        assert!(started.elapsed() < JOIN_TIMEOUT + Duration::from_secs(2));
    }
}
