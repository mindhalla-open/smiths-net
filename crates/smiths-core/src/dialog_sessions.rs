//! Runtime map of live media sessions per dialog (slice 5.6).
//!
//! `DialogRecord` is the *serializable* view of a dialog (carries
//! IDs, codecs, state — everything the HA snapshot layer needs).
//! The non-serializable runtime handles — `Arc<dyn MediaSession>`
//! for each audio / video / image stream — live here. The UAS (and
//! in the future the conference registry) owns one
//! [`DialogSessions`] per process; it's the table the call FSM
//! consults when a re-INVITE needs to atomically swap a session
//! without tearing the dialog down.
//!
//! ## Data model
//!
//! Keyed by `(DialogKey, SessionKey)`. The outer dim scopes by
//! dialog; the inner by leg + media kind. A 2-peer audio call has
//! two inner entries (one per leg); adding video doubles that to
//! four; a conference with N participants scales to N (if each leg
//! runs a participant session), or 2N for audio+video.
//!
//! ## Atomic swap
//!
//! [`DialogSessions::swap`] is the load-bearing operation. It
//! returns the displaced handle rather than dropping it, so the
//! caller can install the new session first, stop the old one on
//! its own schedule (typically one tick after the peer's 200 OK so
//! any in-flight RTP drains). This is why 5.4 T.38 and 5.5
//! conferencing wirings (slice 5.6b follow-on) aren't bespoke
//! code paths — the swap semantics fit both patterns through the
//! same API.

use std::sync::Arc;

use dashmap::DashMap;

use crate::call::{DialogKey, SessionKey};
use crate::media::MediaSession;

/// `(DialogKey, SessionKey)` keyed session table.
///
/// Cheaply cloneable (`Arc<DashMap>` underneath). Shared between
/// the UAS's re-INVITE handler, the BYE handler, and the MCP
/// `join_conference` / `leave_conference` tools (5.6b) without
/// anyone needing a lock they can hold across `.await`.
#[derive(Clone, Default)]
pub struct DialogSessions {
    inner: Arc<DashMap<(DialogKey, SessionKey), Arc<dyn MediaSession>>>,
}

impl std::fmt::Debug for DialogSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogSessions")
            .field("len", &self.inner.len())
            .finish()
    }
}

impl DialogSessions {
    /// Build an empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a fresh session under `(dialog, key)`. Returns the
    /// previous entry, if any — callers that expect to be the first
    /// installer should panic on `Some(_)` in debug builds (it
    /// indicates a lost swap call earlier in the flow).
    pub fn install(
        &self,
        dialog: DialogKey,
        key: SessionKey,
        session: Arc<dyn MediaSession>,
    ) -> Option<Arc<dyn MediaSession>> {
        self.inner.insert((dialog, key), session)
    }

    /// Atomically swap the session at `(dialog, key)` for `new`. The
    /// **previous** handle (if any) is returned so the caller can
    /// `stop()` it on its own schedule. This is the primitive behind
    /// slice 5.4's audio→T.38 re-INVITE and slice 5.5's conference
    /// join/leave.
    ///
    /// On a key that had no prior entry, behaves like
    /// [`Self::install`] and returns `None`. Callers that must
    /// distinguish "first install" from "swap" should check the
    /// return value.
    pub fn swap(
        &self,
        dialog: DialogKey,
        key: SessionKey,
        new: Arc<dyn MediaSession>,
    ) -> Option<Arc<dyn MediaSession>> {
        self.inner.insert((dialog, key), new)
    }

    /// Remove one session by `(dialog, key)`. Returns the removed
    /// handle so the caller can `stop()` it without holding any
    /// lock across the `.await`.
    #[must_use = "the returned session handle must be stopped by the caller"]
    pub fn remove(&self, dialog: &DialogKey, key: &SessionKey) -> Option<Arc<dyn MediaSession>> {
        self.inner
            .remove(&(dialog.clone(), key.clone()))
            .map(|(_, v)| v)
    }

    /// Remove every session belonging to `dialog`. Used on BYE to
    /// drain the call's entire session set in one shot. Returns the
    /// removed handles; the caller is expected to `stop()` each.
    #[must_use = "every drained session handle must be stopped by the caller"]
    pub fn remove_dialog(&self, dialog: &DialogKey) -> Vec<Arc<dyn MediaSession>> {
        let mut drained = Vec::new();
        self.inner.retain(|(d, _), v| {
            if d == dialog {
                drained.push(Arc::clone(v));
                false
            } else {
                true
            }
        });
        drained
    }

    /// Fetch a snapshot clone of one session handle. Returns `None`
    /// when the key is absent (already swapped out, never installed).
    #[must_use]
    pub fn get(&self, dialog: &DialogKey, key: &SessionKey) -> Option<Arc<dyn MediaSession>> {
        self.inner
            .get(&(dialog.clone(), key.clone()))
            .map(|entry| Arc::clone(entry.value()))
    }

    /// Count of live sessions across every dialog. Useful for
    /// metrics and leak diagnostics; not intended to be polled per
    /// packet.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Snapshot of every (dialog, session-key) pair currently
    /// installed. Returned in an unspecified order; callers that
    /// need stable ordering should sort.
    #[must_use]
    pub fn keys(&self) -> Vec<(DialogKey, SessionKey)> {
        self.inner.iter().map(|e| e.key().clone()).collect()
    }

    /// `true` when no sessions are installed across any dialog.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::call::{LegId, MediaKindTag};
    use crate::media::BridgeId;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Minimal `MediaSession` double — tracks a "stopped" flag so
    /// tests can assert the handle the caller got back is the same
    /// one the displaced session refers to.
    struct FakeSession {
        id: BridgeId,
        stopped: AtomicBool,
    }

    impl FakeSession {
        fn new(id: u64) -> Arc<Self> {
            Arc::new(Self {
                id: BridgeId(id),
                stopped: AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl MediaSession for FakeSession {
        fn id(&self) -> BridgeId {
            self.id
        }
        async fn stop(&self) {
            self.stopped.store(true, Ordering::Relaxed);
        }
    }

    fn dk() -> DialogKey {
        ("c@x".into(), "lt".into(), "rt".into())
    }

    fn audio_a() -> SessionKey {
        (LegId(0), MediaKindTag::Audio)
    }

    fn audio_b() -> SessionKey {
        (LegId(1), MediaKindTag::Audio)
    }

    #[test]
    fn install_records_and_get_retrieves() {
        let sessions = DialogSessions::new();
        let s = FakeSession::new(1);
        assert!(sessions.install(dk(), audio_a(), s.clone()).is_none());
        let got = sessions.get(&dk(), &audio_a()).unwrap();
        assert_eq!(got.id(), s.id());
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn swap_returns_old_and_installs_new() {
        let sessions = DialogSessions::new();
        let old = FakeSession::new(10);
        let new = FakeSession::new(11);
        sessions.install(dk(), audio_a(), old.clone());
        let displaced = sessions
            .swap(dk(), audio_a(), new.clone())
            .expect("swap should return previous entry");
        // Displaced handle points to `old`.
        assert_eq!(displaced.id(), old.id());
        // Current entry is `new`.
        let current = sessions.get(&dk(), &audio_a()).unwrap();
        assert_eq!(current.id(), new.id());
        // Caller still controls when `old` stops — it isn't stopped
        // by the swap itself.
        assert!(!old.stopped.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn swap_then_caller_stops_old_on_schedule() {
        let sessions = DialogSessions::new();
        let old = FakeSession::new(10);
        let new = FakeSession::new(11);
        sessions.install(dk(), audio_a(), old.clone());
        let displaced = sessions.swap(dk(), audio_a(), new.clone()).unwrap();
        // Caller's schedule: stop the old handle after the swap.
        displaced.stop().await;
        assert!(old.stopped.load(Ordering::Relaxed));
        // `new` stays untouched — swapping didn't accidentally stop
        // the incoming session.
        assert!(!new.stopped.load(Ordering::Relaxed));
    }

    #[test]
    fn remove_single_key_returns_handle() {
        let sessions = DialogSessions::new();
        let a = FakeSession::new(1);
        let b = FakeSession::new(2);
        sessions.install(dk(), audio_a(), a.clone());
        sessions.install(dk(), audio_b(), b.clone());
        let removed = sessions.remove(&dk(), &audio_a()).unwrap();
        assert_eq!(removed.id(), a.id());
        assert_eq!(sessions.len(), 1);
        assert!(sessions.get(&dk(), &audio_a()).is_none());
        assert!(sessions.get(&dk(), &audio_b()).is_some());
    }

    #[test]
    fn remove_dialog_drains_all_keys_of_that_dialog() {
        let sessions = DialogSessions::new();
        let dk2 = ("other@y".to_string(), "lt2".into(), "rt2".into());
        sessions.install(dk(), audio_a(), FakeSession::new(1));
        sessions.install(dk(), audio_b(), FakeSession::new(2));
        sessions.install(dk2.clone(), audio_a(), FakeSession::new(3));
        let drained = sessions.remove_dialog(&dk());
        assert_eq!(drained.len(), 2);
        assert_eq!(sessions.len(), 1);
        // The untouched dialog still has its one session.
        assert!(sessions.get(&dk2, &audio_a()).is_some());
    }

    #[test]
    fn swap_on_absent_key_installs_and_returns_none() {
        let sessions = DialogSessions::new();
        let s = FakeSession::new(1);
        let prior = sessions.swap(dk(), audio_a(), s.clone());
        assert!(prior.is_none());
        assert!(sessions.get(&dk(), &audio_a()).is_some());
    }

    #[test]
    fn keys_enumerate_every_installed_pair() {
        let sessions = DialogSessions::new();
        sessions.install(dk(), audio_a(), FakeSession::new(1));
        sessions.install(dk(), audio_b(), FakeSession::new(2));
        let mut keys = sessions.keys();
        keys.sort();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().any(|(_, sk)| sk.0 == LegId(0)));
        assert!(keys.iter().any(|(_, sk)| sk.0 == LegId(1)));
    }
}
