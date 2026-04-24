//! Prometheus metrics for the transcoding subsystem.
//!
//! Two metrics ship today (slice 5.3's acceptance bar is "operators
//! can see the CPU envelope"):
//!
//! - `smiths_transcode_active` — gauge of currently-live
//!   transcoders. Ticks up on [`CpuBudget::try_admit`](crate::CpuBudget::try_admit)
//!   success, ticks down when the returned lease is dropped. Mirrors
//!   what `media_bridges_active` does for the bridge: a single number
//!   that tells the operator "how much are we doing right now".
//!
//! - `smiths_transcode_cpu_ms_total` — cumulative CPU ms spent in
//!   encode+decode across every transcoder, keyed by codec kind. The
//!   bridge records `Duration` from an internal `Instant` each time it
//!   runs a transcode step; this counter lets operators graph cost
//!   per-codec and alert when the dispatcher's estimate drifts from
//!   reality.
//!
//! Both are registered on the same `Registry` the engine uses for
//! every other metric (scraped by the health HTTP server), so no
//! separate `/transcode-metrics` endpoint — operators see everything
//! at one `GET /metrics`.

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::codec::CodecKind;

/// Label set for `smiths_transcode_cpu_ms_total`. One label
/// (`codec=pcmu|pcma|opus|pcm16`) so cardinality stays at most 4
/// regardless of call volume.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct CodecLabel {
    /// Short codec identifier — see [`CodecKind::as_str`].
    pub codec: String,
}

/// All metrics the transcoding subsystem records. `Arc`-backed
/// internally; clone freely.
#[derive(Clone, Debug)]
pub struct TranscodeMetrics {
    /// `smiths_transcode_active` — number of currently-live
    /// transcoders (one per transcoded call leg). Operators alert
    /// when this approaches
    /// [`CpuBudgetConfig::max_concurrent_calls`](crate::CpuBudgetConfig::max_concurrent_calls).
    pub active: Gauge,
    /// `smiths_transcode_cpu_ms_total{codec="..."}` — cumulative CPU
    /// milliseconds spent in encode+decode per codec. Divide by
    /// scrape interval × number of cores to get "percent of a core".
    pub cpu_ms: Family<CodecLabel, Counter>,
    /// `smiths_transcode_admissions_refused_total` — cumulative count
    /// of INVITEs that failed
    /// [`CpuBudget::try_admit`](crate::CpuBudget::try_admit) because
    /// the budget was full. Should stay at zero in steady state;
    /// a non-zero slope means operators need to raise
    /// `max_concurrent_calls` or add more worker cores.
    pub admissions_refused: Counter,
}

impl TranscodeMetrics {
    /// Register every metric on `registry` and return a cheaply-
    /// clonable handle.
    #[must_use]
    pub fn register(registry: &mut Registry) -> Arc<Self> {
        let active = Gauge::default();
        let cpu_ms = Family::<CodecLabel, Counter>::default();
        let admissions_refused = Counter::default();

        registry.register(
            "smiths_transcode_active",
            "Currently-live transcoders (one per transcoded call leg).",
            active.clone(),
        );
        registry.register(
            "smiths_transcode_cpu_ms",
            "Cumulative CPU milliseconds spent in encode+decode per codec.",
            cpu_ms.clone(),
        );
        registry.register(
            "smiths_transcode_admissions_refused",
            "INVITEs refused because the CPU budget was full (488 Not Acceptable Here).",
            admissions_refused.clone(),
        );

        Arc::new(Self {
            active,
            cpu_ms,
            admissions_refused,
        })
    }

    /// Build metrics that aren't attached to any registry. Mirrors
    /// [`smiths_core::Metrics::noop`] — useful when a test constructs
    /// a transcoder and doesn't care about `/metrics` output.
    #[must_use]
    pub fn noop() -> Arc<Self> {
        let mut scratch = Registry::default();
        Self::register(&mut scratch)
    }

    /// Credit `ms` milliseconds of CPU to the given codec. Called from
    /// the bridge after each encode or decode step.
    pub fn record_cpu(&self, codec: CodecKind, ms: u64) {
        self.cpu_ms
            .get_or_create(&CodecLabel {
                codec: codec.as_str().to_owned(),
            })
            .inc_by(ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_emits_all_three_metrics() {
        let mut registry = Registry::default();
        let m = TranscodeMetrics::register(&mut registry);

        m.active.inc();
        m.record_cpu(CodecKind::Pcmu, 3);
        m.record_cpu(CodecKind::Opus, 17);
        m.admissions_refused.inc();

        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        assert!(out.contains("smiths_transcode_active 1"));
        assert!(out.contains("smiths_transcode_cpu_ms_total"));
        assert!(out.contains("pcmu"));
        assert!(out.contains("opus"));
        assert!(out.contains("smiths_transcode_admissions_refused_total 1"));
    }
}
