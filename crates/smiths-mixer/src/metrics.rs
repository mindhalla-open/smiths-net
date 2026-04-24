//! Prometheus metrics for the mixer + conferencing subsystem
//! (slice 5.12).
//!
//! Follows the same "clone an `Arc<Metrics>` into every subsystem
//! that records" pattern `smiths-core::Metrics` establishes. The
//! engine's health HTTP server holds the shared `Registry` and
//! encodes it for `/metrics`; this module hands out typed
//! counter / gauge handles.
//!
//! Five metrics ship — minimum viable surface for "is the mixer
//! healthy?":
//!
//! - `smiths_mixer_conferences_active` (gauge)
//! - `smiths_mixer_ticks_total` (counter, labels=`{conference}`)
//! - `smiths_mixer_dominant_switches_total` (counter,
//!   labels=`{conference}`)
//! - `smiths_mixer_ingress_dropped_total` (counter,
//!   labels=`{reason=queue_full|frame_size}`)
//! - `smiths_mixer_participants_active` (gauge)

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// `{conference}` label used by per-conference counters. Cardinality
/// is bounded by the number of live conferences — operators watch
/// this; a run-away would point at a create-leak.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ConferenceLabel {
    /// Conference id as decimal string.
    pub conference: String,
}

/// `{reason}` label on `smiths_mixer_ingress_dropped_total`. Fixed
/// vocabulary so Prometheus doesn't explode on a typo.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct IngressDropReason {
    /// `"queue_full"` or `"frame_size"`.
    pub reason: String,
}

/// Every mixer metric. Arc-backed internally — clone freely; the
/// same gauge/counter is incremented whether you hold the
/// original or a clone.
#[derive(Clone, Debug)]
pub struct MixerMetrics {
    /// Currently-live conferences.
    pub conferences_active: Gauge,
    /// Per-conference mixer ticks.
    pub ticks: Family<ConferenceLabel, Counter>,
    /// Per-conference dominant-speaker transitions (speaker A →
    /// speaker B or anyone → silence).
    pub dominant_switches: Family<ConferenceLabel, Counter>,
    /// Ingress frames dropped, keyed by reason.
    pub ingress_dropped: Family<IngressDropReason, Counter>,
    /// Active participants across every conference. Divide by
    /// `conferences_active` for the average room size.
    pub participants_active: Gauge,
}

impl MixerMetrics {
    /// Register every metric on `registry`; returns a cheaply-
    /// clonable handle.
    #[must_use]
    pub fn register(registry: &mut Registry) -> Arc<Self> {
        let conferences_active = Gauge::default();
        let ticks = Family::<ConferenceLabel, Counter>::default();
        let dominant_switches = Family::<ConferenceLabel, Counter>::default();
        let ingress_dropped = Family::<IngressDropReason, Counter>::default();
        let participants_active = Gauge::default();

        registry.register(
            "smiths_mixer_conferences_active",
            "Currently-live conferences.",
            conferences_active.clone(),
        );
        registry.register(
            "smiths_mixer_ticks",
            "Per-conference mixer ticks.",
            ticks.clone(),
        );
        registry.register(
            "smiths_mixer_dominant_switches",
            "Per-conference dominant-speaker transitions.",
            dominant_switches.clone(),
        );
        registry.register(
            "smiths_mixer_ingress_dropped",
            "Participant ingress frames dropped, by reason.",
            ingress_dropped.clone(),
        );
        registry.register(
            "smiths_mixer_participants_active",
            "Active participants across every conference.",
            participants_active.clone(),
        );

        Arc::new(Self {
            conferences_active,
            ticks,
            dominant_switches,
            ingress_dropped,
            participants_active,
        })
    }

    /// Build metrics not attached to any registry. Useful for
    /// tests that construct a `Conference` without caring about
    /// `/metrics` output.
    #[must_use]
    pub fn noop() -> Arc<Self> {
        let mut scratch = Registry::default();
        Self::register(&mut scratch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_publishes_expected_series() {
        let mut registry = Registry::default();
        let m = MixerMetrics::register(&mut registry);
        m.conferences_active.inc();
        m.ticks
            .get_or_create(&ConferenceLabel {
                conference: "7".into(),
            })
            .inc();
        m.ingress_dropped
            .get_or_create(&IngressDropReason {
                reason: "queue_full".into(),
            })
            .inc_by(3);

        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        assert!(out.contains("smiths_mixer_conferences_active 1"));
        assert!(out.contains("smiths_mixer_ticks_total"));
        assert!(out.contains("conference=\"7\""));
        assert!(out.contains("reason=\"queue_full\""));
    }
}
