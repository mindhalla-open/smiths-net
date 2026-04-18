//! Discover plugins in a directory, spawn them, run the
//! `describe_capabilities` handshake, populate the [`AiRegistry`].
//!
//! Loader is **fail-partial**: a single broken plugin doesn't stop the
//! rest. Failures are collected into [`LoadReport`] so the CLI can log
//! a clean summary and operators know exactly which plugin misbehaved.

use std::path::Path;

use serde_json::Value;
use tracing::{info, warn};

use crate::descriptor::CapabilityDescriptor;
use crate::error::Error;
use crate::manifest::{Manifest, PluginType};
use crate::registry::{AiRegistry, PluginEntry};

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
pub async fn load_plugins(root: &Path, registry: &AiRegistry) -> Result<LoadReport, Error> {
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
        match load_one(&path, registry).await {
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

async fn load_one(dir: &Path, registry: &AiRegistry) -> Result<String, Error> {
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

    if manifest.plugin_type != PluginType::Sidecar {
        return Err(Error::Load {
            plugin: name,
            reason: format!(
                "plugin type `{:?}` not yet supported (sidecar only)",
                manifest.plugin_type
            ),
        });
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

    registry.insert(PluginEntry {
        manifest,
        sidecar,
        capabilities,
    });
    Ok(name)
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
        let report = load_plugins(root.path(), &reg).await.unwrap();
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
        let report = load_plugins(&root.path().join("does-not-exist"), &reg)
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
        let report = load_plugins(root.path(), &reg).await.unwrap();
        assert!(report.loaded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(reg.len(), 0);
    }
}
