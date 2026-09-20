//! `AiRegistry::reload` must respawn a sidecar with the options it
//! was originally loaded with: the sandbox, the notification → bus
//! bridge, and the registry's current env overrides.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use smiths_core::{Event, EventBus, PluginEvent, SandboxConfig};
use smiths_plugin::{AiRegistry, AiRegistryTrait, LoaderOpts, load_plugins};
use tempfile::tempdir;

/// Sidecar stub: `check` reports its own `RLIMIT_NOFILE` and an env
/// var; `notify` emits a JSON-RPC notification before replying.
const STUB: &str = r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  case "$method" in
    describe_capabilities)
      printf '{"jsonrpc":"2.0","id":%s,"result":[{"capability":"ai.tts","plugin":"reload-stub","abi":"1.0"}]}\n' "$id"
      ;;
    check)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"nofile":%s,"flag":"%s"}}\n' "$id" "$(ulimit -n)" "$SMITHS_TEST_FLAG"
      ;;
    notify)
      printf '{"jsonrpc":"2.0","method":"emit_partial","params":{"n":1}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":null}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"unknown"}}\n' "$id"
      ;;
  esac
done
"#;

fn stage(root: &Path) {
    let dir = root.join("reload-stub");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        "name     = \"reload-stub\"\n\
         type     = \"sidecar\"\n\
         entry    = \"./main.sh\"\n\
         provides = [\"ai.tts\"]\n",
    )
    .unwrap();
    let script = dir.join("main.sh");
    fs::write(&script, STUB).unwrap();
    let mut p = fs::metadata(&script).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(&script, p).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_keeps_sandbox_bus_bridge_and_env() {
    let root = tempdir().unwrap();
    stage(root.path());

    let bus = EventBus::new(16);
    let registry = AiRegistry::new();
    registry.set_env("SMITHS_TEST_FLAG", "before");
    let opts = LoaderOpts {
        bus: Some(bus.clone()),
        sandbox: SandboxConfig {
            max_fds: Some(64),
            ..SandboxConfig::default()
        },
        ..LoaderOpts::default()
    };
    let report = load_plugins(root.path(), &registry, opts).await.unwrap();
    assert_eq!(report.loaded, vec!["reload-stub"]);

    let before = AiRegistryTrait::get(&registry, "reload-stub").unwrap();
    let out = before.invoke("check", Value::Null).await.unwrap();
    assert_eq!(out["nofile"].as_u64(), Some(64));
    assert_eq!(out["flag"], "before");

    // Rotate the env override, then reload. The respawn must keep the
    // sandbox and bridge from the original LoaderOpts and pick up the
    // current env snapshot.
    registry.set_env("SMITHS_TEST_FLAG", "after");
    let mut rx = bus.subscribe();
    AiRegistryTrait::reload(&registry, "reload-stub")
        .await
        .expect("reload");

    let after = AiRegistryTrait::get(&registry, "reload-stub").unwrap();
    let out = after.invoke("check", Value::Null).await.unwrap();
    assert_eq!(
        out["nofile"].as_u64(),
        Some(64),
        "sandbox must survive reload; got {out}"
    );
    assert_eq!(
        out["flag"], "after",
        "env snapshot must be re-read on respawn"
    );

    after.invoke("notify", Value::Null).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Ok(Event::Plugin(PluginEvent::Notification { plugin, method, .. })) => {
                    return (plugin, method);
                }
                Ok(_) => {}
                Err(e) => panic!("bus closed: {e}"),
            }
        }
    })
    .await
    .expect("notification bridge must survive reload");
    assert_eq!(event, ("reload-stub".to_owned(), "emit_partial".to_owned()));

    registry.shutdown_all().await;
}
