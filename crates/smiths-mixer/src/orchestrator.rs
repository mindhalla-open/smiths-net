//! Concrete [`ConferenceOrchestrator`] that builds a
//! [`ConferenceParticipantSession`] when the UAS's MCP layer
//! joins a live dialog into a conference (slice 5.6e-runtime).
//!
//! Pairs with the trait seam defined in `smiths-sip::uas`. The
//! MCP `join_conference` tool's future wiring looks up the
//! dialog's endpoint + peer address, then calls
//! `try_orchestrate_conference` — this impl resolves the
//! endpoint's UDP socket through
//! [`UdpMediaFabric::endpoint_socket`], calls
//! [`Conference::join`] to mint a participant slot, and spawns
//! the session.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use async_trait::async_trait;
use smiths_core::media::{BridgeId, MediaError, MediaSession};
use smiths_core::{BridgeLeg, DialogKey};
use smiths_media::UdpMediaFabric;
use smiths_sip::ConferenceOrchestrator;

use crate::conference::{ConferenceConfig, ConferenceId};
use crate::participant::ConferenceParticipantSession;
use crate::registry::ConferenceRegistry;

/// Production [`ConferenceOrchestrator`] — resolves the
/// dialog's UDP socket through a [`UdpMediaFabric`] and spawns
/// a [`ConferenceParticipantSession`] that bridges RTP into the
/// conference registry.
pub struct MixerConferenceOrchestrator {
    fabric: Arc<UdpMediaFabric>,
    conferences: Arc<dyn ConferenceRegistry>,
    /// Maps a conference *room name* (Request-URI user-part) to the
    /// conference id minted on its first join. Behind a `Mutex` so
    /// the get-or-create is atomic across the `await` on
    /// `registry.create` — two racing first-joins to the same room
    /// must land on the same conference.
    room_map: tokio::sync::Mutex<std::collections::HashMap<String, ConferenceId>>,
    next_bridge_id: AtomicU64,
    next_ssrc: AtomicU32,
}

impl MixerConferenceOrchestrator {
    /// Build an orchestrator bound to the given fabric + registry.
    #[must_use]
    pub fn new(fabric: Arc<UdpMediaFabric>, conferences: Arc<dyn ConferenceRegistry>) -> Self {
        Self {
            fabric,
            conferences,
            room_map: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            next_bridge_id: AtomicU64::new(0),
            next_ssrc: AtomicU32::new(0xF0F0_0000),
        }
    }

    /// Resolve the conference id for `room`, creating the conference
    /// on first use. Holds the room-map lock across the create so
    /// concurrent first-joins to the same room share one conference.
    async fn conference_for_room(&self, room: &str) -> ConferenceId {
        let mut map = self.room_map.lock().await;
        if let Some(id) = map.get(room) {
            return *id;
        }
        let id = self.conferences.create(ConferenceConfig::default()).await;
        map.insert(room.to_owned(), id);
        id
    }

    fn fresh_bridge_id(&self) -> BridgeId {
        BridgeId(self.next_bridge_id.fetch_add(1, Ordering::Relaxed))
    }

    fn fresh_ssrc(&self) -> u32 {
        self.next_ssrc.fetch_add(1, Ordering::Relaxed)
    }
}

#[async_trait]
impl ConferenceOrchestrator for MixerConferenceOrchestrator {
    async fn try_orchestrate_conference(
        &self,
        _dialog: DialogKey,
        conference_id: u64,
        leg: BridgeLeg,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        let Some(socket) = self.fabric.endpoint_socket(leg.endpoint) else {
            return Err(MediaError::UnknownEndpoint(leg.endpoint));
        };
        let conf_id = ConferenceId(conference_id);
        // Resolve the live conference handle the participant session
        // needs for `push_frame` / `leave` (slice follow-on:
        // `ConferenceRegistry::get_conference`). A missing handle means
        // the conference was never created or already shut down —
        // decline so the caller can fall back to a 2-peer bridge.
        let Some(conference) = self.conferences.get_conference(conf_id) else {
            tracing::warn!(
                conference_id,
                "conference not found; declining to orchestrate"
            );
            return Ok(None);
        };
        let (participant_id, egress) = conference.join().await;
        let session = ConferenceParticipantSession::spawn(
            self.fresh_bridge_id(),
            conference,
            participant_id,
            egress,
            socket,
            leg.peer,
            self.fresh_ssrc(),
        );
        tracing::info!(
            conference_id,
            participant_id = participant_id.0,
            endpoint = ?leg.endpoint,
            "conference participant session spawned",
        );
        Ok(Some(session as Arc<dyn MediaSession>))
    }

    async fn orchestrate_room(
        &self,
        dialog: DialogKey,
        room: &str,
        leg: BridgeLeg,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        let conf_id = self.conference_for_room(room).await;
        self.try_orchestrate_conference(dialog, conf_id.0, leg)
            .await
    }
}

/// Orchestrator variant that takes an `Arc<Conference>` directly
/// instead of going through the registry seam (slice
/// 5.6e-runtime — proves the session wiring end-to-end
/// without the missing `ConferenceRegistry::get` method).
/// Deployments that hold their own `Arc<Conference>` handle
/// (tests, single-conference bridges) can use this today; the
/// general registry-backed path lights up when
/// `ConferenceRegistry::get` lands in a follow-on.
pub struct DirectConferenceOrchestrator {
    fabric: Arc<UdpMediaFabric>,
    conference: Arc<crate::conference::Conference>,
    next_bridge_id: AtomicU64,
    next_ssrc: AtomicU32,
}

impl DirectConferenceOrchestrator {
    /// Build an orchestrator bound to a specific conference
    /// handle. Every call to `try_orchestrate_conference`
    /// joins this same conference, regardless of the
    /// `conference_id` argument — so it's only appropriate for
    /// single-conference deployments or tests.
    #[must_use]
    pub fn new(
        fabric: Arc<UdpMediaFabric>,
        conference: Arc<crate::conference::Conference>,
    ) -> Self {
        Self {
            fabric,
            conference,
            next_bridge_id: AtomicU64::new(0),
            next_ssrc: AtomicU32::new(0xF0F0_0000),
        }
    }

    fn fresh_bridge_id(&self) -> BridgeId {
        BridgeId(self.next_bridge_id.fetch_add(1, Ordering::Relaxed))
    }

    fn fresh_ssrc(&self) -> u32 {
        self.next_ssrc.fetch_add(1, Ordering::Relaxed)
    }
}

#[async_trait]
impl ConferenceOrchestrator for DirectConferenceOrchestrator {
    async fn try_orchestrate_conference(
        &self,
        _dialog: DialogKey,
        _conference_id: u64,
        leg: BridgeLeg,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        let Some(socket) = self.fabric.endpoint_socket(leg.endpoint) else {
            return Err(MediaError::UnknownEndpoint(leg.endpoint));
        };
        let (participant_id, egress) = self.conference.join().await;
        let session = ConferenceParticipantSession::spawn(
            self.fresh_bridge_id(),
            Arc::clone(&self.conference),
            participant_id,
            egress,
            socket,
            leg.peer,
            self.fresh_ssrc(),
        );
        Ok(Some(session as Arc<dyn MediaSession>))
    }

    async fn orchestrate_room(
        &self,
        dialog: DialogKey,
        _room: &str,
        leg: BridgeLeg,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        // Single-conference orchestrator: every room maps to the one
        // conference handle it holds, so ignore the room name.
        self.try_orchestrate_conference(dialog, 0, leg).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conference::{Conference, ConferenceConfig};
    use smiths_core::MediaFabric;
    use std::net::IpAddr;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_orchestrator_spawns_session_for_live_conference() {
        let fabric = Arc::new(UdpMediaFabric::new());
        let conf = Conference::spawn(
            ConferenceId(1),
            ConferenceConfig {
                frame_interval: Duration::from_millis(20),
                ..Default::default()
            },
        );
        let orch = DirectConferenceOrchestrator::new(Arc::clone(&fabric), Arc::clone(&conf));

        let ep = fabric.allocate(IpAddr::from([127, 0, 0, 1])).await.unwrap();
        let peer_addr = "127.0.0.1:30000".parse().unwrap();
        let dialog: DialogKey = ("c@x".into(), "lt".into(), "rt".into());

        let session = orch
            .try_orchestrate_conference(dialog, 1, BridgeLeg::plain(ep.id(), peer_addr))
            .await
            .unwrap()
            .expect("Some session");

        assert_eq!(conf.stats().await.participants, 1);
        session.stop().await;
        // `stop()` drops the participant from the conference.
        assert_eq!(conf.stats().await.participants, 0);
        conf.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixer_orchestrator_spawns_session_via_registry() {
        use crate::registry::{ConferenceRegistry, InMemoryConferenceRegistry};

        let fabric = Arc::new(UdpMediaFabric::new());
        let registry = Arc::new(InMemoryConferenceRegistry::new());
        let conf_id = registry
            .create(ConferenceConfig {
                frame_interval: Duration::from_millis(20),
                ..Default::default()
            })
            .await;
        let orch = MixerConferenceOrchestrator::new(
            Arc::clone(&fabric),
            Arc::clone(&registry) as Arc<dyn ConferenceRegistry>,
        );

        let ep = fabric.allocate(IpAddr::from([127, 0, 0, 1])).await.unwrap();
        let peer_addr = "127.0.0.1:30100".parse().unwrap();
        let dialog: DialogKey = ("c@x".into(), "lt".into(), "rt".into());

        let session = orch
            .try_orchestrate_conference(dialog, conf_id.0, BridgeLeg::plain(ep.id(), peer_addr))
            .await
            .unwrap()
            .expect("registry-backed orchestrator spawns a session");

        let conf = registry
            .get_conference(conf_id)
            .expect("conference handle resolvable via get_conference");
        assert_eq!(conf.stats().await.participants, 1);
        session.stop().await;
        assert_eq!(conf.stats().await.participants, 0);
        registry.shutdown(conf_id).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixer_orchestrator_declines_unknown_conference() {
        use crate::registry::{ConferenceRegistry, InMemoryConferenceRegistry};

        let fabric = Arc::new(UdpMediaFabric::new());
        let registry = Arc::new(InMemoryConferenceRegistry::new());
        let orch = MixerConferenceOrchestrator::new(
            Arc::clone(&fabric),
            registry as Arc<dyn ConferenceRegistry>,
        );
        let ep = fabric.allocate(IpAddr::from([127, 0, 0, 1])).await.unwrap();
        let dialog: DialogKey = ("c@x".into(), "lt".into(), "rt".into());
        // Conference 42 was never created → decline (Ok(None)), not error.
        let out = orch
            .try_orchestrate_conference(
                dialog,
                42,
                BridgeLeg::plain(ep.id(), "127.0.0.1:30101".parse().unwrap()),
            )
            .await
            .unwrap();
        assert!(out.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_orchestrator_errors_on_unknown_endpoint() {
        let fabric = Arc::new(UdpMediaFabric::new());
        let conf = Conference::spawn(ConferenceId(1), ConferenceConfig::default());
        let orch = DirectConferenceOrchestrator::new(fabric, Arc::clone(&conf));
        let dialog: DialogKey = ("c@x".into(), "lt".into(), "rt".into());
        let result = orch
            .try_orchestrate_conference(
                dialog,
                1,
                BridgeLeg::plain(
                    smiths_core::EndpointId(99_999),
                    "127.0.0.1:1".parse().unwrap(),
                ),
            )
            .await;
        match result {
            Err(MediaError::UnknownEndpoint(_)) => {}
            Err(e) => panic!("expected UnknownEndpoint, got {e:?}"),
            Ok(_) => panic!("expected UnknownEndpoint, got Ok(_)"),
        }
        conf.shutdown().await;
    }
}
