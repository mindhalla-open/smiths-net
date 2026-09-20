//! Discover plugins under a directory, spawn them, run the
//! `describe_capabilities` handshake, populate the [`AiRegistry`].
//!
//! Loader is **fail-partial**: a single broken plugin doesn't stop the
//! rest. Failures are collected into [`LoadReport`] so the CLI can log
//! a clean summary and operators know exactly which plugin misbehaved.
//!
//! ## Directory layout
//!
//! A plugin is a directory holding `plugin.toml`. Directories without
//! a manifest are containers (`plugins/examples`,
//! `plugins/cookbook/wasm/rust`,...) and are descended into, up to
//! [`MAX_SCAN_DEPTH`] levels below the root; hidden directories are
//! skipped. Once a manifest is found the loader does not look inside
//! that plugin's directory for further plugins.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use smiths_core::Metrics;
use smiths_core::ai::parse_descriptors;
use smiths_core::{Event, EventBus, PluginEvent};
use smiths_wasm::WasmEngine;
use tracing::{debug, info, instrument, warn};

use crate::error::Error;
use crate::manifest::{Manifest, PluginType};
use crate::registry::{AiRegistry, PluginEntry};
use crate::script_provider::ScriptProvider;
use crate::wasm_provider::WasmProvider;

/// How many directory levels below the root a plugin may sit. Four
/// covers `plugins/cookbook/<tier>/<language>/<plugin>`.
pub const MAX_SCAN_DEPTH: usize = 4;

/// Summary of one plugin-load pass.
#[derive(Debug, Default)]
pub struct LoadReport {
    /// Successfully loaded and registered plugins.
    pub loaded: Vec<String>,
    /// (plugin dir display, error) for each plugin that failed.
    pub failed: Vec<(String, String)>,
}

/// Dependencies and resource knobs threaded through the loader. Every
/// field has a default so tests and small deployments can pass
/// `LoaderOpts::default`; the CLI builds a fully-populated one at
/// boot. The registry keeps a copy per sidecar plugin so a reload
/// respawns with the same options.
#[derive(Clone, Debug)]
pub struct LoaderOpts {
    /// When set, each loaded sidecar gets a background bridge task
    /// that republishes its JSON-RPC notifications onto the engine's
    /// `EventBus` as [`Event::Plugin`] events.
    pub bus: Option<EventBus>,
    /// Required to load `type = "wasm"` manifests. Without it, WASM
    /// plugins fail-partial with a descriptive error while other
    /// plugins still load. The loader applies the `wasm_*` knobs
    /// below to its clone of the engine.
    pub wasm_engine: Option<WasmEngine>,
    /// Optional engine-wide metrics handle. When set, every loaded
    /// plugin records `plugin_invocations` + `plugin_invoke_duration`
    /// + (sidecars) `sidecar_restarts` through this registry.
    pub metrics: Option<Arc<Metrics>>,
    /// Per-sidecar resource sandbox. See the
    /// [`smiths_core::SandboxConfig`] doc for field semantics.
    /// Default is permissive.
    pub sandbox: smiths_core::SandboxConfig,
    /// Restart policy for sidecar children. Default: 5 retries,
    /// 250 ms initial backoff doubling to 30 s, budget reset after
    /// 10 s of uptime.
    pub sidecar_restart: smiths_sidecar::RestartPolicy,
    /// Timeout for each sidecar RPC (`describe_capabilities` at load
    /// and every `invoke`). Default 30 s.
    pub sidecar_rpc_timeout: Duration,
    /// Largest stdout frame accepted from a sidecar before the child
    /// is restarted. Default 16 MiB.
    pub sidecar_max_frame_bytes: usize,
    /// Linear-memory cap for each WASM guest instance. Default 64 MiB.
    pub wasm_memory_limit_bytes: usize,
    /// Wall-clock budget for one WASM guest call (`describe` at load
    /// and every `invoke`). Default 5 s.
    pub wasm_invoke_timeout: Duration,
    /// Byte budget for each WASM plugin's persistent KV state.
    /// Default 1 MiB.
    pub wasm_state_budget_bytes: usize,
}

impl Default for LoaderOpts {
    fn default() -> Self {
        Self {
            bus: None,
            wasm_engine: None,
            metrics: None,
            sandbox: smiths_core::SandboxConfig::default(),
            sidecar_restart: smiths_sidecar::RestartPolicy::default(),
            sidecar_rpc_timeout: smiths_sidecar::DEFAULT_RPC_TIMEOUT,
            sidecar_max_frame_bytes: smiths_sidecar::DEFAULT_MAX_FRAME_BYTES,
            wasm_memory_limit_bytes: smiths_wasm::DEFAULT_MEMORY_LIMIT_BYTES,
            wasm_invoke_timeout: smiths_wasm::DEFAULT_INVOKE_TIMEOUT,
            wasm_state_budget_bytes: smiths_wasm::DEFAULT_STATE_BUDGET_BYTES,
        }
    }
}

/// Scan `root` for plugin directories (see the module docs for the
/// layout rules), load each, and insert successful ones into
/// `registry`.
///
/// If `root` doesn't exist, returns an empty report (operators turn
/// plugins on by creating the directory — no error).
#[instrument(skip(registry, opts), fields(root = %root.display()))]
pub async fn load_plugins(
    root: &Path,
    registry: &AiRegistry,
    opts: LoaderOpts,
) -> Result<LoadReport, Error> {
    let mut report = LoadReport::default();
    if !root.exists() {
        info!(dir = %root.display(), "plugins dir missing; skipping load");
        return Ok(report);
    }

    // Depth-first over container directories; plugin directories are
    // leaves. `depth` is how many levels below `root` a directory's
    // *children* sit.
    let mut pending: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 1)];
    while let Some((dir, depth)) = pending.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut containers = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if !meta.is_dir() {
                continue;
            }
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                debug!(dir = %path.display(), "skipping hidden directory");
                continue;
            }
            if path.join("plugin.toml").is_file() {
                match load_one(&path, registry, opts.clone()).await {
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
            } else if depth < MAX_SCAN_DEPTH {
                debug!(dir = %path.display(), "no plugin.toml; scanning subdirectories");
                containers.push((path, depth + 1));
            } else {
                debug!(
                    dir = %path.display(),
                    max_depth = MAX_SCAN_DEPTH,
                    "no plugin.toml at maximum scan depth; skipping"
                );
            }
        }
        // Deterministic order regardless of filesystem enumeration.
        containers.sort();
        pending.extend(containers.into_iter().rev());
    }

    Ok(report)
}

#[instrument(skip(registry, opts), fields(dir = %dir.display()))]
pub(crate) async fn load_one(
    dir: &Path,
    registry: &AiRegistry,
    opts: LoaderOpts,
) -> Result<String, Error> {
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
        PluginType::Wasm => {
            return load_wasm(dir, manifest, &opts, registry).await;
        }
        PluginType::Script => {
            return load_script(dir, manifest, opts.metrics.clone(), registry).await;
        }
    }

    // Spawn the subprocess under the configured sandbox, timeouts,
    // and the registry's current env overrides.
    let sidecar = smiths_sidecar::Sidecar::spawn_with_options(
        &name,
        dir,
        &manifest.entry,
        smiths_sidecar::SpawnOptions {
            policy: opts.sidecar_restart,
            sandbox: opts.sandbox.clone(),
            rpc_timeout: opts.sidecar_rpc_timeout,
            max_frame_bytes: opts.sidecar_max_frame_bytes,
            // Per-plugin, from its manifest: the default stays JSON
            // so an existing plugin needs no key at all.
            wire_format: manifest.wire_format,
            env: registry.env_snapshot(),
        },
    )
    .await?;
    if let Some(m) = &opts.metrics {
        sidecar.set_metrics(Arc::clone(m));
    }

    // Handshake: ask the plugin what it provides.
    let raw = sidecar
        .call("describe_capabilities", Value::Null)
        .await
        .map_err(|e| Error::Load {
            plugin: name.clone(),
            reason: format!("describe_capabilities failed: {e}"),
        })?;

    let capabilities = parse_descriptors(raw)
        .map_err(|e| Error::Load {
            plugin: name.clone(),
            reason: e.to_string(),
        })
        .and_then(|descs| bind_and_check(descs, &name, &manifest.provides))?;

    // Start the notification bridge before we register — the bus
    // subscriber slot needs to be armed by the time a plugin starts
    // emitting partials.
    if let Some(bus) = opts.bus.clone() {
        spawn_notification_bridge(&sidecar, name.clone(), bus);
    }

    registry.insert(PluginEntry {
        manifest: manifest.clone(),
        dir: dir.to_path_buf(),
        sidecar,
        capabilities,
        metrics: opts.metrics.clone(),
        loader_opts: opts,
    });
    registry.register_manifest_hooks(&manifest);
    Ok(name)
}

/// Load a `type = "script"` manifest: compile the source via
/// `smiths_script`, run `describe_capabilities`, and register a
/// `ScriptProvider`. Rhai is the only DSL today — other engines slot
/// in here behind a match on `manifest.script_engine`.
async fn load_script(
    dir: &Path,
    manifest: Manifest,
    metrics: Option<Arc<Metrics>>,
    registry: &AiRegistry,
) -> Result<String, Error> {
    let name = manifest.name.clone();
    let entry_path: PathBuf = if manifest.entry.is_absolute() {
        manifest.entry.clone()
    } else {
        dir.join(&manifest.entry)
    };

    let mut limits = smiths_script::ScriptLimits::default();
    if manifest.script_max_operations > 0 {
        limits.max_operations = manifest.script_max_operations;
    }
    if manifest.script_wall_clock_ms > 0 {
        limits.wall_clock = Duration::from_millis(manifest.script_wall_clock_ms);
    }

    let runtime = match manifest.script_engine {
        crate::manifest::ScriptEngine::Rhai => {
            smiths_script::ScriptRuntime::load_rhai(&name, &entry_path, limits).map_err(|e| {
                Error::Load {
                    plugin: name.clone(),
                    reason: format!("script compile: {e}"),
                }
            })?
        }
    };

    let raw = runtime.describe().await.map_err(|e| Error::Load {
        plugin: name.clone(),
        reason: format!("describe_capabilities: {e}"),
    })?;
    let capabilities = parse_descriptors(raw)
        .map_err(|e| Error::Load {
            plugin: name.clone(),
            reason: e.to_string(),
        })
        .and_then(|descs| bind_and_check(descs, &name, &manifest.provides))?;

    let provider = ScriptProvider::new(
        name.clone(),
        manifest.version.clone(),
        manifest.description.clone(),
        manifest.abi.clone(),
        capabilities,
        runtime,
        dir.to_path_buf(),
        limits,
        metrics,
    );
    registry.insert_script(Arc::new(provider));
    registry.register_manifest_hooks(&manifest);
    Ok(name)
}

/// Load a `type = "wasm"` manifest: compile the module and run the
/// guest's `describe` export on the blocking pool (compilation is
/// CPU-bound and `describe` is bounded by the invoke timeout), then
/// register the resulting provider.
async fn load_wasm(
    dir: &Path,
    manifest: Manifest,
    opts: &LoaderOpts,
    registry: &AiRegistry,
) -> Result<String, Error> {
    let name = manifest.name.clone();
    let Some(engine) = opts.wasm_engine.clone() else {
        return Err(Error::Load {
            plugin: name,
            reason: "WASM plugin encountered but loader was not given a WasmEngine".into(),
        });
    };
    let engine = engine
        .with_memory_limit(opts.wasm_memory_limit_bytes)
        .with_invoke_timeout(opts.wasm_invoke_timeout)
        .with_state_budget(opts.wasm_state_budget_bytes);
    let load_dir = dir.to_path_buf();
    let load_manifest = manifest.clone();
    let provider =
        tokio::task::spawn_blocking(move || WasmProvider::load(engine, load_manifest, load_dir))
            .await
            .map_err(|e| Error::Load {
                plugin: name.clone(),
                reason: format!("wasm load worker panicked: {e}"),
            })?
            .map_err(|reason| Error::Load {
                plugin: name.clone(),
                reason,
            })?;
    let provider = match &opts.metrics {
        Some(m) => provider.with_metrics(Arc::clone(m)),
        None => provider,
    };
    registry.insert_wasm(Arc::new(provider));
    registry.register_manifest_hooks(&manifest);
    Ok(name)
}

/// Ensure the manifest's declared `provides` list is fully covered by
/// the descriptor set and clamp each descriptor's `plugin` field to the
/// manifest name so downstream code can trust the binding.
fn bind_and_check(
    descriptors: Vec<smiths_core::ai::CapabilityDescriptor>,
    plugin_name: &str,
    provides: &[String],
) -> Result<Vec<smiths_core::ai::CapabilityDescriptor>, Error> {
    for declared in provides {
        if !descriptors.iter().any(|d| &d.capability == declared) {
            return Err(Error::Load {
                plugin: plugin_name.to_owned(),
                reason: format!("manifest claims `{declared}` but plugin didn't describe it"),
            });
        }
    }
    Ok(descriptors
        .into_iter()
        .map(|mut d| {
            plugin_name.clone_into(&mut d.plugin);
            d
        })
        .collect())
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
        let report = load_plugins(root.path(), &reg, LoaderOpts::default())
            .await
            .unwrap();
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
        let report = load_plugins(
            &root.path().join("does-not-exist"),
            &reg,
            LoaderOpts::default(),
        )
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
        let report = load_plugins(root.path(), &reg, LoaderOpts::default())
            .await
            .unwrap();
        assert!(report.loaded.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(reg.len(), 0);
    }

    /// Plugins nested under container directories (the shipped
    /// `plugins/examples` and `plugins/cookbook/...` layout) load;
    /// containers, hidden directories, plain files, and plugins
    /// beyond `MAX_SCAN_DEPTH` are skipped without a failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn nested_plugin_trees_load_and_containers_are_skipped() {
        let root = tempdir().unwrap();
        make_plugin_dir(&root.path().join("examples"), "stub-a", &["ai.tts"]);
        make_plugin_dir(
            &root.path().join("cookbook/sidecar/bash"),
            "stub-b",
            &["ai.tts"],
        );
        // Hidden container: never scanned.
        make_plugin_dir(&root.path().join(".git"), "stub-hidden", &["ai.tts"]);
        // Five levels down: beyond MAX_SCAN_DEPTH.
        make_plugin_dir(&root.path().join("a/b/c/d"), "stub-deep", &["ai.tts"]);
        fs::create_dir_all(root.path().join("empty-container/nested")).unwrap();
        fs::write(root.path().join("README.md"), "not a plugin").unwrap();

        let reg = AiRegistry::new();
        let report = load_plugins(root.path(), &reg, LoaderOpts::default())
            .await
            .unwrap();
        let mut loaded = report.loaded.clone();
        loaded.sort();
        assert_eq!(loaded, vec!["stub-a", "stub-b"]);
        assert!(
            report.failed.is_empty(),
            "containers must not be reported as failures: {:?}",
            report.failed
        );
        assert_eq!(reg.len(), 2);

        reg.shutdown_all().await;
    }
}
