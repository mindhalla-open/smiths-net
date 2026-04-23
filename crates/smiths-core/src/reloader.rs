//! Config hot-reload substrate (slice 5.8-mvp + 5.9-mvp).
//!
//! Wraps the engine's live [`Config`] in an `ArcSwap<Config>` so
//! subscribers can observe changes without restarting and the
//! control plane can roll back a bad change within a bounded
//! canary window.
//!
//! ## Architecture
//!
//! - The engine owns one [`ConfigReloader`] at boot; it's the
//!   source of truth for "what config is live right now".
//! - [`Self::current`] returns a cheap `Arc<Config>` snapshot any
//!   subsystem can read — zero lock contention on the hot path.
//! - [`Self::apply`] atomically swaps in a new `Arc<Config>`,
//!   mints a [`ChangeReceipt`], and retains the prior snapshot.
//!   The receipt carries a `deadline_at_unix` (Unix seconds at
//!   which auto-rollback fires); the caller runs a timer that
//!   tests the deadline and calls [`Self::rollback`] if the
//!   operator hasn't [`Self::confirm`]ed.
//! - [`Self::confirm`] drops the prior snapshot — the apply is
//!   permanent.
//! - [`Self::rollback`] atomically swaps back to the prior
//!   snapshot and records the rollback reason.
//!
//! ## What's NOT in this mvp
//!
//! - **Auto-rollback timer task.** The deadline lives on the
//!   receipt; a caller decides how to watch it. The CLI's
//!   canary layer adds the timer in a follow-on slice.
//! - **`#[derive(Reloadable)]` macro.** The MVP hardcodes the
//!   reloadable-vs-restart-required field list in
//!   [`Config::apply_report`] (on the `Config` type itself).
//!   Follow-on slice lands the derive macro to keep the list
//!   from drifting.
//! - **Subsystem read-throughs.** Only the `config://current`
//!   MCP resource + future `get_config`/`put_config` tools
//!   consult the live reloader. Every other subsystem still
//!   reads its boot snapshot — documented as "restart required"
//!   for those fields. Follow-on slice threads `Arc<ConfigReloader>`
//!   into tracing-filter / rate-limiter / prompt library /
//!   proxy connector for live updates.
//! - **SIGHUP handler.** The CLI layer adds POSIX SIGHUP +
//!   Windows named-event wiring that calls `apply`; the
//!   reloader itself stays async-runtime-agnostic.
//! - **Error-rate probe.** The 5.9-spec watcher that triggers
//!   early rollback on `plugin_invocations{outcome="error"}` +
//!   `sip_parse_errors` rate ceilings is a focused follow-on;
//!   today's canary is deadline-only.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

use crate::Config;

/// Opaque identifier for one `apply` call. Sequential per
/// process; wraps at `u64::MAX` but that's 580 years of apply
/// calls per second, so practically infinite.
#[derive(
    Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ChangeId(pub u64);

impl std::fmt::Display for ChangeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "apply-{}", self.0)
    }
}

/// Receipt handed back by [`ConfigReloader::apply`].
///
/// Caller stashes it somewhere operator-visible (audit log,
/// MCP resource) and arms a timer for the deadline. When the
/// timer fires without a prior `confirm(id)`, the caller runs
/// `rollback(id)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangeReceipt {
    /// The id minted for this apply.
    pub id: ChangeId,
    /// Unix seconds at which the caller's timer should fire to
    /// auto-rollback (if the operator hasn't confirmed).
    pub deadline_at_unix: i64,
    /// Report of what fields changed (reloadable vs
    /// restart-required). Same content as [`Self::report`]
    /// exposes; duplicated on the receipt so a stored receipt
    /// is self-describing.
    pub report: ApplyReport,
}

/// Outcome of applying a candidate `Config`. Reports which
/// reloadable fields actually changed + which restart-required
/// fields changed (which `apply` rejects — see [`ApplyError`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Reloadable fields whose value differs from the prior
    /// snapshot. Dotted path (`"observability.log_level"`,
    /// `"ai.openai_api_key"`).
    pub reloaded: Vec<String>,
    /// Restart-required fields that would change if the apply
    /// proceeded. Non-empty → `apply` returns
    /// [`ApplyError::RestartRequired`] and the swap is *not*
    /// performed.
    pub restart_required: Vec<String>,
}

impl ApplyReport {
    /// `true` when no reloadable field changed — the apply is
    /// a no-op.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.reloaded.is_empty() && self.restart_required.is_empty()
    }
}

/// Errors returned by [`ConfigReloader::apply`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// Candidate config differs on at least one restart-required
    /// field. The swap is not performed; operator restarts the
    /// engine to pick up the change.
    #[error(
        "candidate config changes restart-required fields: {fields:?}; \
         restart the engine to apply"
    )]
    RestartRequired {
        /// Fields that would need a restart.
        fields: Vec<String>,
    },
    /// Candidate config failed `Config::validate()`. Reason
    /// carried verbatim.
    #[error("candidate config failed validation: {0}")]
    Invalid(String),
}

/// Reason surfaced to [`ConfigReloader::rollback`] for metrics
/// + audit logging.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackReason {
    /// Operator invoked the MCP / CLI rollback path.
    Manual,
    /// Auto-rollback deadline fired without a `confirm`.
    Timeout,
    /// Hard-failure probe tripped its error-rate ceiling
    /// (5.9 follow-on slice — the enum value exists today so
    /// the wire format is stable when the probe lands).
    ErrorBudget,
}

impl RollbackReason {
    /// Wire-format token matching the `snake_case` serde rep.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Timeout => "timeout",
            Self::ErrorBudget => "error_budget",
        }
    }
}

/// Errors returned by [`ConfigReloader::confirm`] /
/// [`ConfigReloader::rollback`].
#[derive(Debug, thiserror::Error)]
pub enum CanaryError {
    /// No change with this id — already confirmed, already
    /// rolled back, or never minted.
    #[error("unknown change id: {0}")]
    UnknownChange(ChangeId),
}

/// Process-wide config reloader.
///
/// Cheap to clone (two `Arc`s inside). Subsystems hold a clone
/// and call `current()` on every hot-path access.
pub struct ConfigReloader {
    current: ArcSwap<Config>,
    state: Mutex<ReloaderState>,
    next_change_id: AtomicU64,
    /// Broadcast channel subsystems subscribe to for
    /// change notifications (slice 5.8-b). Every `apply` and
    /// `rollback` sends the new live `Arc<Config>` through
    /// here. Subsystems hold a `watch::Receiver` and
    /// `select!`-await `changed()` — no polling.
    watch_tx: watch::Sender<Arc<Config>>,
}

struct ReloaderState {
    /// Live change that hasn't been confirmed or rolled back
    /// yet. `None` when every prior apply has resolved.
    pending: Option<PendingChange>,
}

struct PendingChange {
    id: ChangeId,
    /// The `Arc<Config>` that was live *before* this apply.
    /// Swapped back on rollback.
    prior_snapshot: Arc<Config>,
    /// Original receipt — returned to the caller, stashed here
    /// for the history ring in a follow-on slice.
    receipt: ChangeReceipt,
}

impl ConfigReloader {
    /// Build a reloader seeded with the engine's boot config.
    #[must_use]
    pub fn new(boot_config: Config) -> Arc<Self> {
        let arc = Arc::new(boot_config);
        let (watch_tx, _rx) = watch::channel(Arc::clone(&arc));
        Arc::new(Self {
            current: ArcSwap::new(arc),
            state: Mutex::new(ReloaderState { pending: None }),
            next_change_id: AtomicU64::new(0),
            watch_tx,
        })
    }

    /// Subscribe to config-change notifications (slice 5.8-b).
    /// The returned `Receiver` yields the new live `Arc<Config>`
    /// on every `apply` or `rollback`. Cheap to clone; subsystems
    /// call this once at boot and hold the receiver for the
    /// process lifetime.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.watch_tx.subscribe()
    }

    /// Cheap snapshot of the live config. Every caller gets a
    /// consistent view — concurrent `apply` doesn't tear the
    /// `Arc`.
    #[must_use]
    pub fn current(&self) -> Arc<Config> {
        self.current.load_full()
    }

    /// Attempt to apply a candidate config.
    ///
    /// - **No-op** (every field identical): returns a receipt
    ///   with an empty report. No swap happens; the pending
    ///   slot stays empty.
    /// - **Restart-required field changed**: returns
    ///   [`ApplyError::RestartRequired`] — no swap.
    /// - **Only reloadable fields changed + validates**: swaps
    ///   the live `Arc<Config>` atomically, mints a receipt
    ///   with `deadline_at_unix = now + canary_window_s`, and
    ///   stores the prior snapshot so `rollback` can restore it.
    ///   Subsequent `apply` calls while a pending change is in
    ///   flight error with [`ApplyError::Invalid`] (the prior
    ///   apply must confirm or roll back first — keeps the
    ///   canary model simple).
    ///
    /// # Errors
    /// [`ApplyError`] — see variant docs.
    pub async fn apply(
        &self,
        candidate: Config,
        canary_window_s: u64,
    ) -> Result<ChangeReceipt, ApplyError> {
        if let Err(e) = candidate.validate() {
            return Err(ApplyError::Invalid(e));
        }

        let mut state = self.state.lock().await;
        if state.pending.is_some() {
            return Err(ApplyError::Invalid(
                "previous apply still pending — confirm or rollback first".into(),
            ));
        }

        let prior = self.current.load_full();
        let report = prior.apply_report(&candidate);

        if !report.restart_required.is_empty() {
            return Err(ApplyError::RestartRequired {
                fields: report.restart_required,
            });
        }

        let id = ChangeId(self.next_change_id.fetch_add(1, Ordering::Relaxed));
        #[allow(clippy::cast_possible_wrap)]
        let deadline_at_unix = current_unix_seconds() + canary_window_s as i64;
        let receipt = ChangeReceipt {
            id,
            deadline_at_unix,
            report,
        };

        // Swap only if something actually changed. A no-op
        // apply still mints a receipt so the operator sees
        // their action acknowledged, but the pending slot
        // stays empty.
        if !receipt.report.reloaded.is_empty() {
            let new_arc = Arc::new(candidate);
            self.current.store(Arc::clone(&new_arc));
            state.pending = Some(PendingChange {
                id,
                prior_snapshot: prior,
                receipt: receipt.clone(),
            });
            // Broadcast the new config to every subscriber
            // (slice 5.8-b). `watch::send` never errors when
            // at least the reloader itself holds a sender; the
            // receivers drain asynchronously.
            let _ = self.watch_tx.send(new_arc);
        }

        Ok(receipt)
    }

    /// Confirm a pending change — drop the prior snapshot, the
    /// apply is permanent.
    ///
    /// # Errors
    /// [`CanaryError::UnknownChange`] when `id` doesn't match
    /// the current pending change.
    pub async fn confirm(&self, id: ChangeId) -> Result<(), CanaryError> {
        let mut state = self.state.lock().await;
        match state.pending.as_ref() {
            Some(p) if p.id == id => {
                state.pending = None;
                Ok(())
            }
            _ => Err(CanaryError::UnknownChange(id)),
        }
    }

    /// Roll back a pending change — atomically restore the
    /// prior snapshot and clear the pending slot.
    ///
    /// # Errors
    /// [`CanaryError::UnknownChange`] when `id` doesn't match
    /// the current pending change.
    pub async fn rollback(&self, id: ChangeId, _reason: RollbackReason) -> Result<(), CanaryError> {
        let mut state = self.state.lock().await;
        match state.pending.take() {
            Some(p) if p.id == id => {
                let restored = Arc::clone(&p.prior_snapshot);
                self.current.store(p.prior_snapshot);
                // Broadcast the restored config (slice 5.8-b)
                // so subsystems reverse any live-applied
                // side effects.
                let _ = self.watch_tx.send(restored);
                Ok(())
            }
            Some(p) => {
                // Put it back — id didn't match.
                state.pending = Some(p);
                Err(CanaryError::UnknownChange(id))
            }
            None => Err(CanaryError::UnknownChange(id)),
        }
    }

    /// Snapshot the current canary state. `Some` = a pending
    /// change is in flight; `None` = every prior apply has
    /// resolved.
    pub async fn pending(&self) -> Option<ChangeReceipt> {
        self.state
            .lock()
            .await
            .pending
            .as_ref()
            .map(|p| p.receipt.clone())
    }

    /// Spawn an auto-rollback timer task (slice 5.8-followup-c).
    /// Sleeps until `receipt.deadline_at_unix`; if the change is
    /// still pending, calls `rollback(id, Timeout)` and updates
    /// the canary metrics. Idempotent with operator `confirm` /
    /// manual `rollback` — the timer's rollback call returns
    /// `UnknownChange` when the change already resolved.
    pub fn spawn_auto_rollback(
        self: &Arc<Self>,
        receipt: &ChangeReceipt,
        metrics: Option<Arc<crate::metrics::Metrics>>,
    ) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        let id = receipt.id;
        let deadline = receipt.deadline_at_unix;
        if let Some(m) = &metrics {
            m.config_canary_active.set(1);
        }
        tokio::spawn(async move {
            let now = current_unix_seconds();
            let wait = deadline.saturating_sub(now).max(0);
            #[allow(clippy::cast_sign_loss)] // clamped ≥ 0
            let wait = wait as u64;
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            match me.rollback(id, RollbackReason::Timeout).await {
                Ok(()) => {
                    tracing::warn!(%id, "config canary deadline fired; rolled back");
                    if let Some(m) = &metrics {
                        m.config_canary_active.set(0);
                        m.config_rollbacks
                            .get_or_create(&crate::metrics::ConfigRollbackLabel {
                                reason: RollbackReason::Timeout.as_str().to_owned(),
                            })
                            .inc();
                    }
                }
                Err(CanaryError::UnknownChange(_)) => {
                    tracing::debug!(%id, "canary timer fired but change already resolved");
                    if let Some(m) = &metrics {
                        m.config_canary_active.set(0);
                    }
                }
            }
        })
    }

    /// Spawn a subsystem read-through adapter (slice 5.8-b).
    ///
    /// `extract` pulls one reloadable value out of the live
    /// `Config`; `apply` takes the extracted value and does
    /// whatever the subsystem needs to make it live (e.g. call
    /// `tracing_subscriber::reload::Handle::reload`). The
    /// adapter task watches the reloader's broadcast and only
    /// invokes `apply` when the extracted value actually
    /// changed — a no-op apply on the config doesn't wake the
    /// adapter.
    ///
    /// `field_name` is the dotted path used by
    /// [`crate::metrics::Metrics::config_reloaded_fields`] so
    /// operators see which adapter fired.
    ///
    /// The returned `JoinHandle` completes when every
    /// `watch::Sender` drops (which happens only on
    /// `ConfigReloader` teardown) or the caller aborts.
    pub fn spawn_read_through<T, F, A>(
        self: &Arc<Self>,
        field_name: &'static str,
        metrics: Option<Arc<crate::metrics::Metrics>>,
        mut extract: F,
        mut apply: A,
    ) -> tokio::task::JoinHandle<()>
    where
        T: PartialEq + Clone + Send + 'static,
        F: FnMut(&Config) -> T + Send + 'static,
        A: FnMut(&T) + Send + 'static,
    {
        let mut rx = self.subscribe();
        // Seed with the current value; never invoke `apply` for
        // the initial state — the subsystem already initialised
        // itself from the boot config. We only react to CHANGES.
        let initial = extract(&rx.borrow());
        tokio::spawn(async move {
            let mut last = initial;
            while rx.changed().await.is_ok() {
                let next = extract(&rx.borrow());
                if next == last {
                    continue;
                }
                apply(&next);
                last = next;
                if let Some(m) = &metrics {
                    m.config_reloaded_fields
                        .get_or_create(&crate::metrics::ConfigFieldLabel {
                            field: field_name.to_owned(),
                        })
                        .inc();
                }
            }
        })
    }

    /// Rollback helper that also updates metrics. Thin wrapper
    /// around [`Self::rollback`] — use this for operator-driven
    /// rollbacks so `smiths_config_rollbacks_total{reason}`
    /// increments correctly.
    ///
    /// # Errors
    /// [`CanaryError`] — see variant docs.
    pub async fn rollback_with_metrics(
        &self,
        id: ChangeId,
        reason: RollbackReason,
        metrics: Option<&crate::metrics::Metrics>,
    ) -> Result<(), CanaryError> {
        self.rollback(id, reason).await?;
        if let Some(m) = metrics {
            m.config_canary_active.set(0);
            m.config_rollbacks
                .get_or_create(&crate::metrics::ConfigRollbackLabel {
                    reason: reason.as_str().to_owned(),
                })
                .inc();
        }
        Ok(())
    }
}

fn current_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl Config {
    /// Produce an [`ApplyReport`] diffing `self` (prior) against
    /// `new` (candidate). Hardcoded field list today — a future
    /// slice lands `#[derive(Reloadable)]` to keep this from
    /// drifting against the real struct.
    ///
    /// Conservative default: any field the list doesn't
    /// explicitly name as reloadable is treated as
    /// restart-required. That keeps the blast radius of an
    /// unknown field small — a new config block lands, and
    /// changing it is "restart required" until the reloadable
    /// list is updated in tandem.
    #[must_use]
    pub fn apply_report(&self, new: &Self) -> ApplyReport {
        let mut report = ApplyReport::default();

        // --- Reloadable fields ---
        if self.observability.log_level != new.observability.log_level {
            report.reloaded.push("observability.log_level".into());
        }
        if self.sip.rate_limit.per_sec != new.sip.rate_limit.per_sec
            || self.sip.rate_limit.burst != new.sip.rate_limit.burst
        {
            report.reloaded.push("sip.rate_limit".into());
        }
        if self.ai.openai_api_key != new.ai.openai_api_key {
            report.reloaded.push("ai.openai_api_key".into());
        }
        if self.ai.anthropic_api_key != new.ai.anthropic_api_key {
            report.reloaded.push("ai.anthropic_api_key".into());
        }
        if self.media.prompts.capacity != new.media.prompts.capacity {
            report.reloaded.push("media.prompts.capacity".into());
        }
        if self.media.transcode.max_concurrent_calls != new.media.transcode.max_concurrent_calls
            || self.media.transcode.cpu_budget_ms_per_call
                != new.media.transcode.cpu_budget_ms_per_call
        {
            report.reloaded.push("media.transcode".into());
        }

        // --- Restart-required fields (anything structural) ---
        if self.sip.bind != new.sip.bind
            || self.sip.transports != new.sip.transports
            || self.sip.tls_cert_path != new.sip.tls_cert_path
            || self.sip.tls_key_path != new.sip.tls_key_path
        {
            report
                .restart_required
                .push("sip bind / transports / tls paths".into());
        }
        if self.mcp.http_bind != new.mcp.http_bind
            || self.mcp.enabled_http != new.mcp.enabled_http
            || self.mcp.http3.enabled != new.mcp.http3.enabled
            || self.mcp.http3.bind != new.mcp.http3.bind
        {
            report.restart_required.push("mcp binds".into());
        }
        if self.a2a.enabled != new.a2a.enabled || self.a2a.bind != new.a2a.bind {
            report.restart_required.push("a2a bind".into());
        }
        if self.observability.health_bind != new.observability.health_bind
            || self.observability.log_format != new.observability.log_format
        {
            report
                .restart_required
                .push("observability bind / log format".into());
        }
        if self.plugins.dir != new.plugins.dir {
            report.restart_required.push("plugins.dir".into());
        }
        if self.auth.backend != new.auth.backend {
            report.restart_required.push("auth backend".into());
        }
        if self.storage.backend != new.storage.backend {
            report.restart_required.push("storage backend".into());
        }

        report
    }

    /// Validate the config — catches semantic errors that
    /// pass TOML parsing but would fail at apply time.
    ///
    /// Cheap today: checks (a) rate-limit thresholds are
    /// self-consistent, (b) TLS paths exist when TLS is in
    /// `sip.transports`, (c) bind addresses parse (they
    /// already did during deserialization — included for
    /// future structural checks). A future slice adds cert+key
    /// cryptographic match verification via rustls.
    ///
    /// # Errors
    /// Returns a single human-readable reason on the first
    /// failure — for a multi-error accumulator, build a
    /// `Vec<String>` wrapper.
    pub fn validate(&self) -> Result<(), String> {
        if self.sip.rate_limit.per_sec > 0 && self.sip.rate_limit.burst == 0 {
            return Err(
                "sip.rate_limit: per_sec > 0 but burst = 0 — bucket never admits traffic".into(),
            );
        }
        if self
            .sip
            .transports
            .contains(&crate::config::SipTransport::Tls)
            && (self.sip.tls_cert_path.is_none() || self.sip.tls_key_path.is_none())
        {
            return Err(
                "sip.transports includes `tls` but tls_cert_path / tls_key_path are unset".into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LogFormat;

    fn base_config() -> Config {
        Config::default()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn current_returns_boot_config() {
        let reloader = ConfigReloader::new(base_config());
        let cur = reloader.current();
        assert_eq!(
            cur.observability.log_level,
            Config::default().observability.log_level
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_noop_returns_empty_report_and_no_pending() {
        let reloader = ConfigReloader::new(base_config());
        let receipt = reloader.apply(base_config(), 60).await.unwrap();
        assert!(receipt.report.is_noop());
        assert!(reloader.pending().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_reloadable_swaps_and_mints_pending() {
        let reloader = ConfigReloader::new(base_config());
        let mut next = base_config();
        next.observability.log_level = "debug".into();

        let receipt = reloader.apply(next, 60).await.unwrap();
        assert_eq!(receipt.report.reloaded, vec!["observability.log_level"]);
        assert!(receipt.report.restart_required.is_empty());
        assert_eq!(reloader.current().observability.log_level, "debug");

        let pending = reloader.pending().await.unwrap();
        assert_eq!(pending.id, receipt.id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_restart_required_does_not_swap() {
        let reloader = ConfigReloader::new(base_config());
        let mut next = base_config();
        next.observability.log_format = LogFormat::Pretty;

        let err = reloader.apply(next, 60).await.unwrap_err();
        assert!(matches!(err, ApplyError::RestartRequired { .. }));
        // Live config unchanged — still at the default.
        assert!(matches!(
            reloader.current().observability.log_format,
            LogFormat::Json
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn confirm_drops_pending() {
        let reloader = ConfigReloader::new(base_config());
        let mut next = base_config();
        next.observability.log_level = "trace".into();
        let receipt = reloader.apply(next, 60).await.unwrap();

        reloader.confirm(receipt.id).await.unwrap();
        assert!(reloader.pending().await.is_none());
        // Confirming twice is an error.
        let err = reloader.confirm(receipt.id).await.unwrap_err();
        assert!(matches!(err, CanaryError::UnknownChange(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rollback_restores_prior_snapshot() {
        let reloader = ConfigReloader::new(base_config());
        let mut next = base_config();
        next.observability.log_level = "trace".into();
        let receipt = reloader.apply(next, 60).await.unwrap();
        assert_eq!(reloader.current().observability.log_level, "trace");

        reloader
            .rollback(receipt.id, RollbackReason::Manual)
            .await
            .unwrap();
        // Restored to the boot default.
        assert_eq!(
            reloader.current().observability.log_level,
            Config::default().observability.log_level
        );
        assert!(reloader.pending().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rollback_wrong_id_leaves_pending_intact() {
        let reloader = ConfigReloader::new(base_config());
        let mut next = base_config();
        next.observability.log_level = "trace".into();
        let receipt = reloader.apply(next, 60).await.unwrap();

        let bogus = ChangeId(9_999);
        let err = reloader
            .rollback(bogus, RollbackReason::Manual)
            .await
            .unwrap_err();
        assert!(matches!(err, CanaryError::UnknownChange(_)));
        // Pending survives.
        assert!(reloader.pending().await.is_some());
        // The real id still works.
        reloader
            .rollback(receipt.id, RollbackReason::Manual)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_with_pending_refuses() {
        let reloader = ConfigReloader::new(base_config());
        let mut a = base_config();
        a.observability.log_level = "debug".into();
        let _rec_a = reloader.apply(a, 60).await.unwrap();

        let mut b = base_config();
        b.observability.log_level = "trace".into();
        let err = reloader.apply(b, 60).await.unwrap_err();
        assert!(matches!(err, ApplyError::Invalid(_)));
    }

    #[test]
    fn validate_catches_inconsistent_rate_limit() {
        let mut cfg = base_config();
        cfg.sip.rate_limit.per_sec = 10;
        cfg.sip.rate_limit.burst = 0;
        assert!(cfg.validate().is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribe_receives_applied_config() {
        let reloader = ConfigReloader::new(base_config());
        let mut rx = reloader.subscribe();
        // Initial value is the boot config.
        assert_eq!(
            rx.borrow().observability.log_level,
            Config::default().observability.log_level
        );

        let mut next = base_config();
        next.observability.log_level = "trace".into();
        let _receipt = reloader.apply(next, 60).await.unwrap();

        // `changed()` resolves once the reloader sent the new value.
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().observability.log_level, "trace");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_read_through_fires_only_on_actual_change() {
        use std::sync::Mutex as StdMutex;
        let reloader = ConfigReloader::new(base_config());
        let applied: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let applied_clone = Arc::clone(&applied);
        let handle = reloader.spawn_read_through(
            "observability.log_level",
            None,
            |c: &Config| c.observability.log_level.clone(),
            move |v: &String| applied_clone.lock().unwrap().push(v.clone()),
        );

        // Apply something that doesn't touch log_level — the
        // adapter should NOT fire.
        let mut other = base_config();
        other.ai.openai_api_key = Some("sk-test".into());
        reloader.apply(other, 60).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            applied.lock().unwrap().is_empty(),
            "adapter fired on an unrelated field change"
        );

        // Confirm to free the pending slot so the next apply
        // succeeds.
        let pending = reloader.pending().await.unwrap();
        reloader.confirm(pending.id).await.unwrap();

        // Apply a change to log_level — the adapter SHOULD fire.
        let mut next = base_config();
        next.observability.log_level = "debug".into();
        reloader.apply(next, 60).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let seen = applied.lock().unwrap().clone();
        assert_eq!(seen, vec!["debug".to_string()]);

        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn subscribe_receives_restored_config_on_rollback() {
        let reloader = ConfigReloader::new(base_config());
        let mut rx = reloader.subscribe();
        let mut next = base_config();
        next.observability.log_level = "debug".into();
        let receipt = reloader.apply(next, 60).await.unwrap();
        rx.changed().await.unwrap();
        assert_eq!(rx.borrow().observability.log_level, "debug");

        reloader
            .rollback(receipt.id, RollbackReason::Manual)
            .await
            .unwrap();
        rx.changed().await.unwrap();
        assert_eq!(
            rx.borrow().observability.log_level,
            Config::default().observability.log_level
        );
    }

    #[test]
    fn deadline_is_now_plus_canary_window() {
        let reloader = ConfigReloader::new(base_config());
        let start = current_unix_seconds();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut next = base_config();
            next.observability.log_level = "debug".into();
            let receipt = reloader.apply(next, 120).await.unwrap();
            // Within a few seconds of start+120.
            let deadline = receipt.deadline_at_unix;
            assert!(
                deadline >= start + 119 && deadline <= start + 125,
                "deadline {deadline} not within [{start}+119, {start}+125]"
            );
        });
    }
}
