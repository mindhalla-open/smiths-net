//! Manifest `hooks` / `priority` and config-listed plugins drive
//! `on_dialog_created` through the registry's dispatcher, in priority
//! order, via the bus consumer.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use smiths_core::{Event, EventBus, SipEvent};
use smiths_plugin::{AiRegistry, LoaderOpts, load_plugins, spawn_call_event_hooks};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

/// Sidecar stub that appends its name to `$HOOK_LOG` whenever the
/// engine invokes `on_dialog_created`.
fn stub(name: &str) -> String {
    format!(
        r#"#!/usr/bin/env bash
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  case "$method" in
    describe_capabilities)
      printf '{{"jsonrpc":"2.0","id":%s,"result":[{{"capability":"ai.tts","plugin":"{name}","abi":"1.0"}}]}}\n' "$id"
      ;;
    on_dialog_created)
      echo "{name}" >> "$HOOK_LOG"
      printf '{{"jsonrpc":"2.0","id":%s,"result":null}}\n' "$id"
      ;;
    *)
      printf '{{"jsonrpc":"2.0","id":%s,"error":{{"code":-32601,"message":"unknown"}}}}\n' "$id"
      ;;
  esac
done
"#
    )
}

fn stage(root: &Path, name: &str, hooks: &[&str], priority: u16) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let hooks_toml = hooks
        .iter()
        .map(|h| format!("\"{h}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        dir.join("plugin.toml"),
        format!(
            "name     = \"{name}\"\n\
             type     = \"sidecar\"\n\
             entry    = \"./main.sh\"\n\
             provides = [\"ai.tts\"]\n\
             hooks    = [{hooks_toml}]\n\
             priority = {priority}\n"
        ),
    )
    .unwrap();
    let script = dir.join("main.sh");
    fs::write(&script, stub(name)).unwrap();
    let mut p = fs::metadata(&script).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(&script, p).unwrap();
}

async fn wait_for_lines(log: &PathBuf, n: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let lines: Vec<String> = fs::read_to_string(log)
            .map(|s| s.lines().map(str::to_owned).collect())
            .unwrap_or_default();
        if lines.len() >= n || tokio::time::Instant::now() > deadline {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dialog_created_fans_out_in_priority_order() {
    let root = tempdir().unwrap();
    // Registration order deliberately differs from priority order.
    stage(root.path(), "late", &["on_dialog_created"], 20);
    stage(root.path(), "early", &["on_dialog_created"], 10);
    // No manifest hooks; only reachable through the config list.
    stage(root.path(), "listed", &[], 50);
    let log = root.path().join("hooks.log");

    let bus = EventBus::new(16);
    let registry = AiRegistry::new();
    registry.set_env("HOOK_LOG", log.display().to_string());
    let report = load_plugins(
        root.path(),
        &registry,
        LoaderOpts {
            bus: Some(bus.clone()),
            ..LoaderOpts::default()
        },
    )
    .await
    .unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        registry.hooks().hooks_for("on_dialog_created"),
        vec!["early.on_dialog_created", "late.on_dialog_created"]
    );

    let cancel = CancellationToken::new();
    let consumer = spawn_call_event_hooks(&bus, &registry, vec!["listed".into()], cancel.clone());
    assert_eq!(
        registry.hooks().hooks_for("on_dialog_created"),
        vec![
            "early.on_dialog_created",
            "late.on_dialog_created",
            "listed.on_dialog_created"
        ]
    );

    bus.publish(Event::Sip(SipEvent::DialogCreated {
        call_id: "call-1".into(),
        media_endpoint: None,
        remote_rtp: None,
    }))
    .unwrap();

    let lines = wait_for_lines(&log, 3).await;
    assert_eq!(lines, vec!["early", "late", "listed"]);

    cancel.cancel();
    let _ = consumer.await;
    registry.shutdown_all().await;
}
