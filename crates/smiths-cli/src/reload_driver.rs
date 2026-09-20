//! Config hot-reload: the SIGHUP driver, the canary watchdogs armed
//! on every apply, and the per-subsystem read-through adapters.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smiths_core::probe::{ErrorRateProbe, ProbeConfig};
use smiths_core::{
    BuildSupport, ChangeReceipt, Config, ConfigReloader, Metrics, ReloadConfig, SipRateLimit,
    WebRtcPrivacyConfig, hangup_stream,
};
use smiths_transcode::CpuBudget;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::logging::LogReloader;
use crate::replication_service::ReplicationState;

/// Which optional runtimes this binary can serve. Every field stays
/// `false`: the `sip-quic`, `mcp-http3` and `wireguard` Cargo
/// features only reserve the config surface — no build ships the
/// listener / tunnel behind them, and the sidecar storage adapters
/// and WebRTC TLS termination do not exist yet. Flip a field only
/// together with the runtime that honors it.
pub(crate) fn build_support() -> BuildSupport {
    BuildSupport::default()
}

/// Why a SIGHUP was not acted on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReloadRefusal {
    /// `[reload] enabled = false` in the live config.
    Disabled,
    /// A reload ran less than `max_frequency_s` ago.
    TooFrequent {
        /// Time left until the next signal would be accepted.
        retry_in: Duration,
    },
}

/// `[reload]` gate: honors `enabled` and throttles by
/// `max_frequency_s`, both read from the live config on every
/// signal so they are themselves hot-reloadable.
#[derive(Debug, Default)]
pub(crate) struct ReloadThrottle {
    last_attempt: Option<Instant>,
}

impl ReloadThrottle {
    /// Decide whether a reload may run `now`; records the attempt
    /// when admitted.
    pub(crate) fn admit(&mut self, cfg: &ReloadConfig, now: Instant) -> Result<(), ReloadRefusal> {
        if !cfg.enabled {
            return Err(ReloadRefusal::Disabled);
        }
        if cfg.max_frequency_s > 0
            && let Some(last) = self.last_attempt
        {
            let min_gap = Duration::from_secs(cfg.max_frequency_s);
            let elapsed = now.saturating_duration_since(last);
            if elapsed < min_gap {
                return Err(ReloadRefusal::TooFrequent {
                    retry_in: min_gap.saturating_sub(elapsed),
                });
            }
        }
        self.last_attempt = Some(now);
        Ok(())
    }
}

/// Reload `path` through validate → `ConfigReloader::apply` and arm
/// the canary watchdogs on success. Shared by the SIGHUP driver.
pub(crate) async fn reload_from_file(
    path: &Path,
    reloader: &Arc<ConfigReloader>,
    metrics: &Arc<Metrics>,
) {
    let candidate = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(?e, "config reload: load failed; prior config kept");
            return;
        }
    };
    if let Err(e) = candidate.validate_with(&build_support()) {
        warn!(%e, "config reload: validation failed; prior config kept");
        return;
    }
    let deadline = candidate.canary.deadline_s;
    let probe_cfg = ProbeConfig::from(&candidate.canary);
    match reloader.apply(candidate, deadline).await {
        Ok(receipt) if receipt.report.is_noop() => {
            info!(%receipt.id, "config reload: no-op");
        }
        Ok(receipt) => {
            info!(
                %receipt.id,
                reloaded = ?receipt.report.reloaded,
                deadline_secs = deadline,
                "config reload: canary window armed"
            );
            spawn_canary_watchdogs(
                Arc::clone(reloader),
                Arc::clone(metrics),
                &receipt,
                probe_cfg,
            );
        }
        Err(e) => warn!(%e, "config reload: apply rejected"),
    }
}

/// Reload the config file on every POSIX SIGHUP, subject to the
/// live `[reload]` gate. `None` where the platform has no SIGHUP.
pub(crate) fn spawn_sighup_driver(
    path: PathBuf,
    reloader: Arc<ConfigReloader>,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) -> Option<JoinHandle<()>> {
    let Some(mut hup) = hangup_stream() else {
        info!("SIGHUP reload unavailable on this platform");
        return None;
    };
    Some(tokio::spawn(async move {
        let mut throttle = ReloadThrottle::default();
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                sig = hup.recv() => {
                    if sig.is_none() { return; }
                }
            }
            let live = reloader.current();
            match throttle.admit(&live.reload, Instant::now()) {
                Ok(()) => {}
                Err(ReloadRefusal::Disabled) => {
                    warn!("SIGHUP received but [reload] enabled = false; ignored");
                    continue;
                }
                Err(ReloadRefusal::TooFrequent { retry_in }) => {
                    warn!(
                        retry_in_secs = retry_in.as_secs(),
                        max_frequency_s = live.reload.max_frequency_s,
                        "SIGHUP reload refused: faster than [reload] max_frequency_s"
                    );
                    continue;
                }
            }
            info!(path = %path.display(), "SIGHUP received; reloading config");
            reload_from_file(&path, &reloader, &metrics).await;
        }
    }))
}

/// Arm the canary watchdogs on a freshly-applied receipt: the
/// deadline timer plus the error-rate probe. Whichever resolves
/// first (operator confirm, deadline, probe trip) cancels the other
/// through a shared token; engine shutdown cancels both through the
/// reloader's own token.
pub(crate) fn spawn_canary_watchdogs(
    reloader: Arc<ConfigReloader>,
    metrics: Arc<Metrics>,
    receipt: &ChangeReceipt,
    probe_cfg: ProbeConfig,
) {
    let cancel = CancellationToken::new();
    let timer_cancel = cancel.clone();
    let deadline_handle = reloader.spawn_auto_rollback(receipt, Some(Arc::clone(&metrics)));
    tokio::spawn(async move {
        let _ = deadline_handle.await;
        timer_cancel.cancel();
    });
    let probe = ErrorRateProbe::new(metrics, probe_cfg);
    let _probe_handle = probe.spawn(reloader, receipt, cancel);
}

/// Subsystem handles the read-through adapters update.
pub(crate) struct AdapterTargets {
    pub log_reloader: LogReloader,
    pub sip_rate_limit: smiths_sip::SipRateLimiter,
    pub cpu_budget: CpuBudget,
    pub prompt_library: Option<smiths_media::PromptLibrary>,
    pub ai_registry: smiths_plugin::AiRegistry,
    pub webrtc_privacy: Option<Arc<std::sync::Mutex<WebRtcPrivacyConfig>>>,
    pub replication: Option<Arc<ReplicationState>>,
}

/// Spawn one read-through adapter per hot-reloadable subsystem
/// value. Each fires only when its extracted value actually changed
/// and bumps `smiths_config_reloaded_fields_total{field}`.
pub(crate) fn wire_read_through_adapters(
    reloader: &Arc<ConfigReloader>,
    metrics: &Arc<Metrics>,
    targets: AdapterTargets,
) -> Vec<JoinHandle<()>> {
    let mut handles = Vec::new();
    let m = || Some(Arc::clone(metrics));

    let log_reloader = targets.log_reloader;
    handles.push(reloader.spawn_read_through(
        "observability.log_level",
        m(),
        |c: &Config| c.observability.log_level.clone(),
        move |new_level: &String| match log_reloader(new_level.as_str()) {
            Ok(()) => info!(new_level = %new_level, "log filter reloaded"),
            Err(e) => warn!(?e, new_level = %new_level,
                "log filter reload rejected; keeping the prior filter"),
        },
    ));

    let limiter = targets.sip_rate_limit;
    handles.push(reloader.spawn_read_through(
        "sip.rate_limit",
        m(),
        |c: &Config| c.sip.rate_limit,
        move |new_cfg: &SipRateLimit| {
            limiter.reconfigure(*new_cfg);
            info!(
                per_sec = new_cfg.per_sec,
                burst = new_cfg.burst,
                "sip.rate_limit reconfigured"
            );
        },
    ));

    let budget = targets.cpu_budget;
    handles.push(reloader.spawn_read_through(
        "media.transcode",
        m(),
        |c: &Config| c.media.transcode.max_concurrent_calls,
        move |new_cap: &usize| {
            budget.set_max_concurrent(*new_cap);
            info!(
                max_concurrent_calls = *new_cap,
                "transcode CpuBudget cap reconfigured"
            );
        },
    ));

    if let Some(library) = targets.prompt_library {
        handles.push(reloader.spawn_read_through(
            "media.prompts.capacity",
            m(),
            |c: &Config| c.media.prompts.capacity,
            move |new_cap: &usize| {
                library.resize(*new_cap);
                info!(capacity = *new_cap, "prompt library resized");
            },
        ));
    }

    // Rotated AI keys reach sidecars on their next respawn.
    for (field, env_key) in [
        ("ai.openai_api_key", "OPENAI_API_KEY"),
        ("ai.anthropic_api_key", "ANTHROPIC_API_KEY"),
    ] {
        let reg = targets.ai_registry.clone();
        let extract: fn(&Config) -> Option<String> = match env_key {
            "OPENAI_API_KEY" => |c: &Config| c.ai.openai_api_key.clone(),
            _ => |c: &Config| c.ai.anthropic_api_key.clone(),
        };
        handles.push(reloader.spawn_read_through(
            field,
            m(),
            extract,
            move |val: &Option<String>| {
                match val {
                    Some(v) => reg.set_env(env_key, v.clone()),
                    None => reg.clear_env(env_key),
                }
                info!(%field, %env_key, "AI credential snapshot rotated; next sidecar respawn inherits");
            },
        ));
    }

    if let Some(privacy) = targets.webrtc_privacy {
        handles.push(reloader.spawn_read_through(
            "webrtc.privacy",
            m(),
            |c: &Config| c.webrtc.privacy.clone(),
            move |new_cfg: &WebRtcPrivacyConfig| {
                let mut guard = privacy
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *guard = new_cfg.clone();
                info!(mode = ?new_cfg.mode, "webrtc.privacy reloaded");
            },
        ));
    }

    if let Some(state) = targets.replication {
        handles.push(reloader.spawn_read_through(
            "cluster.heartbeat_interval_secs",
            m(),
            |c: &Config| c.cluster.heartbeat_interval_secs,
            move |secs: &u32| {
                state.set_heartbeat_secs(u64::from(*secs));
                info!(secs, "cluster.heartbeat_interval_secs reconfigured");
            },
        ));
    }

    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, max_frequency_s: u64) -> ReloadConfig {
        ReloadConfig {
            enabled,
            max_frequency_s,
        }
    }

    #[test]
    fn throttle_refuses_when_reload_disabled() {
        let mut t = ReloadThrottle::default();
        assert_eq!(
            t.admit(&cfg(false, 10), Instant::now()),
            Err(ReloadRefusal::Disabled)
        );
    }

    #[test]
    fn throttle_enforces_max_frequency() {
        let mut t = ReloadThrottle::default();
        let t0 = Instant::now();
        assert_eq!(t.admit(&cfg(true, 10), t0), Ok(()));
        match t.admit(&cfg(true, 10), t0 + Duration::from_secs(3)) {
            Err(ReloadRefusal::TooFrequent { retry_in }) => {
                assert_eq!(retry_in, Duration::from_secs(7));
            }
            other => panic!("expected TooFrequent, got {other:?}"),
        }
        assert_eq!(
            t.admit(&cfg(true, 10), t0 + Duration::from_secs(10)),
            Ok(())
        );
    }

    #[test]
    fn zero_max_frequency_disables_throttle() {
        let mut t = ReloadThrottle::default();
        let t0 = Instant::now();
        assert_eq!(t.admit(&cfg(true, 0), t0), Ok(()));
        assert_eq!(t.admit(&cfg(true, 0), t0), Ok(()));
    }

    #[test]
    fn build_support_advertises_no_optional_runtime() {
        assert_eq!(build_support(), BuildSupport::default());
    }
}
