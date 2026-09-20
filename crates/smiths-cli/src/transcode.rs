//! Transcode admission + session construction for codec-mismatched
//! rendezvous pairs.
//!
//! The UAS consults [`BudgetedTranscodeOrchestrator`] when the two
//! legs of a rendezvous negotiated different codecs. Admission goes
//! through the engine-wide [`CpuBudget`] (`[media.transcode]
//! max_concurrent_calls`, hot-reloadable); an admitted call gets a
//! [`TranscodedSession`] transcoding between the two fabric
//! sockets.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use smiths_core::media::{BridgeId, BridgeLeg, MediaError, MediaSession};
use smiths_core::{NegotiatedCodec, TranscodeConfig};
use smiths_media::{TranscodedLeg, TranscodedSession, UdpMediaFabric};
use smiths_sip::TranscodeOrchestrator;
use smiths_transcode::{CpuBudget, CpuBudgetConfig, G711Variant, TranscodeMetrics};
use tracing::{debug, info, warn};

/// Session ids minted here start far above the fabric's own bridge
/// counter so log lines never show two sessions with one id.
const SESSION_ID_BASE: u64 = 1 << 40;

/// [`TranscodeOrchestrator`] backed by the shared CPU budget.
pub(crate) struct BudgetedTranscodeOrchestrator {
    fabric: Arc<UdpMediaFabric>,
    budget: CpuBudget,
    next_id: AtomicU64,
}

impl BudgetedTranscodeOrchestrator {
    pub(crate) fn new(fabric: Arc<UdpMediaFabric>, budget: CpuBudget) -> Self {
        Self {
            fabric,
            budget,
            next_id: AtomicU64::new(SESSION_ID_BASE),
        }
    }

    /// The live budget. The hot-reload adapter holds its own clone
    /// of the same budget, so this accessor exists for the tests
    /// that assert admission accounting.
    #[cfg(test)]
    pub(crate) fn budget(&self) -> &CpuBudget {
        &self.budget
    }
}

/// G.711 variant for a negotiated audio codec, or `None` when this
/// build cannot transcode it.
fn variant_for(codec: &NegotiatedCodec) -> Option<G711Variant> {
    match codec {
        NegotiatedCodec::Pcmu => Some(G711Variant::Pcmu),
        NegotiatedCodec::Pcma => Some(G711Variant::Pcma),
        _ => None,
    }
}

fn unsupported(codec: &NegotiatedCodec) -> MediaError {
    MediaError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("no transcoder for codec `{codec}` in this build"),
    ))
}

#[async_trait]
impl TranscodeOrchestrator for BudgetedTranscodeOrchestrator {
    async fn try_orchestrate(
        &self,
        leg_a: BridgeLeg,
        codec_a: NegotiatedCodec,
        leg_b: BridgeLeg,
        codec_b: NegotiatedCodec,
    ) -> Result<Option<Arc<dyn MediaSession>>, MediaError> {
        let variant_a = variant_for(&codec_a).ok_or_else(|| unsupported(&codec_a))?;
        let variant_b = variant_for(&codec_b).ok_or_else(|| unsupported(&codec_b))?;
        let socket_a = self
            .fabric
            .endpoint_socket(leg_a.endpoint)
            .ok_or(MediaError::UnknownEndpoint(leg_a.endpoint))?;
        let socket_b = self
            .fabric
            .endpoint_socket(leg_b.endpoint)
            .ok_or(MediaError::UnknownEndpoint(leg_b.endpoint))?;

        let Ok(session_lease) = self.budget.try_admit() else {
            warn!(
                active = self.budget.active(),
                max = self.budget.max_concurrent(),
                %codec_a, %codec_b,
                "transcode admission refused: CPU budget exhausted"
            );
            return Ok(None);
        };
        let id = BridgeId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let session = TranscodedSession::spawn(
            id,
            TranscodedLeg::g711(socket_a, leg_a.peer, variant_a),
            TranscodedLeg::g711(socket_b, leg_b.peer, variant_b),
            Arc::clone(self.budget.metrics()),
            session_lease,
        );
        info!(?id, %codec_a, %codec_b, "transcoded session installed");
        debug!(
            active = self.budget.active(),
            "transcode budget after admission"
        );
        Ok(Some(session))
    }
}

/// Build the engine's budget from `[media.transcode]`, registering
/// the transcode metrics on the shared registry.
pub(crate) fn build_budget(
    cfg: &TranscodeConfig,
    registry: &mut prometheus_client::registry::Registry,
) -> CpuBudget {
    CpuBudget::new(
        CpuBudgetConfig::from(cfg),
        TranscodeMetrics::register(registry),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use smiths_core::MediaFabric;
    use std::net::{IpAddr, Ipv4Addr};

    fn orchestrator(cap: usize) -> BudgetedTranscodeOrchestrator {
        let fabric = Arc::new(UdpMediaFabric::new());
        let budget = CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: cap,
                cpu_budget_ms_per_call: 50,
            },
            TranscodeMetrics::noop(),
        );
        BudgetedTranscodeOrchestrator::new(fabric, budget)
    }

    async fn legs(o: &BudgetedTranscodeOrchestrator) -> (BridgeLeg, BridgeLeg) {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let a = o.fabric.allocate(ip).await.unwrap();
        let b = o.fabric.allocate(ip).await.unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        (
            BridgeLeg::plain(a.id(), peer),
            BridgeLeg::plain(b.id(), peer),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admits_one_slot_per_session_and_refuses_past_cap() {
        let o = orchestrator(1);
        let (a, b) = legs(&o).await;
        let session = o
            .try_orchestrate(a, NegotiatedCodec::Pcmu, b, NegotiatedCodec::Pcma)
            .await
            .unwrap()
            .expect("first call admitted");
        assert_eq!(o.budget().active(), 1, "one call costs exactly one slot");

        let (a2, b2) = legs(&o).await;
        let refused = o
            .try_orchestrate(a2, NegotiatedCodec::Pcmu, b2, NegotiatedCodec::Pcma)
            .await
            .unwrap();
        assert!(refused.is_none(), "second call must be refused at cap 1");

        session.stop().await;
        drop(session);
        assert_eq!(
            o.budget().active(),
            0,
            "dropping the session frees the slot"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsupported_codec_is_an_error_not_a_refusal() {
        let o = orchestrator(4);
        let (a, b) = legs(&o).await;
        let Err(err) = o
            .try_orchestrate(a, NegotiatedCodec::Opus, b, NegotiatedCodec::Pcmu)
            .await
        else {
            panic!("an unsupported codec must be an error, not a refusal");
        };
        assert!(err.to_string().contains("opus"), "{err}");
        assert_eq!(o.budget().active(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_endpoint_is_reported() {
        let o = orchestrator(4);
        let peer: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        let bogus = BridgeLeg::plain(smiths_core::EndpointId(u64::MAX), peer);
        let Err(err) = o
            .try_orchestrate(
                bogus.clone(),
                NegotiatedCodec::Pcmu,
                bogus,
                NegotiatedCodec::Pcma,
            )
            .await
        else {
            panic!("an unknown endpoint must be reported as an error");
        };
        assert!(matches!(err, MediaError::UnknownEndpoint(_)));
    }
}
