//! Parse and validate `plugin.toml`.
//!
//! The schema is closed (`deny_unknown_fields`): every key below is
//! read by the loader, and a typo fails the load with a targeted
//! error instead of being silently ignored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Lifecycle hooks the engine dispatches to plugins that declare them
/// in `hooks = [...]`. Each maps to a plugin method of the same name:
///
/// - `on_dialog_created` — params `{call_id, remote_rtp}` after a 2xx
///   to `INVITE` establishes a dialog.
/// - `on_dialog_terminated` — params `{call_id}` after `BYE`.
///
/// `describe_capabilities` is the mandatory load-time handshake every
/// plugin implements, not a hook, so it is not listed here.
pub const SUPPORTED_HOOKS: &[&str] = &["on_dialog_created", "on_dialog_terminated"];

/// Highest allowed `priority`. Lower numbers run first.
pub const MAX_PRIORITY: u16 = 100;

/// `priority` used when the manifest omits it.
pub const DEFAULT_PRIORITY: u16 = 50;

/// Parsed contents of a `plugin.toml`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Unique plugin name (used in logs, MCP tool output, config).
    pub name: String,
    /// Plugin version (semver-ish, free-form string).
    #[serde(default)]
    pub version: String,
    /// Which tier of the plugin ABI this plugin belongs to.
    #[serde(rename = "type")]
    pub plugin_type: PluginType,
    /// Path to the executable (sidecars), `.wasm` module (WASM), or
    /// script source (script plugins), relative to the plugin's
    /// directory.
    pub entry: PathBuf,
    /// Capabilities this plugin claims to provide. Must match what
    /// its `describe_capabilities` handshake actually returns.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Host-surface capabilities the plugin requires at runtime.
    /// Checked by the WASM host on every gated import: `"state"`
    /// (persistent KV), `"events"` (`publish_event`), `"timers"`
    /// (`timer_set`), `"send_rtp"`, `"send_sip"` (`originate` /
    /// `hangup`). Default: empty — plugins that only need
    /// `smiths::log` can omit the field.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// ABI revision the plugin was built against. Plugins written for
    /// a future major must be refused.
    #[serde(default = "default_abi")]
    pub abi: String,
    /// Free-form human-readable description shown in `list_ai_providers`.
    #[serde(default)]
    pub description: String,
    /// Lifecycle hooks the engine should dispatch to this plugin.
    /// Every entry must be one of [`SUPPORTED_HOOKS`]; the loader
    /// registers each on the registry's hook dispatcher under this
    /// plugin's `priority`.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Ordering among plugins registered for the same hook:
    /// `0..=100`, lower runs first. Default [`DEFAULT_PRIORITY`].
    #[serde(default = "default_priority")]
    pub priority: u16,
    /// Which DSL engine a `type = "script"` plugin targets. Ignored
    /// for every other plugin type. Default is `"rhai"` so a
    /// minimal manifest works out of the box.
    #[serde(default)]
    pub script_engine: ScriptEngine,
    /// Optional per-invocation op-count cap for script plugins.
    /// `0` means "use the engine default" (1M). Ignored for every
    /// other plugin type.
    #[serde(default)]
    pub script_max_operations: u64,
    /// Optional per-invocation wall-clock cap in ms for script
    /// plugins. `0` means "use the engine default" (500 ms).
    #[serde(default)]
    pub script_wall_clock_ms: u64,
}

/// Which DSL engine a `type = "script"` manifest selects.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ScriptEngine {
    /// Rhai 1.x — the default.
    #[default]
    Rhai,
}

fn default_abi() -> String {
    "1.0".to_string()
}

fn default_priority() -> u16 {
    DEFAULT_PRIORITY
}

/// Which tier of the plugin ABI a manifest targets.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Subprocess + JSON-RPC over stdio.
    Sidecar,
    /// WASM guest run by the in-process `wasmtime` host.
    Wasm,
    /// Embedded DSL script (Rhai) run in-process.
    Script,
}

impl Manifest {
    /// Load a manifest from `dir/plugin.toml`.
    pub fn from_dir(dir: &Path) -> Result<Self, Error> {
        let path = dir.join("plugin.toml");
        let text = std::fs::read_to_string(&path).map_err(|e| Error::Manifest {
            path: path.display().to_string(),
            reason: format!("read: {e}"),
        })?;
        let m: Self = toml::from_str(&text).map_err(|e| Error::Manifest {
            path: path.display().to_string(),
            reason: format!("parse: {e}"),
        })?;
        m.validate().map_err(|reason| Error::Manifest {
            path: path.display().to_string(),
            reason,
        })?;
        Ok(m)
    }

    /// Schema checks beyond what serde enforces. Returns a
    /// human-readable reason on failure.
    fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("`name` is empty".into());
        }
        if !self.abi.starts_with("1.") {
            return Err(format!(
                "unsupported abi `{}` (engine supports 1.x)",
                self.abi
            ));
        }
        if self.priority > MAX_PRIORITY {
            return Err(format!(
                "`priority` {} is out of range (0..={MAX_PRIORITY}; lower runs first)",
                self.priority
            ));
        }
        for (i, hook) in self.hooks.iter().enumerate() {
            if !SUPPORTED_HOOKS.contains(&hook.as_str()) {
                return Err(format!(
                    "unknown hook `{hook}` in `hooks` (supported: {})",
                    SUPPORTED_HOOKS.join(", ")
                ));
            }
            if self.hooks[..i].contains(hook) {
                return Err(format!("hook `{hook}` is listed twice in `hooks`"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn parse(toml: &str) -> Result<Manifest, Error> {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("plugin.toml"), toml).unwrap();
        Manifest::from_dir(dir.path())
    }

    #[test]
    fn parses_valid_sidecar_manifest() {
        let m = parse(
            r#"
name     = "ai-tts-mock"
version  = "0.1.0"
type     = "sidecar"
entry    = "./main.py"
provides = ["ai.tts"]
abi      = "1.0"
description = "A mock TTS"
"#,
        )
        .unwrap();
        assert_eq!(m.name, "ai-tts-mock");
        assert_eq!(m.plugin_type, PluginType::Sidecar);
        assert_eq!(m.provides, vec!["ai.tts".to_string()]);
        assert!(m.hooks.is_empty());
        assert_eq!(m.priority, DEFAULT_PRIORITY);
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(matches!(
            parse(
                r#"
name  = "x"
type  = "sidecar"
entry = "./x"
bogus = 42
"#
            ),
            Err(Error::Manifest { .. })
        ));
    }

    #[test]
    fn rejects_future_abi() {
        let err = parse(
            r#"
name  = "x"
type  = "sidecar"
entry = "./x"
abi   = "2.0"
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("abi"), "{msg}");
    }

    #[test]
    fn hooks_and_priority_parse() {
        let m = parse(
            r#"
name     = "brain"
type     = "wasm"
entry    = "./brain.wasm"
hooks    = ["on_dialog_created", "on_dialog_terminated"]
priority = 10
"#,
        )
        .unwrap();
        assert_eq!(m.hooks, vec!["on_dialog_created", "on_dialog_terminated"]);
        assert_eq!(m.priority, 10);
    }

    #[test]
    fn unknown_hook_is_rejected_with_the_supported_list() {
        let err = parse(
            r#"
name  = "x"
type  = "sidecar"
entry = "./x"
hooks = ["on_rtp_frame"]
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown hook `on_rtp_frame`"), "{msg}");
        assert!(msg.contains("on_dialog_created"), "{msg}");
        assert!(msg.contains("on_dialog_terminated"), "{msg}");
    }

    #[test]
    fn duplicate_hook_is_rejected() {
        let err = parse(
            r#"
name  = "x"
type  = "sidecar"
entry = "./x"
hooks = ["on_dialog_created", "on_dialog_created"]
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }

    #[test]
    fn priority_out_of_range_is_rejected() {
        let err = parse(
            r#"
name     = "x"
type     = "sidecar"
entry    = "./x"
priority = 101
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("priority"), "{err}");
    }
}
