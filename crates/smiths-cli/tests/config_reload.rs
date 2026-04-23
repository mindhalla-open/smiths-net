//! Slice 5.8-b Small 3 + 5.9 Small 3 integration tests.
//!
//! Each test pairs a real subsystem (the same handle shape the
//! CLI wires to `ConfigReloader::spawn_read_through`) with a
//! live reloader + a config mutation, then asserts the
//! subsystem's state reflects the new value within a single
//! watch tick. The 5.9 test intentionally bumps the plugin-
//! error aggregate counter past the canary ceiling and asserts
//! that `config_rollbacks_total{reason="error_budget"}` bumps
//! on the probe's next sample.

use std::sync::Arc;
use std::time::Duration;

use smiths_core::probe::{ErrorRateProbe, ProbeConfig};
use smiths_core::{Config, ConfigReloader, Metrics, SipRateLimit};
use smiths_media::PromptLibrary;
use smiths_plugin::AiRegistry;
use smiths_transcode::{CpuBudget, CpuBudgetConfig, TranscodeMetrics};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Grace period we give each adapter between `apply()` and the
/// assertion — the reloader's watch tick fires on the next
/// scheduler hop; 50 ms is generous.
const TICK_GRACE: Duration = Duration::from_millis(50);

#[tokio::test(flavor = "multi_thread")]
async fn sip_rate_limit_read_through_applies_new_thresholds() {
    let reloader = ConfigReloader::new(Config::default());
    let limiter = smiths_sip::SipRateLimiter::new(Config::default().sip.rate_limit);
    let adapter = {
        let l = limiter.clone();
        reloader.spawn_read_through(
            "sip.rate_limit",
            None,
            |c: &Config| c.sip.rate_limit,
            move |new: &SipRateLimit| l.reconfigure(*new),
        )
    };

    let mut next = Config::default();
    next.sip.rate_limit = SipRateLimit {
        per_sec: 500,
        burst: 1_000,
    };
    reloader.apply(next, 60).await.unwrap();
    sleep(TICK_GRACE).await;

    assert_eq!(limiter.rate_per_sec(), 500);
    assert_eq!(limiter.burst(), 1_000);
    adapter.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn cpu_budget_read_through_applies_new_cap() {
    let reloader = ConfigReloader::new(Config::default());
    let budget = CpuBudget::new(
        CpuBudgetConfig::from(&Config::default().media.transcode),
        TranscodeMetrics::noop(),
    );
    let adapter = {
        let b = budget.clone();
        reloader.spawn_read_through(
            "media.transcode",
            None,
            |c: &Config| c.media.transcode.max_concurrent_calls,
            move |new: &usize| b.set_max_concurrent(*new),
        )
    };

    let mut next = Config::default();
    next.media.transcode.max_concurrent_calls = 128;
    reloader.apply(next, 60).await.unwrap();
    sleep(TICK_GRACE).await;

    assert_eq!(budget.max_concurrent(), 128);
    adapter.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn prompt_library_read_through_resizes_lru() {
    let reloader = ConfigReloader::new(Config::default());
    let library = PromptLibrary::with_root("").with_capacity(8);
    let adapter = {
        let lib = library.clone();
        reloader.spawn_read_through(
            "media.prompts.capacity",
            None,
            |c: &Config| c.media.prompts.capacity,
            move |new: &usize| lib.resize(*new),
        )
    };
    assert_eq!(library.capacity(), 8);

    let mut next = Config::default();
    next.media.prompts.capacity = 32;
    reloader.apply(next, 60).await.unwrap();
    sleep(TICK_GRACE).await;

    assert_eq!(library.capacity(), 32);
    adapter.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_key_read_through_refreshes_env_snapshot() {
    let reloader = ConfigReloader::new(Config::default());
    let registry = AiRegistry::new();
    let adapter = {
        let reg = registry.clone();
        reloader.spawn_read_through(
            "ai.openai_api_key",
            None,
            |c: &Config| c.ai.openai_api_key.clone(),
            move |val: &Option<String>| match val {
                Some(v) => reg.set_env("OPENAI_API_KEY", v.clone()),
                None => reg.clear_env("OPENAI_API_KEY"),
            },
        )
    };
    assert!(!registry.env_snapshot().contains_key("OPENAI_API_KEY"));

    let mut next = Config::default();
    next.ai.openai_api_key = Some("sk-rotated".into());
    reloader.apply(next, 60).await.unwrap();
    sleep(TICK_GRACE).await;

    assert_eq!(
        registry.env_snapshot().get("OPENAI_API_KEY"),
        Some(&"sk-rotated".to_owned())
    );
    adapter.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn log_level_read_through_runs_the_adapter_closure() {
    // The real `init_tracing` path can only be installed once per
    // process, so instead of round-tripping through
    // `tracing_subscriber::reload::Handle` we install a shim
    // `Arc<Mutex<Option<String>>>` as the apply sink and verify
    // the adapter saw the new level — the handle wiring itself
    // is already covered by `tracing-subscriber`'s own tests.
    use std::sync::Mutex as StdMutex;
    let reloader = ConfigReloader::new(Config::default());
    let applied: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
    let adapter = {
        let sink = Arc::clone(&applied);
        reloader.spawn_read_through(
            "observability.log_level",
            None,
            |c: &Config| c.observability.log_level.clone(),
            move |new_level: &String| {
                *sink.lock().unwrap() = Some(new_level.clone());
            },
        )
    };

    let mut next = Config::default();
    next.observability.log_level = "trace".into();
    reloader.apply(next, 60).await.unwrap();
    sleep(TICK_GRACE).await;

    assert_eq!(applied.lock().unwrap().as_deref(), Some("trace"));
    adapter.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn error_rate_probe_rolls_back_on_ceiling() {
    // Slice 5.9 Small 3: apply a canary, bump the plugin-error
    // aggregate past the ceiling, verify the probe fires a
    // rollback credited to `error_budget`.
    let mut scratch = prometheus_client::registry::Registry::default();
    let metrics = Metrics::register(&mut scratch);
    let reloader = ConfigReloader::new(Config::default());

    // Change a reloadable field so the apply produces a pending
    // change (a no-op apply wouldn't arm the canary).
    let mut next = Config::default();
    next.observability.log_level = "trace".into();
    let receipt = reloader.apply(next, 60).await.unwrap();
    assert!(!receipt.report.is_noop());

    let probe = ErrorRateProbe::new(
        Arc::clone(&metrics),
        ProbeConfig {
            // Zero ceiling → any non-zero error rate trips.
            plugin_error_rate_ceiling: 0.0,
            sip_parse_errors_per_sec_ceiling: u64::MAX,
        },
    );
    let cancel = CancellationToken::new();
    let handle = probe.spawn(Arc::clone(&reloader), &receipt, cancel.clone());

    // The probe's first tick fires immediately and captures the
    // baseline (zero errors). Wait one full tick before driving
    // the aggregate so the next sample sees a non-zero *delta*
    // — `classify` works on in-window deltas, not absolute
    // counters, so bumping before the baseline is taken would
    // register as "no activity" and never trip.
    sleep(Duration::from_millis(1_200)).await;
    for _ in 0..50 {
        metrics.plugin_invocations_error.inc();
    }

    // At ceiling=0.0 and 100 % error rate, the probe trips on
    // the next sample (≤ 1 s later). 5 s gives generous headroom
    // for busy test runners.
    let verdict = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("probe timed out")
        .expect("probe task panicked");
    assert_eq!(verdict, smiths_core::probe::ProbeVerdict::PluginErrorRate);

    // The rollback must have bumped the reason counter + cleared
    // the pending slot.
    assert!(reloader.pending().await.is_none());
    let rolled = metrics
        .config_rollbacks
        .get_or_create(&smiths_core::metrics::ConfigRollbackLabel {
            reason: "error_budget".into(),
        })
        .get();
    assert!(rolled >= 1, "error_budget rollback counter = {rolled}");
    let probe_label = metrics
        .config_probe_triggered
        .get_or_create(&smiths_core::metrics::ConfigProbeLabel {
            probe: "plugin_error_rate".into(),
        })
        .get();
    assert!(probe_label >= 1, "probe_triggered counter = {probe_label}");
}
