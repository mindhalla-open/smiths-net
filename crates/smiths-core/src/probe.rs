//! Error-rate probe (slice 5.9-followup).
//!
//! Runs alongside the canary deadline: while a pending
//! [`crate::ConfigReloader`] apply is in flight, this task samples
//! `plugin_invocations_total{outcome}` and `sip_parse_errors_total`
//! every second, maintains a 30-second trailing window, and trips
//! the first time either metric crosses its configured ceiling.
//! Tripping calls
//! [`ConfigReloader::rollback_with_metrics`](crate::ConfigReloader::rollback_with_metrics)
//! with [`RollbackReason::ErrorBudget`](crate::RollbackReason::ErrorBudget)
//! so operators see the same audit trail as a manual rollback —
//! the only thing that distinguishes the two is the reason label.
//!
//! ## What's sampled
//!
//! - **Plugin error rate** — `errors / (errors + oks)` inside the
//!   window. Counters come from
//!   [`Metrics::plugin_invocations`](crate::metrics::Metrics::plugin_invocations).
//!   When the total is zero the probe reports `0.0` — no traffic,
//!   no signal, no rollback.
//! - **SIP parse-error rate** — `Δerrors / Δseconds` inside the
//!   window, reading
//!   [`Metrics::sip_parse_errors`](crate::metrics::Metrics::sip_parse_errors).
//!
//! The [`ProbeConfig`] surface mirrors the `[canary]` TOML block
//! one-for-one, so no extra translation lives in the CLI — it
//! hands the probe the operator's deployed canary ceilings
//! verbatim.
//!
//! The probe shuts down as soon as the canary resolves (confirm
//! or rollback from any path), so it can't outlive the change
//! it was watching. The CLI ties the probe's `JoinHandle` to the
//! `spawn_auto_rollback` handle through a `CancellationToken` —
//! whichever arm wins (operator confirm, deadline timer, probe
//! trip) fires the token and the other two observe cancellation
//! on their next select.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::metrics::{ConfigProbeLabel, Metrics};
use crate::reloader::{ChangeReceipt, ConfigReloader, RollbackReason};

/// Trailing-window size the probe aggregates over. 30 s is long
/// enough for a bursty plugin to have committed to a failure mode
/// (rather than a one-off timeout) and short enough that the
/// canary deadline — typically 5 minutes — outlasts it by an
/// order of magnitude.
pub const WINDOW_SECS: u64 = 30;

/// How often the probe samples. 1 s ticks mean the worst-case
/// detection lag is `WINDOW_SECS + 1`; anything tighter would
/// burn CPU without improving recall.
pub const TICK: Duration = Duration::from_secs(1);

/// Ceiling thresholds the probe checks each tick. Matches
/// [`crate::config::CanaryConfig`] one-for-one; the CLI's
/// `CanaryConfig::into()` copies the block in without
/// translation.
#[derive(Clone, Copy, Debug)]
pub struct ProbeConfig {
    /// Plugin-invocation error rate ceiling (`errors / total`)
    /// in `[0.0, 1.0]`. `1.0` disables this probe — no finite
    /// rate can reach it.
    pub plugin_error_rate_ceiling: f32,
    /// SIP parse errors per second ceiling. `u64::MAX`
    /// disables.
    pub sip_parse_errors_per_sec_ceiling: u64,
}

impl ProbeConfig {
    /// `true` when both ceilings are at their disable sentinels.
    /// The CLI skips the spawn when this holds — one less
    /// background task during idle periods.
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.plugin_error_rate_ceiling >= 1.0 && self.sip_parse_errors_per_sec_ceiling == u64::MAX
    }
}

impl From<&crate::config::CanaryConfig> for ProbeConfig {
    fn from(c: &crate::config::CanaryConfig) -> Self {
        Self {
            plugin_error_rate_ceiling: c.plugin_error_rate_ceiling,
            sip_parse_errors_per_sec_ceiling: c.sip_parse_errors_per_sec_ceiling,
        }
    }
}

/// One sample the probe took off the metrics handle. Captured so
/// tests can drive the verdict logic without a live engine + so
/// operators can dump the sample ring via a future MCP resource.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeSample {
    /// Wall-clock of the sample.
    pub at: Instant,
    /// Cumulative plugin-invocation error count at `at`.
    pub plugin_errors: u64,
    /// Cumulative plugin-invocation ok count at `at`.
    pub plugin_oks: u64,
    /// Cumulative SIP parse errors at `at`.
    pub sip_parse_errors: u64,
}

/// What a single probe tick decided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeVerdict {
    /// Window not yet full, or no ceiling crossed. Keep watching.
    Ok,
    /// Plugin error-rate probe tripped. Rollback.
    PluginErrorRate,
    /// SIP parse-error probe tripped. Rollback.
    SipParseErrors,
}

impl ProbeVerdict {
    /// Metric label written to
    /// [`crate::metrics::Metrics::config_probe_triggered`]
    /// when this verdict fires a rollback. `Ok` returns `None`
    /// — no metric bump on the quiet path.
    #[must_use]
    pub fn as_metric_label(&self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::PluginErrorRate => Some("plugin_error_rate"),
            Self::SipParseErrors => Some("sip_parse_errors"),
        }
    }
}

/// Decide whether the window between `oldest` and `newest`
/// crossed any ceiling. Pure function — every field the caller
/// needs is on the two samples, so tests drive this without a
/// live tokio runtime.
#[must_use]
pub fn classify(oldest: &ProbeSample, newest: &ProbeSample, cfg: ProbeConfig) -> ProbeVerdict {
    // Window must span at least a single tick — otherwise
    // `Δerrors / Δseconds` divides by ~0 on the very first
    // sample and we report a bogus infinite rate.
    let window = newest.at.saturating_duration_since(oldest.at);
    if window < TICK {
        return ProbeVerdict::Ok;
    }

    let plugin_errors = newest.plugin_errors.saturating_sub(oldest.plugin_errors);
    let plugin_oks = newest.plugin_oks.saturating_sub(oldest.plugin_oks);
    let plugin_total = plugin_errors + plugin_oks;
    if plugin_total > 0 && cfg.plugin_error_rate_ceiling < 1.0 {
        // cast is exact for the magnitudes we see in a 30-s
        // window (max ≈ few thousand invocations).
        #[allow(clippy::cast_precision_loss)]
        let rate = plugin_errors as f32 / plugin_total as f32;
        if rate > cfg.plugin_error_rate_ceiling {
            return ProbeVerdict::PluginErrorRate;
        }
    }

    let parse_delta = newest
        .sip_parse_errors
        .saturating_sub(oldest.sip_parse_errors);
    if cfg.sip_parse_errors_per_sec_ceiling != u64::MAX {
        let window_secs = window.as_secs().max(1);
        let per_sec = parse_delta / window_secs;
        if per_sec > cfg.sip_parse_errors_per_sec_ceiling {
            return ProbeVerdict::SipParseErrors;
        }
    }

    ProbeVerdict::Ok
}

/// Sample the live metrics into a fresh [`ProbeSample`]. Used by
/// the background task but exposed so tests + the `smiths-net
/// reload --diff` path can snapshot without spawning the task.
///
/// Reads the plugin-agnostic aggregate counters rather than
/// summing the labelled `Family` — the latter has no
/// iter-values API in `prometheus-client` 0.24 and summing
/// per-tick would force a whole-registry text-encode.
#[must_use]
pub fn sample(metrics: &Metrics) -> ProbeSample {
    ProbeSample {
        at: Instant::now(),
        plugin_errors: metrics.plugin_invocations_error.get(),
        plugin_oks: metrics.plugin_invocations_ok.get(),
        sip_parse_errors: metrics.sip_parse_errors.get(),
    }
}

/// Background probe driver. Cheaply cloneable — every field is
/// an `Arc` under the hood.
#[derive(Clone)]
pub struct ErrorRateProbe {
    metrics: Arc<Metrics>,
    config: ProbeConfig,
}

impl ErrorRateProbe {
    /// Build a probe that watches `metrics` against `config`'s
    /// ceilings.
    #[must_use]
    pub fn new(metrics: Arc<Metrics>, config: ProbeConfig) -> Self {
        Self { metrics, config }
    }

    /// Spawn the probe as a tokio task. `reloader` is the live
    /// reloader; `receipt` names the change the probe is watching.
    /// `cancel` is shared with the auto-rollback timer so any
    /// three-way resolution (confirm / timeout / probe trip) ends
    /// the other two tasks cleanly.
    ///
    /// When [`ProbeConfig::is_disabled`] holds, returns a no-op
    /// `JoinHandle` that completes immediately — callers can
    /// always `await` the handle without branching.
    #[must_use]
    pub fn spawn(
        self,
        reloader: Arc<ConfigReloader>,
        receipt: &ChangeReceipt,
        cancel: CancellationToken,
    ) -> JoinHandle<ProbeVerdict> {
        if self.config.is_disabled() {
            return tokio::spawn(async move { ProbeVerdict::Ok });
        }
        let id = receipt.id;
        let metrics = Arc::clone(&self.metrics);
        let cfg = self.config;
        tokio::spawn(async move {
            let mut ticker = interval(TICK);
            // Small ring of samples spanning `WINDOW_SECS`.
            #[allow(clippy::cast_possible_truncation)]
            // WINDOW_SECS is a const 30 — fits u16, never mind usize.
            let mut window: Vec<ProbeSample> = Vec::with_capacity(WINDOW_SECS as usize + 1);
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return ProbeVerdict::Ok,
                    _ = ticker.tick() => {}
                }
                let s = sample(&metrics);
                window.push(s);
                while window.first().is_some_and(|oldest| {
                    s.at.saturating_duration_since(oldest.at) > Duration::from_secs(WINDOW_SECS)
                }) {
                    window.remove(0);
                }
                let Some(oldest) = window.first().copied() else {
                    continue;
                };
                let verdict = classify(&oldest, &s, cfg);
                if verdict != ProbeVerdict::Ok
                    && let Some(label) = verdict.as_metric_label()
                {
                    metrics
                        .config_probe_triggered
                        .get_or_create(&ConfigProbeLabel {
                            probe: label.to_owned(),
                        })
                        .inc();
                    tracing::warn!(
                        %id, probe = label,
                        "config canary probe tripped; rolling back"
                    );
                    match reloader
                        .rollback_with_metrics(id, RollbackReason::ErrorBudget, Some(&metrics))
                        .await
                    {
                        Ok(()) => {
                            cancel.cancel();
                            return verdict;
                        }
                        Err(e) => {
                            // Change already resolved (confirm
                            // won the race); the probe is obsolete.
                            tracing::debug!(%id, ?e, "probe rollback a no-op");
                            cancel.cancel();
                            return ProbeVerdict::Ok;
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChangeId;

    fn s(at_offset_secs: u64, errors: u64, oks: u64, parse: u64) -> ProbeSample {
        let base = Instant::now();
        ProbeSample {
            at: base + Duration::from_secs(at_offset_secs),
            plugin_errors: errors,
            plugin_oks: oks,
            sip_parse_errors: parse,
        }
    }

    #[test]
    fn short_window_is_always_ok() {
        // Under-a-tick window — the divide-by-~0 guard keeps the
        // probe from firing on the very first sample before any
        // time has elapsed.
        let cfg = ProbeConfig {
            plugin_error_rate_ceiling: 0.0,
            sip_parse_errors_per_sec_ceiling: 0,
        };
        let oldest = s(0, 1_000, 0, 1_000);
        let newest = s(0, 1_000, 0, 1_000);
        assert_eq!(classify(&oldest, &newest, cfg), ProbeVerdict::Ok);
    }

    #[test]
    fn plugin_error_rate_ceiling_trips() {
        // 30/50 = 0.6 > 0.5 ceiling → trip.
        let cfg = ProbeConfig {
            plugin_error_rate_ceiling: 0.5,
            sip_parse_errors_per_sec_ceiling: u64::MAX,
        };
        let oldest = s(0, 0, 0, 0);
        let newest = s(30, 30, 20, 0);
        assert_eq!(
            classify(&oldest, &newest, cfg),
            ProbeVerdict::PluginErrorRate
        );
    }

    #[test]
    fn plugin_error_rate_below_ceiling_is_ok() {
        // 10/100 = 0.1 < 0.5 ceiling → ok.
        let cfg = ProbeConfig {
            plugin_error_rate_ceiling: 0.5,
            sip_parse_errors_per_sec_ceiling: u64::MAX,
        };
        let oldest = s(0, 0, 0, 0);
        let newest = s(30, 10, 90, 0);
        assert_eq!(classify(&oldest, &newest, cfg), ProbeVerdict::Ok);
    }

    #[test]
    fn sip_parse_error_rate_ceiling_trips() {
        // 300 errors / 30 s = 10/s > 9/s ceiling → trip.
        let cfg = ProbeConfig {
            plugin_error_rate_ceiling: 1.0,
            sip_parse_errors_per_sec_ceiling: 9,
        };
        let oldest = s(0, 0, 0, 0);
        let newest = s(30, 0, 0, 300);
        assert_eq!(
            classify(&oldest, &newest, cfg),
            ProbeVerdict::SipParseErrors
        );
    }

    #[test]
    fn disabled_ceilings_never_trip() {
        // Both ceilings at sentinel → probe is inert even if
        // both underlying counters scream.
        let cfg = ProbeConfig {
            plugin_error_rate_ceiling: 1.0,
            sip_parse_errors_per_sec_ceiling: u64::MAX,
        };
        assert!(cfg.is_disabled());
        let oldest = s(0, 0, 0, 0);
        let newest = s(30, 10_000, 0, 10_000);
        assert_eq!(classify(&oldest, &newest, cfg), ProbeVerdict::Ok);
    }

    #[test]
    fn zero_traffic_is_ok_not_zero_percent_trip() {
        // A 0.0 ceiling with zero traffic must not trip — that
        // edge was the reason for the `plugin_total > 0` guard.
        let cfg = ProbeConfig {
            plugin_error_rate_ceiling: 0.0,
            sip_parse_errors_per_sec_ceiling: u64::MAX,
        };
        let oldest = s(0, 0, 0, 0);
        let newest = s(30, 0, 0, 0);
        assert_eq!(classify(&oldest, &newest, cfg), ProbeVerdict::Ok);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn spawn_disabled_completes_immediately() {
        let metrics = Metrics::noop();
        let reloader = ConfigReloader::new(crate::Config::default());
        let receipt = ChangeReceipt {
            id: ChangeId(0),
            deadline_at_unix: 0,
            report: crate::ApplyReport::default(),
        };
        let probe = ErrorRateProbe::new(
            metrics,
            ProbeConfig {
                plugin_error_rate_ceiling: 1.0,
                sip_parse_errors_per_sec_ceiling: u64::MAX,
            },
        );
        let h = probe.spawn(reloader, &receipt, CancellationToken::new());
        let verdict = h.await.unwrap();
        assert_eq!(verdict, ProbeVerdict::Ok);
    }
}
