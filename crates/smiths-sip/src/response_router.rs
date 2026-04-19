//! Response correlation for locally-originated SIP requests.
//!
//! The engine owns one socket per SIP transport bind, and the UAS
//! reader loop receives everything — requests *and* responses. When a
//! [`crate::UacClient`] sends a request it registers the Via-branch it
//! used, then awaits a [`tokio::sync::oneshot`]. The UAS, on seeing a
//! response, calls [`ResponseRouter::deliver`] to resolve the matching
//! oneshot. Unknown branches are dropped with a debug log.
//!
//! This is a single-shot correlator — one response per subscription.
//! That matches SIP semantics: each request's final response resolves
//! its transaction.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use tokio::sync::oneshot;
use tracing::debug;

/// Thread-safe response correlator keyed by the Via `branch`.
#[derive(Clone, Default)]
pub struct ResponseRouter {
    pending: Arc<DashMap<String, oneshot::Sender<Bytes>>>,
}

impl ResponseRouter {
    /// Build an empty router. Cheap to clone.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register interest in the response carrying `branch`. Returns a
    /// receiver that resolves once [`Self::deliver`] fires for the
    /// same branch. Drops any prior subscription for the same branch.
    pub fn subscribe(&self, branch: impl Into<String>) -> oneshot::Receiver<Bytes> {
        let (tx, rx) = oneshot::channel();
        self.pending.insert(branch.into(), tx);
        rx
    }

    /// Deliver the response bytes to whoever subscribed on `branch`.
    /// Returns `true` when a subscriber was notified, `false` when the
    /// branch was unknown (stale response / no UAC).
    pub fn deliver(&self, branch: &str, bytes: Bytes) -> bool {
        if let Some((_, tx)) = self.pending.remove(branch) {
            if tx.send(bytes).is_err() {
                debug!(branch, "response subscriber dropped before delivery");
            }
            true
        } else {
            debug!(branch, "no subscriber for response branch");
            false
        }
    }

    /// Drop an outstanding subscription without receiving a response.
    /// Used by [`crate::UacClient`] to unwind on cancellation.
    pub fn cancel(&self, branch: &str) {
        self.pending.remove(branch);
    }

    /// Outstanding subscription count. Useful for tests and metrics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// `true` when no subscriptions are active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[tokio::test(flavor = "multi_thread")]
    async fn deliver_resolves_subscriber() {
        let r = ResponseRouter::new();
        let rx = r.subscribe("br-1");
        assert!(r.deliver("br-1", Bytes::from_static(b"200 OK")));
        let got = rx.await.unwrap();
        assert_eq!(&got[..], b"200 OK");
        assert!(r.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deliver_on_unknown_branch_returns_false() {
        let r = ResponseRouter::new();
        assert!(!r.deliver("nope", Bytes::from_static(b"x")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_drops_subscription() {
        let r = ResponseRouter::new();
        let rx = r.subscribe("br-2");
        r.cancel("br-2");
        assert!(r.is_empty());
        // The oneshot sender was dropped — receiver sees `Err`.
        assert!(rx.await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_subscribe_replaces_first() {
        let r = ResponseRouter::new();
        let rx1 = r.subscribe("br-3");
        let rx2 = r.subscribe("br-3");
        assert!(r.deliver("br-3", Bytes::from_static(b"hit")));
        // Second subscription wins; first oneshot gets dropped → Err.
        assert!(rx1.await.is_err());
        assert_eq!(&rx2.await.unwrap()[..], b"hit");
    }
}
