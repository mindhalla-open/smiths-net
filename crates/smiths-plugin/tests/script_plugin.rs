//! Slice 4.1 acceptance: load the `route-rhai` reference plugin
//! through the standard loader, invoke `route(req)` through the
//! `AiProvider` trait surface, and verify the hot-reload rollback
//! fires after five consecutive errors.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use smiths_core::ai::{AiProvider, AiRegistry as AiRegistryTrait};
use smiths_plugin::{AiRegistry, LoaderOpts, ROLLBACK_AFTER, load_plugins};

fn route_rhai_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("plugins/examples/route-rhai"))
        .expect("workspace root")
}

fn stage_in_temp(src: &std::path::Path) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join(src.file_name().unwrap());
    std::fs::create_dir_all(&dest).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let e = entry.unwrap();
        let target = dest.join(e.file_name());
        std::fs::copy(e.path(), &target).unwrap();
    }
    tmp
}

#[tokio::test(flavor = "multi_thread")]
async fn route_rhai_loads_and_routes_support_to_pool() {
    let registry = AiRegistry::new();
    let tmp = stage_in_temp(&route_rhai_src());
    let report = load_plugins(tmp.path(), &registry, LoaderOpts::default())
        .await
        .unwrap();
    assert!(
        report.failed.is_empty(),
        "load failures: {:?}",
        report.failed
    );
    assert!(report.loaded.iter().any(|n| n == "route-rhai"));

    let provider: Arc<dyn AiProvider> =
        AiRegistryTrait::get(&registry, "route-rhai").expect("provider registered");
    let caps = provider.capabilities();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].capability, "routing.dialplan");

    // Positive case — `support` maps to the queue pool.
    let out: Value = provider
        .invoke(
            "route",
            json!({
                "from": "sip:alice@example.com",
                "to": "sip:support@smiths.local",
                "call_id": "abc@smiths.local",
            }),
        )
        .await
        .expect("route ok");
    assert_eq!(out["target"], "sip:queue-support@pbx.internal");
    assert!(
        out["reason"].as_str().unwrap().contains("support"),
        "{out:?}"
    );

    // Negative case — unknown user returns `#{}` (empty object).
    let fallthrough: Value = provider
        .invoke(
            "route",
            json!({ "from": "x", "to": "sip:nobody@smiths.local", "call_id": "y" }),
        )
        .await
        .expect("route ok");
    assert!(fallthrough.as_object().unwrap().is_empty());

    registry.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn script_rollback_restores_previous_runtime_after_five_errors() {
    // Direct smiths-plugin surface (no file system) — build a
    // ScriptProvider with a healthy script, swap in a broken one,
    // then observe rollback after 5 invokes.
    use smiths_plugin::ScriptProvider;
    use smiths_script::{ScriptLimits, ScriptRuntime};

    let good = ScriptRuntime::from_source_rhai(
        "rollback-test",
        r#"
            fn describe_capabilities() {
                [#{
                    "capability": "routing.dialplan",
                    "plugin": "rollback-test",
                    "abi": "1.0",
                }]
            }
            fn route(req) { #{ "target": "good", "score": 1 } }
        "#
        .into(),
        PathBuf::from("good.rhai"),
        ScriptLimits::default(),
    )
    .unwrap();
    let caps = smiths_core::ai::parse_descriptors(good.describe().await.unwrap()).unwrap();

    let provider = Arc::new(ScriptProvider::new(
        "rollback-test".into(),
        "0.1.0".into(),
        "test".into(),
        "1.0".into(),
        caps,
        good,
        PathBuf::from("/tmp/not-used"),
        ScriptLimits::default(),
        None,
    ));

    // Working version routes fine.
    let ok = provider
        .invoke("route", json!({}))
        .await
        .expect("good script answers");
    assert_eq!(ok["target"], "good");

    // Hot-swap a broken script that always throws.
    let broken = ScriptRuntime::from_source_rhai(
        "rollback-test",
        r#"
            fn describe_capabilities() {
                [#{ "capability": "routing.dialplan", "plugin": "rollback-test", "abi": "1.0" }]
            }
            fn route(req) { throw "boom" }
        "#
        .into(),
        PathBuf::from("broken.rhai"),
        ScriptLimits::default(),
    )
    .unwrap();
    provider.swap_runtime(broken);

    // Every call errors until ROLLBACK_AFTER restores the previous
    // runtime. The 5th failure is the trigger; the 6th call then
    // routes through the restored good script.
    let mut last_err: Option<String> = None;
    for _ in 0..ROLLBACK_AFTER {
        let err = provider
            .invoke("route", json!({}))
            .await
            .unwrap_err()
            .to_string();
        last_err = Some(err);
    }
    assert!(last_err.unwrap().contains("boom"));

    // Give the Tokio runtime a beat — rollback happens
    // synchronously inside the invoke path, but some async
    // bookkeeping from the prior calls might still be resolving.
    tokio::time::sleep(Duration::from_millis(5)).await;

    let recovered = provider
        .invoke("route", json!({}))
        .await
        .expect("post-rollback call succeeds against good runtime");
    assert_eq!(recovered["target"], "good");
    assert_eq!(provider.consecutive_errors(), 0);
}
