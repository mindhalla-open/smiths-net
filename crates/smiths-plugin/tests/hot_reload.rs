//! End-to-end test for the plugin hot-reload watcher.
//!
//! Stages a shell-stub sidecar, loads it, then touches its
//! `plugin.toml` and verifies the watcher fires a reload. Uses the
//! same kind of stub as `loader.rs`'s tests so we don't need a real
//! plugin binary on disk.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use smiths_plugin::{AiRegistry, LoaderOpts, load_plugins, spawn_watcher};
use tempfile::tempdir;

const TTS_STUB: &str = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  case "$method" in
    describe_capabilities)
      printf '{"jsonrpc":"2.0","id":%s,"result":[{"capability":"ai.tts","plugin":"ai-tts-stub","model_id":"stub-v1","abi":"1.0"}]}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"unknown"}}\n' "$id"
      ;;
  esac
done
"#;

fn stage_stub(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        format!(
            "name     = \"{name}\"\n\
             version  = \"0.1.0\"\n\
             type     = \"sidecar\"\n\
             entry    = \"./main.sh\"\n\
             provides = [\"ai.tts\"]\n\
             abi      = \"1.0\"\n"
        ),
    )
    .unwrap();
    let script = dir.join("main.sh");
    fs::write(&script, TTS_STUB).unwrap();
    let mut p = fs::metadata(&script).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(&script, p).unwrap();
    dir
}

fn init_tracing() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_test_writer()
            .try_init();
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_triggers_reload_on_manifest_touch() {
    init_tracing();
    let root = tempdir().unwrap();
    let plugin_dir = stage_stub(root.path(), "ai-tts-stub");

    let registry = AiRegistry::new();
    let report = load_plugins(root.path(), &registry, LoaderOpts::default())
        .await
        .unwrap();
    assert_eq!(report.loaded, vec!["ai-tts-stub"]);

    // Capture the original entry Arc — a successful reload replaces
    // the registry's entry with a fresh `Arc<PluginEntry>`, so we can
    // spot the change by comparing `Arc::ptr_eq`.
    let before = registry.get("ai-tts-stub").expect("plugin entry");

    let handle = spawn_watcher(root.path(), Arc::new(registry.clone()));

    // Touch the manifest to trigger a reload. A file rewrite fires
    // Modify(Data) on most backends; append a harmless whitespace.
    let toml_path = plugin_dir.join("plugin.toml");
    let current = fs::read_to_string(&toml_path).unwrap();
    // Sleep briefly so the mtime bump is observable.
    tokio::time::sleep(Duration::from_millis(50)).await;
    fs::write(&toml_path, format!("{current}\n# touch\n")).unwrap();

    // Watcher debounces 250 ms; give it up to 5 s before failing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut reloaded = false;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(after) = registry.get("ai-tts-stub")
            && !Arc::ptr_eq(&before, &after)
        {
            reloaded = true;
            break;
        }
    }
    assert!(
        reloaded,
        "watcher should have reloaded ai-tts-stub after the manifest touch"
    );

    handle.shutdown().await;
    registry.shutdown_all().await;
}
