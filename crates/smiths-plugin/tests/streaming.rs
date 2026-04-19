//! Bidirectional RPC end-to-end: load `ai-asr-mock`, invoke
//! `transcribe` with `controls.stream = true`, watch `emit_partial`
//! notifications land on the engine's bus as `PluginEvent`.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use smiths_core::{Event, EventBus, PluginEvent};
use smiths_plugin::{AiRegistry, load_plugins};
use tokio::time::timeout;

fn asr_mock_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("plugins/examples/ai-asr-mock"))
        .expect("workspace root")
}

/// Copy `ai-asr-mock` into a fresh tempdir so the loader scans a
/// clean single-plugin tree (keeps the test independent of other
/// example plugins that happen to live alongside it).
fn stage_mock_in_temp() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("ai-asr-mock");
    std::fs::create_dir_all(&dest).unwrap();
    for entry in std::fs::read_dir(asr_mock_src()).unwrap() {
        let e = entry.unwrap();
        let target = dest.join(e.file_name());
        std::fs::copy(e.path(), &target).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let main = dest.join("main.py");
        let mut p = std::fs::metadata(&main).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&main, p).unwrap();
    }
    tmp
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_transcribe_emits_partials_on_bus() {
    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let registry = AiRegistry::new();
    let tmp = stage_mock_in_temp();

    let opts = smiths_plugin::LoaderOpts {
        bus: Some(bus.clone()),
        wasm_engine: None,
    };
    let report = load_plugins(tmp.path(), &registry, opts).await.unwrap();
    assert!(
        report.failed.is_empty(),
        "load failures: {:?}",
        report.failed
    );
    assert!(report.loaded.iter().any(|n| n == "ai-asr-mock"));

    // Grab the provider and invoke streaming transcribe.
    let entry = registry.get("ai-asr-mock").expect("plugin entry");
    let audio_bytes = vec![0u8; 32_000]; // 2 s of PCM16 @ 8 kHz
    let params = json!({
        "audio_base64": base64::engine::general_purpose::STANDARD.encode(&audio_bytes),
        "sample_rate":  8000,
        "language":     "auto",
        "controls":     { "stream": true },
        "call_id":      "stream-1",
    });
    let final_result: Value = entry
        .sidecar
        .call("transcribe", params)
        .await
        .expect("transcribe");

    // Final response shape is what transcribe always returns.
    assert!(final_result["text"].is_string());

    // The plugin emits two `emit_partial` notifications before the
    // final. Collect them off the bus. Allow other events through
    // (e.g. future call lifecycle noise) but fail out on timeout.
    let mut partials: Vec<Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while partials.len() < 2 && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(Event::Plugin(PluginEvent::Notification {
            plugin,
            method,
            params,
        }))) = timeout(Duration::from_millis(250), rx.recv()).await
        {
            assert_eq!(plugin, "ai-asr-mock");
            assert_eq!(method, "emit_partial");
            partials.push(params.unwrap_or(Value::Null));
        }
    }
    assert_eq!(
        partials.len(),
        2,
        "expected 2 emit_partial notifications, got {}",
        partials.len()
    );
    assert_eq!(partials[0]["is_final"], false);
    assert_eq!(partials[0]["call_id"], "stream-1");
    assert!(!partials[0]["text"].as_str().unwrap().is_empty());
    // Second partial is the fuller fragment (cumulative).
    let first_len = partials[0]["text"].as_str().unwrap().len();
    let second_len = partials[1]["text"].as_str().unwrap().len();
    assert!(
        second_len >= first_len,
        "second partial should be at least as long"
    );

    registry.shutdown_all().await;
}
