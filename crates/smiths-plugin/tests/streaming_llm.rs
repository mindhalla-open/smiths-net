//! Bidirectional RPC end-to-end for `ai.llm.chat`: load
//! `ai-llm-mock`, invoke `chat` with `controls.stream = true`, and
//! verify the plugin's `ai.llm.partial` notifications reach the
//! engine's bus as `PluginEvent::Notification`.
//!
//! Same contract the cloud sidecars (`ai-llm-openai` /
//! `ai-llm-anthropic`, slice 3.2) use for real streaming completions
//! — the mock is the one plugin we can exercise in CI without an API
//! key on disk.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use smiths_core::{Event, EventBus, PluginEvent};
use smiths_plugin::{AiRegistry, load_plugins};
use tokio::time::timeout;

fn llm_mock_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("plugins/examples/ai-llm-mock"))
        .expect("workspace root")
}

fn stage_mock_in_temp() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("ai-llm-mock");
    std::fs::create_dir_all(&dest).unwrap();
    for entry in std::fs::read_dir(llm_mock_src()).unwrap() {
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
async fn streaming_chat_emits_partials_on_bus() {
    let bus = EventBus::new(64);
    let mut rx = bus.subscribe();
    let registry = AiRegistry::new();
    let tmp = stage_mock_in_temp();

    let opts = smiths_plugin::LoaderOpts {
        bus: Some(bus.clone()),
        wasm_engine: None,
        metrics: None,
        ..Default::default()
    };
    let report = load_plugins(tmp.path(), &registry, opts).await.unwrap();
    assert!(
        report.failed.is_empty(),
        "load failures: {:?}",
        report.failed
    );
    assert!(report.loaded.iter().any(|n| n == "ai-llm-mock"));

    let entry = registry.get("ai-llm-mock").expect("plugin entry");
    let params = json!({
        "messages": [
            {"role": "user", "content": "hello there friend"},
        ],
        "controls": { "stream": true },
    });
    let final_result: Value = entry.sidecar.call("chat", params).await.expect("chat");

    // Final response carries usage + the full content.
    assert!(final_result["message"]["content"].is_string());
    assert!(final_result["usage"]["output_tokens"].as_u64().unwrap() > 0);

    // Collect `ai.llm.partial` notifications off the bus until we
    // see the `is_final = true` terminator. The mock emits one
    // partial per output token plus the final marker, so for the
    // canned "Hello, how can I help you today?" reply we expect 8+1.
    let mut partials: Vec<Value> = Vec::new();
    let mut saw_final = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !saw_final && tokio::time::Instant::now() < deadline {
        if let Ok(Ok(Event::Plugin(PluginEvent::Notification {
            plugin,
            method,
            params,
        }))) = timeout(Duration::from_millis(300), rx.recv()).await
        {
            assert_eq!(plugin, "ai-llm-mock");
            if method == "ai.llm.partial" {
                let p = params.unwrap_or(Value::Null);
                if p["is_final"] == true {
                    saw_final = true;
                }
                partials.push(p);
            }
        }
    }
    assert!(
        saw_final,
        "never saw is_final=true terminator; partials collected = {}",
        partials.len()
    );
    assert!(
        partials.len() >= 2,
        "expected at least 2 partials (intermediate + final), got {}",
        partials.len()
    );
    // Intermediate partials are cumulative — each `text` is a prefix
    // of the next, monotonically non-decreasing in length.
    for win in partials.windows(2) {
        let a = win[0]["text"].as_str().unwrap();
        let b = win[1]["text"].as_str().unwrap();
        assert!(
            b.starts_with(a) || a.starts_with(b),
            "partials must be cumulative: {a:?} -> {b:?}"
        );
    }
    // Final partial's text matches the RPC response's content.
    let last = partials.last().unwrap();
    assert_eq!(last["is_final"], true);
    let final_text = last["text"].as_str().unwrap();
    assert_eq!(
        final_text,
        final_result["message"]["content"].as_str().unwrap()
    );

    registry.shutdown_all().await;
}
