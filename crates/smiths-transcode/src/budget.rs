//! CPU budget + admission control.
//!
//! The budget is a simple concurrent counter of live transcoding
//! calls, bounded by [`CpuBudgetConfig::max_concurrent_calls`]. It's
//! consulted exactly once per INVITE that requires transcoding: the
//! UAS (future slice — see the crate-level doc) hands a `CpuBudget`
//! through the negotiator and calls [`CpuBudget::try_admit`]. If the
//! budget has headroom, the call is admitted and the UAS keeps the
//! returned [`TranscodeLease`] alongside the dialog — dropping the
//! lease on BYE decrements the counter automatically, so a panicked
//! task can't permanently leak a slot.
//!
//! The design is deliberately slot-based, not token-bucket — the
//! cost of Opus encode is almost a pure function of frame cadence
//! (20 ms) and sample rate (48 kHz), so the engineering friction of
//! a proper leaky-bucket doesn't buy anything at this scale.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;

use smiths_core::config::TranscodeConfig;

use crate::metrics::TranscodeMetrics;

/// Default concurrent-calls cap — conservative, assumes a 4-core
/// box with ~2.5 % CPU per Opus-transcoded call. Operators raise it
/// after measuring actual consumption against
/// [`smiths_transcode_cpu_ms`](crate::TranscodeMetrics::cpu_ms).
pub const DEFAULT_MAX_CONCURRENT_CALLS: usize = 40;

/// Default per-call CPU-ms ceiling — advisory only today. The
/// counter is used to surface "this call is unexpectedly expensive"
/// alerts; it doesn't hard-preempt a live transcoder. Treat it as
/// the SLO, not the fence.
pub const DEFAULT_CPU_BUDGET_MS_PER_CALL: u64 = 50;

/// Settings for the transcoding admission layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuBudgetConfig {
    /// Hard cap on simultaneous transcoded calls. INVITEs past this
    /// get a `488 Not Acceptable Here` with `Warning: 370`.
    pub max_concurrent_calls: usize,
    /// Advisory per-call CPU-ms budget. Wired into
    /// [`TranscodeMetrics`] for alerting; not enforced per-frame.
    pub cpu_budget_ms_per_call: u64,
}

impl Default for CpuBudgetConfig {
    fn default() -> Self {
        Self {
            max_concurrent_calls: DEFAULT_MAX_CONCURRENT_CALLS,
            cpu_budget_ms_per_call: DEFAULT_CPU_BUDGET_MS_PER_CALL,
        }
    }
}

impl From<&TranscodeConfig> for CpuBudgetConfig {
    fn from(cfg: &TranscodeConfig) -> Self {
        Self {
            max_concurrent_calls: cfg.max_concurrent_calls,
            cpu_budget_ms_per_call: cfg.cpu_budget_ms_per_call,
        }
    }
}

impl From<TranscodeConfig> for CpuBudgetConfig {
    fn from(cfg: TranscodeConfig) -> Self {
        Self::from(&cfg)
    }
}

/// Admission outcomes that surface to the UAS.
#[derive(Debug, Error)]
pub enum AdmissionError {
    /// Concurrent-calls cap reached — SIP UAS should reply
    /// `488 Not Acceptable Here` + `Warning: 370 transcode budget
    /// exhausted`.
    #[error("transcode budget exhausted (active={active}, max={max})")]
    BudgetExhausted {
        /// How many transcoders are currently live.
        active: usize,
        /// The configured ceiling.
        max: usize,
    },
}

/// Process-wide CPU budget for transcoding. Cloneable — every
/// consumer holds the same `Arc<…>` internally, so the admission
/// counter is truly shared.
#[derive(Clone, Debug)]
pub struct CpuBudget {
    config: CpuBudgetConfig,
    active: Arc<AtomicUsize>,
    metrics: Arc<TranscodeMetrics>,
}

impl CpuBudget {
    /// Build a budget bound to the given metrics handle.
    #[must_use]
    pub fn new(config: CpuBudgetConfig, metrics: Arc<TranscodeMetrics>) -> Self {
        Self {
            config,
            active: Arc::new(AtomicUsize::new(0)),
            metrics,
        }
    }

    /// Configuration snapshot. Read-only — the budget isn't designed
    /// to be reconfigured at runtime (slice 5.7 will add hot-reload,
    /// but it'll rebuild the budget, not mutate this one).
    #[must_use]
    pub fn config(&self) -> CpuBudgetConfig {
        self.config
    }

    /// Metrics handle the budget reports to. Exposed so the bridge
    /// can record CPU ms on the same handle without a separate
    /// plumbing wire.
    #[must_use]
    pub fn metrics(&self) -> &Arc<TranscodeMetrics> {
        &self.metrics
    }

    /// Current number of live transcoders.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// Attempt to admit a new transcoded call. Returns a
    /// [`TranscodeLease`] guard on success; dropping the lease
    /// decrements the counter.
    ///
    /// Uses a CAS loop rather than unconditional `fetch_add` so the
    /// counter never briefly exceeds the cap even under contention.
    /// With 40 simultaneous calls and INVITE arrival rates of ~10/s
    /// that's a theoretical worry more than a practical one, but the
    /// CAS is cheap and keeps the invariant strict.
    ///
    /// # Errors
    /// [`AdmissionError::BudgetExhausted`] when the concurrent-call
    /// cap has been reached.
    pub fn try_admit(&self) -> Result<TranscodeLease, AdmissionError> {
        let max = self.config.max_concurrent_calls;
        loop {
            let cur = self.active.load(Ordering::Acquire);
            if cur >= max {
                self.metrics.admissions_refused.inc();
                return Err(AdmissionError::BudgetExhausted { active: cur, max });
            }
            if self
                .active
                .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.metrics.active.inc();
                return Ok(TranscodeLease {
                    active: Arc::clone(&self.active),
                    metrics: Arc::clone(&self.metrics),
                });
            }
        }
    }
}

/// RAII guard for an admitted transcoding call. Releases the slot
/// on drop — no explicit `release()` call required, which means a
/// panicked call-handler can't leak a slot.
///
/// The lease is `Send + Sync` so the UAS can stash it inside the
/// dialog struct and move that struct across the FSM boundary.
#[derive(Debug)]
#[must_use = "dropping the lease immediately releases the budget slot"]
pub struct TranscodeLease {
    active: Arc<AtomicUsize>,
    metrics: Arc<TranscodeMetrics>,
}

impl Drop for TranscodeLease {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        self.metrics.active.dec();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(max: usize) -> CpuBudget {
        CpuBudget::new(
            CpuBudgetConfig {
                max_concurrent_calls: max,
                cpu_budget_ms_per_call: DEFAULT_CPU_BUDGET_MS_PER_CALL,
            },
            TranscodeMetrics::noop(),
        )
    }

    #[test]
    fn admits_up_to_cap_then_refuses() {
        let b = budget(3);
        let _a = b.try_admit().unwrap();
        let _b = b.try_admit().unwrap();
        let _c = b.try_admit().unwrap();
        assert_eq!(b.active(), 3);
        let err = b.try_admit().unwrap_err();
        assert!(matches!(
            err,
            AdmissionError::BudgetExhausted { active: 3, max: 3 }
        ));
    }

    #[test]
    fn dropping_lease_frees_slot() {
        let b = budget(2);
        let lease_a = b.try_admit().unwrap();
        let _lease_b = b.try_admit().unwrap();
        assert!(b.try_admit().is_err());
        drop(lease_a);
        assert_eq!(b.active(), 1);
        // Freed slot is usable.
        let _lease_c = b.try_admit().unwrap();
        assert_eq!(b.active(), 2);
    }

    #[test]
    fn cloned_budget_shares_counter() {
        let b = budget(2);
        let b2 = b.clone();
        let _lease = b.try_admit().unwrap();
        assert_eq!(b2.active(), 1);
    }
}
