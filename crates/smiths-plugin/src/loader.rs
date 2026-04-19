//! Discover plugins in a directory, spawn them, run the
//! `describe_capabilities` handshake, populate the [`AiRegistry`].
//!
//! Loader is **fail-partial**: a single broken plugin doesn't stop the
//! rest. Failures are collected into [`LoadReport`] so the CLI can log
//! a clean summary and operators know exactly which plugin misbehaved.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use smiths_core::ai::CapabilityDescriptor;
use smiths_core::{Event, EventBus, PluginEvent};
use smiths_wasm::WasmEngine;
use tracing::{debug, info, instrument, warn};

use crate::error::Error;
use crate::manifest::{Manifest, PluginType};
use crate::registry::{AiRegistry, PluginEntry};
use crate::wasm_provider::WasmProvider;

/// Summary of one plugin-load pass.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Successfully loaded and registered plugins.
    pub loaded: Vec<String>,
    /// (plugin dir display, error) for each plugin that failed.
    pub failed: Vec<(String, String)>,
}

/// Scan `root` for subdirectories containing `plugin.toml`, spawn each,
/// run the handshake, and insert successful ones into `registry`.
///
/// If `root` doesn't exist, returns an empty report (operators turn
/// plugins on by creating the directory — no error).
///
/// When `bus` is `Some`, each loaded plugin gets a background bridge
/// task that republishes its JSON-RPC notifications onto the bus as
/// [`Event::Plugin`] events. Pass `None` in tests that don't care.
///
/// `wasm_engine` is only consulted for `type = "wasm"` manifests. When
/// `None`, any WASM plugin encountered fails-partial with a descriptive
/// error — sidecar plugins still load. Callers that want WASM support
/// must construct a [`WasmEngine`] and pass it in.
#[instrument(skip(registry, bus, wasm_engine), fields(root = %root.display()))]
pub async fn load_plugins(
    root: &Path,
    registry: &AiRegistry,
    bus: Option<EventBus>,
    wasm_engine: Option<WasmEngine>,
) -> Result<LoadReport, Error> {
    let mut report = LoadReport::default();
    if !root.exists() {
        info!(dir = %root.display(), "plugins dir missing; skipping load");
        return Ok(report);
    }

    let mut entries = tokio::fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.metadata().await?;
        if !meta.is_dir() {
            continue;
        }
        let path = entry.path();
        match load_one(&path, registry, bus.clone(), wasm_engine.clone()).await {
            Ok(name) => {
                info!(plugin = %name, dir = %path.display(), "plugin loaded");
                report.loaded.push(name);
            }
            Err(e) => {
                warn!(dir = %path.display(), ?e, "plugin failed to load");
                report
                    .failed
                    .push((path.display().to_string(), e.to_string()));
            }
        }
    }

    Ok(report)
}

#[instrument(skip(registry, bus, wasm_engine), fields(dir = %dir.display()))]
pub(crate) async fn load_one(
    dir: &Path,
    registry: &AiRegistry,
    bus: Option<EventBus>,
    wasm_engine: Option<WasmEngine>,
) -> Result<String, Error> {
    // `plugin.toml` must exist or the directory isn't a plugin — skip
    // silently by reporting a clean NotFound at the loader boundary.
    let manifest_path = dir.join("plugin.toml");
    if !manifest_path.exists() {
        return Err(Error::Manifest {
            path: manifest_path.display().to_string(),
            reason: "plugin.toml missing".into(),
        });
    }

    let manifest = Manifest::from_dir(dir)?;
    let name = manifest.name.clone();

    match manifest.plugin_type {
        PluginType::Sidecar => { /* fall through to sidecar path below */ }
        PluginType::Wasm => return load_wasm(dir, manifest, wasm_engine, registry),
        PluginType::Script => {
            return Err(Error::Load {
                plugin: name,
                reason: "plugin type `Script` not yet supported".into(),
            });
        }
    }

    // Spawn the subprocess.
    let sidecar = smiths_sidecar::Sidecar::spawn(&name, dir, &manifest.entry).await?;

    // Handshake: ask the plugin what it provides.
    let raw = sidecar
        .call("describe_capabilities", Value::Null)
        .await
        .map_err(|e| Error::Load {
            plugin: name.clone(),
            reason: format!("describe_capabilities failed: {e}"),
        })?;

    let descriptors = parse_descriptors(raw).map_err(|reason| Error::Load {
        plugin: name.clone(),
        reason,
    })?;

    // Sanity-check: the descriptors must match the manifest's declared
    // `provides` list (each provided capability has a descriptor).
    for declared in &manifest.provides {
        if !descriptors.iter().any(|d| &d.capability == declared) {
            return Err(Error::Load {
                plugin: name.clone(),
                reason: format!("manifest claims `{declared}` but plugin didn't describe it"),
            });
        }
    }

    // Each descriptor's `plugin` field should match our manifest name —
    // rewrite it defensively so downstream code can trust the binding.
    let capabilities: Vec<CapabilityDescriptor> = descriptors
        .into_iter()
        .map(|mut d| {
            d.plugin.clone_from(&name);
            d
        })
        .collect();

    // Start the notification bridge before we register — the bus
    // subscriber slot needs to be armed by the time a plugin starts
    // emitting partials.
    if let Some(bus) = bus {
        spawn_notification_bridge(&sidecar, name.clone(), bus);
    }

    registry.insert(PluginEntry {
        manifest,
        dir: dir.to_path_buf(),
        sidecar,
        capabilities,
    });
    Ok(name)
}

/// Load a `type = "wasm"` manifest: compile the module, run the
/// guest's `describe()` export, and register the resulting provider.
/// Synchronous — compilation happens on the caller's thread (wasmtime
/// doesn't have async compile APIs here and the cost is comparable to
/// spawning a subprocess).
fn load_wasm(
    dir: &Path,
    manifest: Manifest,
    wasm_engine: Option<WasmEngine>,
    registry: &AiRegistry,
) -> Result<String, Error> {
    let name = manifest.name.clone();
    let Some(engine) = wasm_engine else {
        return Err(Error::Load {
            plugin: name,
            reason: "WASM plugin encountered but loader was not given a WasmEngine".into(),
        });
    };
    let provider =
        WasmProvider::load(engine, manifest, dir.to_path_buf()).map_err(|reason| Error::Load {
            plugin: name.clone(),
            reason,
        })?;
    registry.insert_wasm(Arc::new(provider));
    Ok(name)
}

/// Subscribe to `sidecar`'s notification broadcast and forward each
/// frame to the engine's bus as [`Event::Plugin`]. Task exits when
/// the sidecar's notification channel closes (plugin shut down).
fn spawn_notification_bridge(sidecar: &smiths_sidecar::Sidecar, plugin: String, bus: EventBus) {
    let mut rx = sidecar.subscribe_notifications();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(notif) => {
                    let _ = bus.publish(Event::Plugin(PluginEvent::Notification {
                        plugin: plugin.clone(),
                        method: notif.method,
                        params: notif.params,
                    }));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(plugin = %plugin, lagged = n, "plugin notification bridge lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    debug!(plugin = %plugin, "plugin notification bridge exiting");
                    return;
                }
            }
        }
    });
}

/// Accept either a single descriptor object or an array of them.
fn parse_descriptors(raw: Value) -> Result<Vec<CapabilityDescriptor>, String> {
    let list: Vec<CapabilityDescriptor> = if raw.is_array() {
        serde_json::from_value(raw).map_err(|e| format!("descriptor array parse: {e}"))?
    } else {
        let single: CapabilityDescriptor =
            serde_json::from_value(raw).map_err(|e| format!("descriptor parse: {e}"))?;
        vec![single]
    };
    if list.is_empty() {
        return Err("plugin returned no capabilities".into());
    }
    for d in &list {
        d.validate()?;
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// A shell-script plugin that satisfies the sidecar handshake
    /// contract for one `ai.tts` capability.
    const TTS_STUB: &str = r#"#!/usr/bin/env bash
# Trivial sidecar: one request-response of describe_capabilities.
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p')
  method=$(printf '%s' "$line" | sed -nE 's/.*"method"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p')
  case "$method" in
    describe_capabilities)
      printf '{"jsonrpc":"2.0","id":%s,"result":[{"capability":"ai.tts","plugin":"ai-tts-stub","model_id":"stub-v1","abi":"1.0","voices":[{"id":"a","lang":"ru","gender":"female"}],"controls":{"rate":{"type":"number","minimum":0.5,"maximum":2.0,"default":1.0}}}]}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"unknown"}}\n' "$id"
      ;;
  esac
done
"#;

    fn make_plugin_dir(root: &Path, name: &str, provides: &[&str]) -> std::path::PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let provides_toml = provides
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            dir.join("plugin.toml"),
            format!(
                "name     = \"{name}\"\n\
                 version  = \"0.1.0\"\n\
                 type     = \"sidecar\"\n\
                 entry    = \"./main.sh\"\n\
                 provides = [{provides_toml}]\n\
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

    #[tokio::test(flavor = "multi_thread")]
    async fn loads_shell_stub_plugin() {
        let root = tempdir().unwrap();
        make_plugin_dir(root.path(), "ai-tts-stub", &["ai.tts"]);

        let reg = AiRegistry::new();
        let report = load_plugins(root.path(), &reg, None, None).await.unwrap();
        assert_eq!(report.loaded, vec!["ai-tts-stub"]);
        assert!(report.failed.is_empty());
        assert_eq!(reg.len(), 1);

        let caps = reg.capabilities();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].capability, "ai.tts");
        assert!(caps[0].extra.contains_key("voices"));

        reg.shutdown_all().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_root_is_not_an_error() {
        let root = tempdir().unwrap();
        let reg = AiRegistry::new();
        let report = load_plugins(&root.path().join("does-not-exist"), &reg, None, None)
            .await
            .unwrap();
        assert!(report.loaded.is_empty());
        assert!(report.failed.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn manifest_mismatch_is_recorded_as_failure() {
        let root = tempdir().unwrap();
        // Manifest claims ai.asr but the stub only describes ai.tts.
        make_plugin_dir(root.path(), "ai-asr-liar", &["ai.asr"]);

        let reg = AiRegistry::new();
        let report = load_plugins(root.path(), &reg, None, None).await.unwrap();
        assert!(report.loaded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(reg.len(), 0);
    }
}
