//! Prometheus metrics registry for smiths-net.
//!
//! One `Registry` + [`Metrics`] struct is built at boot; every
//! subsystem that records data (UAS, MCP/A2A tool dispatch) gets an
//! `Arc<Metrics>` clone and increments counters / observes histograms
//! through the handles it cares about.
//!
//! Rendering is done by the health HTTP server: it holds the
//! `Arc<Registry>` and calls `prometheus_client::encoding::text::encode`
//! on demand.

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;

/// Default latency histogram buckets (seconds). Fine-grained up to
/// 10 s; adequate for MCP tool calls and SIP request handling.
pub const DEFAULT_LATENCY_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// `sip_requests_total{method="..."}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SipMethodLabel {
    pub method: String,
}

/// `sip_responses_total{code="..."}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct SipCodeLabel {
    pub code: String,
}

/// `tool_invocations_total{tool="...", outcome="..."}`.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ToolOutcomeLabel {
    pub tool: String,
    pub outcome: String,
}

/// `tool_duration_seconds{tool="..."}` histogram key.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ToolLabel {
    pub tool: String,
}

/// All engine metrics. Handles are `Arc`-backed internally, so cloning
/// the struct (or its containers) is cheap — the same counter is
/// incremented whether you hold the original or a clone.
#[derive(Clone)]
pub struct Metrics {
    /// SIP requests received, keyed by method.
    pub sip_requests: Family<SipMethodLabel, Counter>,
    /// SIP responses emitted, keyed by status code string.
    pub sip_responses: Family<SipCodeLabel, Counter>,
    /// Number of dialogs the engine currently considers live.
    pub dialogs_active: Gauge,
    /// Tool invocation outcome counts.
    pub tool_invocations: Family<ToolOutcomeLabel, Counter>,
    /// Tool call latency.
    pub tool_duration: Family<ToolLabel, Histogram, fn() -> Histogram>,
}

impl Metrics {
    /// Register every metric on `registry` and return a cheaply-
    /// clonable handle.
    #[must_use]
    pub fn register(registry: &mut Registry) -> Arc<Self> {
        let sip_requests = Family::<SipMethodLabel, Counter>::default();
        let sip_responses = Family::<SipCodeLabel, Counter>::default();
        let dialogs_active = Gauge::default();
        let tool_invocations = Family::<ToolOutcomeLabel, Counter>::default();
        let tool_duration: Family<ToolLabel, Histogram, fn() -> Histogram> =
            Family::new_with_constructor(default_histogram);

        registry.register(
            "sip_requests",
            "SIP requests received",
            sip_requests.clone(),
        );
        registry.register("sip_responses", "SIP responses sent", sip_responses.clone());
        registry.register(
            "sip_dialogs_active",
            "Currently-live SIP dialogs",
            dialogs_active.clone(),
        );
        registry.register(
            "tool_invocations",
            "MCP / A2A tool invocations",
            tool_invocations.clone(),
        );
        registry.register(
            "tool_duration_seconds",
            "Tool call latency in seconds",
            tool_duration.clone(),
        );

        Arc::new(Self {
            sip_requests,
            sip_responses,
            dialogs_active,
            tool_invocations,
            tool_duration,
        })
    }

    /// Build metrics that aren't attached to any registry. Useful for
    /// tests that inject `Arc<Metrics>` into subsystems without
    /// caring about `/metrics` output.
    #[must_use]
    pub fn noop() -> Arc<Self> {
        let mut scratch = Registry::default();
        Self::register(&mut scratch)
    }
}

fn default_histogram() -> Histogram {
    Histogram::new(DEFAULT_LATENCY_BUCKETS.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_does_not_panic_and_values_round_trip() {
        let mut registry = Registry::default();
        let m = Metrics::register(&mut registry);

        m.sip_requests
            .get_or_create(&SipMethodLabel {
                method: "INVITE".into(),
            })
            .inc();
        m.dialogs_active.inc();
        m.tool_invocations
            .get_or_create(&ToolOutcomeLabel {
                tool: "health".into(),
                outcome: "ok".into(),
            })
            .inc();
        m.tool_duration
            .get_or_create(&ToolLabel {
                tool: "health".into(),
            })
            .observe(0.002);

        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        assert!(out.contains("sip_requests_total"));
        assert!(out.contains("INVITE"));
        assert!(out.contains("sip_dialogs_active 1"));
        assert!(out.contains("tool_invocations_total"));
        assert!(out.contains("tool_duration_seconds"));
    }
}
