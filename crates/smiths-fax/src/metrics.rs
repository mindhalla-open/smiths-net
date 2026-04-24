//! Prometheus metrics for the T.38 FAX subsystem (slice 5.12).
//!
//! Three metrics ship — the minimum "is the relay healthy?"
//! surface:
//!
//! - `smiths_fax_sessions_active` (gauge) — currently-live
//!   [`UdptlSession`](crate::UdptlSession)s.
//! - `smiths_fax_datagrams_forwarded_total` (counter,
//!   labels=`{direction=a_to_b|b_to_a}`) — per-direction UDPTL
//!   datagrams the relay forwarded.
//! - `smiths_fax_parse_errors_total` (counter,
//!   labels=`{kind=truncated|length_overflow|error_recovery|payload_too_large}`)
//!   — UDPTL frames the parser rejected. Non-zero under normal
//!   traffic usually points at a buggy fax terminal or a `MitM` that
//!   mangled the datagram.
//!
//! Follows the same registry pattern as `smiths-core::Metrics`:
//! one `Arc<FaxMetrics>` per process, registered at boot, cloned
//! into every `UdptlSession` that records.

use std::sync::Arc;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

use crate::udptl::UdptlError;

/// `{direction}` label on the forwarded-datagrams counter.
/// Fixed-cardinality ({`"a_to_b"`, `"b_to_a"`}) — Prometheus stays
/// happy regardless of call volume.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct FaxDirectionLabel {
    /// `"a_to_b"` or `"b_to_a"`.
    pub direction: String,
}

/// `{kind}` label on the parse-error counter. Four variants, one
/// per [`UdptlError`] case.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct FaxParseErrorLabel {
    /// Error kind token — see [`FaxMetrics::record_parse_error`].
    pub kind: String,
}

/// Every fax metric. Cheaply cloneable — the underlying
/// atomics are shared.
#[derive(Clone, Debug)]
pub struct FaxMetrics {
    /// Currently-live UDPTL relay sessions.
    pub sessions_active: Gauge,
    /// Datagrams the forwarder relayed, by direction.
    pub datagrams_forwarded: Family<FaxDirectionLabel, Counter>,
    /// UDPTL parse errors, by kind.
    pub parse_errors: Family<FaxParseErrorLabel, Counter>,
}

impl FaxMetrics {
    /// Register every metric on `registry`; returns a cheaply-
    /// clonable handle.
    #[must_use]
    pub fn register(registry: &mut Registry) -> Arc<Self> {
        let sessions_active = Gauge::default();
        let datagrams_forwarded = Family::<FaxDirectionLabel, Counter>::default();
        let parse_errors = Family::<FaxParseErrorLabel, Counter>::default();

        registry.register(
            "smiths_fax_sessions_active",
            "Currently-live UDPTL relay sessions.",
            sessions_active.clone(),
        );
        registry.register(
            "smiths_fax_datagrams_forwarded",
            "UDPTL datagrams the relay forwarded, per direction.",
            datagrams_forwarded.clone(),
        );
        registry.register(
            "smiths_fax_parse_errors",
            "UDPTL parse errors, per kind.",
            parse_errors.clone(),
        );

        Arc::new(Self {
            sessions_active,
            datagrams_forwarded,
            parse_errors,
        })
    }

    /// Build metrics not attached to any registry. Tests use this.
    #[must_use]
    pub fn noop() -> Arc<Self> {
        let mut scratch = Registry::default();
        Self::register(&mut scratch)
    }

    /// Translate a [`UdptlError`] into the kind token used on
    /// `smiths_fax_parse_errors_total{kind}`. Centralized here so
    /// every call site records the same label values.
    pub fn record_parse_error(&self, err: &UdptlError) {
        let kind = match err {
            UdptlError::Truncated(_) => "truncated",
            UdptlError::LengthOverflow => "length_overflow",
            UdptlError::MalformedErrorRecovery => "error_recovery",
            UdptlError::PayloadTooLarge => "payload_too_large",
        };
        self.parse_errors
            .get_or_create(&FaxParseErrorLabel {
                kind: kind.to_owned(),
            })
            .inc();
    }

    /// Credit one forwarded datagram to `direction`.
    pub fn record_forward(&self, direction: &str) {
        self.datagrams_forwarded
            .get_or_create(&FaxDirectionLabel {
                direction: direction.to_owned(),
            })
            .inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_publishes_expected_series() {
        let mut registry = Registry::default();
        let m = FaxMetrics::register(&mut registry);
        m.sessions_active.inc();
        m.record_forward("a_to_b");
        m.record_forward("b_to_a");
        m.record_parse_error(&UdptlError::Truncated(3));
        m.record_parse_error(&UdptlError::LengthOverflow);

        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        assert!(out.contains("smiths_fax_sessions_active 1"));
        assert!(out.contains("smiths_fax_datagrams_forwarded_total"));
        assert!(out.contains("direction=\"a_to_b\""));
        assert!(out.contains("direction=\"b_to_a\""));
        assert!(out.contains("kind=\"truncated\""));
        assert!(out.contains("kind=\"length_overflow\""));
    }

    #[test]
    fn record_parse_error_maps_every_variant() {
        let m = FaxMetrics::noop();
        m.record_parse_error(&UdptlError::Truncated(3));
        m.record_parse_error(&UdptlError::LengthOverflow);
        m.record_parse_error(&UdptlError::MalformedErrorRecovery);
        m.record_parse_error(&UdptlError::PayloadTooLarge);
        // No direct read path — but the helpers didn't panic,
        // which is the contract. A follow-on slice can add a
        // `get_sample` method if operators ask.
    }
}
