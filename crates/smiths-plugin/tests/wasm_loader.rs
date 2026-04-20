//! Integration test: `load_plugins` with a `type = "wasm"` manifest.
//!
//! Stages a tiny WAT-compiled module that exports `describe()` — the
//! bytes at the pointer it returns parse as a valid
//! `CapabilityDescriptor`. The test asserts the plugin ends up in the
//! registry and its capability is surfaced through
//! `AiRegistry::capabilities()`, proving the WASM tier is wired
//! end-to-end through the loader.

use std::fs;
use std::path::Path;

use smiths_plugin::wasm::WasmEngine;
use smiths_plugin::{AiRegistry, LoaderOpts, load_plugins};
use tempfile::tempdir;

/// Inline WAT that advertises one `ai.log` capability. The descriptor
/// JSON lives in a data segment at offset 0; `describe` returns
/// `(0 << 32) | len` = `len` (in this case).
const DESCRIBE_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{\"capability\":\"ai.log\",\"plugin\":\"wat-logger\",\"abi\":\"1.0\"}")
  (func (export "describe") (result i64)
    ;; packed = (ptr << 32) | len  — ptr is 0, len is 57
    i64.const 57))
"#;

fn stage_plugin(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let wasm = wat::parse_str(DESCRIBE_WAT).expect("WAT parse");
    fs::write(dir.join("plugin.wasm"), &wasm).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        "name        = \"wat-logger\"\n\
         version     = \"0.1.0\"\n\
         type        = \"wasm\"\n\
         entry       = \"./plugin.wasm\"\n\
         provides    = [\"ai.log\"]\n\
         abi         = \"1.0\"\n\
         description = \"Inline-WAT wasm plugin for loader tests.\"\n",
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn loads_wasm_plugin_and_surfaces_capability() {
    let root = tempdir().unwrap();
    stage_plugin(&root.path().join("wat-logger"));

    let reg = AiRegistry::new();
    let engine = WasmEngine::new().expect("WasmEngine::new");
    let opts = LoaderOpts {
        bus: None,
        wasm_engine: Some(engine),
        metrics: None,
        ..Default::default()
    };
    let report = load_plugins(root.path(), &reg, opts).await.unwrap();
    assert!(
        report.failed.is_empty(),
        "load failures: {:?}",
        report.failed
    );
    assert_eq!(report.loaded, vec!["wat-logger"]);

    let caps = reg.capabilities();
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].capability, "ai.log");
    assert_eq!(caps[0].plugin, "wat-logger");

    reg.shutdown_all().await;
}

/// Inline WAT with full `describe` + `alloc` + `invoke` surface. The
/// `invoke` export always returns `{"result":"ok"}`. Used by the
/// end-to-end test that exercises the `AiProvider::invoke` trampoline.
const INVOKE_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  ;; Descriptor JSON at offset 0, length 57.
  (data (i32.const 0) "{\"capability\":\"ai.log\",\"plugin\":\"wat-logger\",\"abi\":\"1.0\"}")
  (func (export "describe") (result i64)
    i64.const 57)
  ;; Response envelope at offset 128, length 15.
  (data (i32.const 128) "{\"result\":\"ok\"}")
  (global $HEAP (mut i32) (i32.const 512))
  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $HEAP))
    (global.set $HEAP (i32.add (global.get $HEAP) (local.get $len)))
    (local.get $ptr))
  (func (export "invoke") (param i32 i32 i32 i32) (result i64)
    ;; (128 << 32) | 15 = 549_755_813_903
    i64.const 549755813903))
"#;

fn stage_invoke_plugin(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    let wasm = wat::parse_str(INVOKE_WAT).expect("WAT parse");
    fs::write(dir.join("plugin.wasm"), &wasm).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        "name        = \"wat-logger\"\n\
         version     = \"0.1.0\"\n\
         type        = \"wasm\"\n\
         entry       = \"./plugin.wasm\"\n\
         provides    = [\"ai.log\"]\n\
         abi         = \"1.0\"\n\
         description = \"Inline-WAT wasm plugin with full invoke surface.\"\n",
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn invoke_round_trips_end_to_end_through_provider() {
    let root = tempdir().unwrap();
    stage_invoke_plugin(&root.path().join("wat-logger"));

    let reg = AiRegistry::new();
    let engine = WasmEngine::new().expect("WasmEngine::new");
    let opts = LoaderOpts {
        bus: None,
        wasm_engine: Some(engine),
        metrics: None,
        ..Default::default()
    };
    let report = load_plugins(root.path(), &reg, opts).await.unwrap();
    assert!(
        report.failed.is_empty(),
        "load failures: {:?}",
        report.failed
    );

    // Fetch the provider through the trait seam and invoke it.
    let provider = smiths_core::ai::AiRegistry::get(&reg, "wat-logger")
        .expect("wat-logger should be registered");
    let out = provider
        .invoke("ping", serde_json::json!({"msg": "hello"}))
        .await
        .expect("invoke should succeed");
    assert_eq!(out, serde_json::Value::String("ok".into()));

    reg.shutdown_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_plugin_without_engine_fails_partial() {
    let root = tempdir().unwrap();
    stage_plugin(&root.path().join("wat-logger"));

    let reg = AiRegistry::new();
    // No engine passed — loader must surface a descriptive failure
    // instead of panicking, and the registry stays empty.
    let report = load_plugins(root.path(), &reg, LoaderOpts::default())
        .await
        .unwrap();
    assert!(report.loaded.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert!(
        report.failed[0].1.contains("WasmEngine"),
        "expected WasmEngine-related error, got {}",
        report.failed[0].1
    );
    assert_eq!(reg.len(), 0);
}
