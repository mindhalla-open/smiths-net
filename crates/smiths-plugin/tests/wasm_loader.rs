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
