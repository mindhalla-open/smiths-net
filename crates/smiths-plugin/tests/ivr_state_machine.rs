//! Slice 4.2 acceptance — drive the `ivr-kit` Rhai state machine
//! through a synthetic "press 1 / press 2" session and verify the
//! resulting routing decisions. Stands in for the full call-flow
//! E2E (which needs the engine-side media runtime to push DTMF
//! events into the script, a dedicated follow-on); the state
//! machine's contract is fully exercised.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use smiths_core::ai::{AiProvider, AiRegistry as AiRegistryTrait};
use smiths_plugin::{AiRegistry, LoaderOpts, load_plugins};

fn ivr_kit_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("plugins/examples/ivr-kit"))
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

async fn invoke(provider: &Arc<dyn AiProvider>, method: &str, params: Value) -> Value {
    provider
        .invoke(method, params)
        .await
        .unwrap_or_else(|e| panic!("{method} failed: {e}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn press_1_routes_to_sales_press_2_routes_to_support() {
    let registry = AiRegistry::new();
    let tmp = stage_in_temp(&ivr_kit_src());
    let report = load_plugins(tmp.path(), &registry, LoaderOpts::default())
        .await
        .unwrap();
    assert!(
        report.failed.is_empty(),
        "load failures: {:?}",
        report.failed
    );
    assert!(report.loaded.iter().any(|n| n == "ivr-kit"));

    let provider: Arc<dyn AiProvider> =
        AiRegistryTrait::get(&registry, "ivr-kit").expect("provider registered");

    // --- New call: engine calls `greet` first. ---
    let greeting = invoke(&provider, "greet", json!({ "call_id": "voicebot-1" })).await;
    assert_eq!(greeting["action"], "play_prompt");
    assert_eq!(greeting["prompt"], "prompts/welcome.wav");
    assert_eq!(greeting["state"], "main_menu");
    assert_eq!(greeting["done"], false);

    // --- Press 1 → transfer to the sales queue. ---
    let sales = invoke(
        &provider,
        "on_dtmf",
        json!({
            "call_id": "voicebot-1",
            "state":   "main_menu",
            "digit":   "1",
        }),
    )
    .await;
    assert_eq!(sales["action"], "transfer");
    assert_eq!(sales["target"], "sip:queue-sales@pbx.internal");
    assert_eq!(sales["done"], true);

    // --- Press 2 → transfer to the support queue. ---
    let support = invoke(
        &provider,
        "on_dtmf",
        json!({
            "call_id": "voicebot-2",
            "state":   "main_menu",
            "digit":   "2",
        }),
    )
    .await;
    assert_eq!(support["action"], "transfer");
    assert_eq!(support["target"], "sip:queue-support@pbx.internal");
    assert_eq!(support["done"], true);

    // --- Unknown digit replays the greeting, stays in state. ---
    let replay = invoke(
        &provider,
        "on_dtmf",
        json!({
            "call_id": "voicebot-3",
            "state":   "main_menu",
            "digit":   "9",
        }),
    )
    .await;
    assert_eq!(replay["action"], "play_prompt");
    assert_eq!(replay["prompt"], "prompts/try-again.wav");
    assert_eq!(replay["state"], "main_menu");
    assert_eq!(replay["done"], false);

    // --- Press # hangs up. ---
    let hangup = invoke(
        &provider,
        "on_dtmf",
        json!({
            "call_id": "voicebot-4",
            "state":   "main_menu",
            "digit":   "#",
        }),
    )
    .await;
    assert_eq!(hangup["action"], "hangup");
    assert_eq!(hangup["done"], true);

    // --- Submenu: 3 → record_message state, then `#` ends it. ---
    let submenu = invoke(
        &provider,
        "on_dtmf",
        json!({
            "call_id": "voicebot-5",
            "state":   "main_menu",
            "digit":   "3",
        }),
    )
    .await;
    assert_eq!(submenu["action"], "play_prompt");
    assert_eq!(submenu["state"], "record_message");
    let end = invoke(
        &provider,
        "on_dtmf",
        json!({
            "call_id": "voicebot-5",
            "state":   "record_message",
            "digit":   "#",
        }),
    )
    .await;
    assert_eq!(end["action"], "hangup");

    registry.shutdown_all().await;
}
