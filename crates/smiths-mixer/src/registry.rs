//! Process-wide conference registry.
//!
//! Engines hold one [`Arc<dyn ConferenceRegistry>`] that MCP tools,
//! the UAS, and the control-plane `ControlState` all share. The
//! trait exists so tests can mock it without wiring a full mixer
//! fabric; the production impl is [`InMemoryConferenceRegistry`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use thiserror::Error;

use crate::conference::{
    Conference, ConferenceConfig, ConferenceId, ParticipantFrame, ParticipantId,
};

/// Errors surfaced by registry operations.
#[derive(Debug, Error)]
pub enum ConferenceRegistryError {
    /// `join_conference` / `leave_conference` / `shutdown` reached for
    /// a conference id that was never created (or already shut down).
    #[error("unknown conference: {0}")]
    UnknownConference(ConferenceId),
    /// A [`crate::conference::ConferenceError`] bubbling up from the
    /// mixer tick path.
    #[error(transparent)]
    Conference(#[from] crate::conference::ConferenceError),
}

/// Process-wide conference handle manager.
#[async_trait]
pub trait ConferenceRegistry: Send + Sync {
    /// Create a fresh conference with the given config. Returns a
    /// stable id the operator can use in subsequent MCP calls.
    async fn create(&self, cfg: ConferenceConfig) -> ConferenceId;

    /// Add a participant to an existing conference. Returns the
    /// participant id + the egress `Receiver` the caller drains to
    /// hear the mix.
    async fn join(
        &self,
        conf: ConferenceId,
    ) -> Result<
        (ParticipantId, tokio::sync::mpsc::Receiver<ParticipantFrame>),
        ConferenceRegistryError,
    >;

    /// Remove a participant. No-op for unknown participant ids.
    async fn leave(
        &self,
        conf: ConferenceId,
        participant: ParticipantId,
    ) -> Result<(), ConferenceRegistryError>;

    /// Push one PCM16 frame from a participant.
    async fn push_frame(
        &self,
        conf: ConferenceId,
        participant: ParticipantId,
        samples: Vec<i16>,
    ) -> Result<(), ConferenceRegistryError>;

    /// Tear a conference down. Idempotent — calling `shutdown` on an
    /// already-dropped conference returns `UnknownConference`.
    async fn shutdown(&self, conf: ConferenceId) -> Result<(), ConferenceRegistryError>;

    /// Snapshot of currently-live conference ids.
    async fn list(&self) -> Vec<ConferenceId>;

    /// Fetch the live [`Conference`] handle for `conf`, if any. The
    /// conference orchestrator needs the concrete handle to spawn a
    /// participant session (`push_frame` / `leave`). Default returns
    /// `None` so mock registries don't have to implement it — only
    /// registries that own real `Arc<Conference>`s (the in-memory
    /// impl) populate the orchestrator's session-spawn path.
    fn get_conference(&self, conf: ConferenceId) -> Option<Arc<Conference>> {
        let _ = conf;
        None
    }
}

/// In-memory registry. Suitable for single-node deployments; a
/// future "routing plugin" could delegate across nodes for
/// horizontal scale but that's out of scope for slice 5.5.
#[derive(Default)]
pub struct InMemoryConferenceRegistry {
    next_id: AtomicU64,
    conferences: DashMap<ConferenceId, Arc<Conference>>,
}

impl InMemoryConferenceRegistry {
    /// Build a fresh empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn fresh_id(&self) -> ConferenceId {
        ConferenceId(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn get(&self, id: ConferenceId) -> Result<Arc<Conference>, ConferenceRegistryError> {
        self.conferences
            .get(&id)
            .map(|c| Arc::clone(c.value()))
            .ok_or(ConferenceRegistryError::UnknownConference(id))
    }
}

#[async_trait]
impl ConferenceRegistry for InMemoryConferenceRegistry {
    async fn create(&self, cfg: ConferenceConfig) -> ConferenceId {
        let id = self.fresh_id();
        let conf = Conference::spawn(id, cfg);
        self.conferences.insert(id, conf);
        id
    }

    async fn join(
        &self,
        conf: ConferenceId,
    ) -> Result<
        (ParticipantId, tokio::sync::mpsc::Receiver<ParticipantFrame>),
        ConferenceRegistryError,
    > {
        let handle = self.get(conf)?;
        Ok(handle.join().await)
    }

    async fn leave(
        &self,
        conf: ConferenceId,
        participant: ParticipantId,
    ) -> Result<(), ConferenceRegistryError> {
        let handle = self.get(conf)?;
        handle.leave(participant).await?;
        Ok(())
    }

    async fn push_frame(
        &self,
        conf: ConferenceId,
        participant: ParticipantId,
        samples: Vec<i16>,
    ) -> Result<(), ConferenceRegistryError> {
        let handle = self.get(conf)?;
        handle.push_frame(participant, samples).await?;
        Ok(())
    }

    async fn shutdown(&self, conf: ConferenceId) -> Result<(), ConferenceRegistryError> {
        let (_, handle) = self
            .conferences
            .remove(&conf)
            .ok_or(ConferenceRegistryError::UnknownConference(conf))?;
        handle.shutdown().await;
        Ok(())
    }

    async fn list(&self) -> Vec<ConferenceId> {
        let mut ids: Vec<ConferenceId> = self.conferences.iter().map(|e| *e.key()).collect();
        ids.sort();
        ids
    }

    fn get_conference(&self, conf: ConferenceId) -> Option<Arc<Conference>> {
        self.conferences.get(&conf).map(|c| Arc::clone(c.value()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_join_leave_list_shutdown_round_trip() {
        let reg = InMemoryConferenceRegistry::new();
        let a = reg.create(ConferenceConfig::default()).await;
        let b = reg.create(ConferenceConfig::default()).await;
        assert_ne!(a, b);

        let (p1, _rx1) = reg.join(a).await.unwrap();
        let (p2, _rx2) = reg.join(a).await.unwrap();
        assert_ne!(p1, p2);

        reg.leave(a, p1).await.unwrap();
        let ids = reg.list().await;
        assert_eq!(ids, vec![a, b]);

        reg.shutdown(a).await.unwrap();
        let ids_after = reg.list().await;
        assert_eq!(ids_after, vec![b]);

        // Second shutdown is an error (not a panic).
        assert!(matches!(
            reg.shutdown(a).await.unwrap_err(),
            ConferenceRegistryError::UnknownConference(_)
        ));
    }
}
