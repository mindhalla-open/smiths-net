//! The cookbook's binary-framing sample must actually work against
//! the engine's decoder — a sample that only looks right is worse
//! than none.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use smiths_sidecar::framing::WireFormat;
use smiths_sidecar::{RestartPolicy, Sidecar, SpawnOptions};

fn sample_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/cookbook/sidecar/python/binary-frames")
        .canonicalize()
        .expect("the binary-frames sample must exist")
}

#[tokio::test(flavor = "multi_thread")]
async fn cookbook_binary_sample_describes_and_invokes() {
    let dir = sample_dir();
    let sidecar = Sidecar::spawn_with_options(
        "binary-frames-py",
        &dir,
        Path::new("./binary_frames.py"),
        SpawnOptions {
            policy: RestartPolicy::no_restart(),
            wire_format: WireFormat::Binary,
            rpc_timeout: Duration::from_secs(10),
            ..SpawnOptions::default()
        },
    )
    .await
    .expect("sample spawns");

    let caps = sidecar
        .call("describe_capabilities", Value::Null)
        .await
        .expect("describe_capabilities over binary framing");
    assert_eq!(caps[0]["capability"], "ai.echo");
    assert_eq!(caps[0]["plugin"], "binary-frames-py");

    let mut notes = sidecar.subscribe_notifications();
    let out = sidecar
        .call("invoke", json!({"text": "hi"}))
        .await
        .expect("invoke over binary framing");
    let echoed: Value = serde_json::from_str(out["echo"].as_str().expect("echo is a string"))
        .expect("echoed params are json");
    assert_eq!(echoed["text"], "hi");

    let note = tokio::time::timeout(Duration::from_secs(5), notes.recv())
        .await
        .expect("notification arrives")
        .expect("channel open");
    assert_eq!(note.method, "progress");

    let err = sidecar
        .call("nope", Value::Null)
        .await
        .expect_err("an unknown method is an error");
    assert!(err.to_string().contains("nope"), "{err}");
}
