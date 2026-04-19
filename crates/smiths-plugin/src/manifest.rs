//! Parse and validate `plugin.toml`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

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
    /// Path to the executable (for sidecars) or `.wasm` (for WASM),
    /// relative to the plugin's directory.
    pub entry: PathBuf,
    /// Capabilities this plugin claims to provide. Must match what
    /// its `describe_capabilities` handshake actually returns.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Host-surface capabilities the plugin requires at runtime.
    /// Currently meaningful values: `"state"` (persistent KV
    /// `smiths::state_{get,set}`). Future values will gate
    /// `send_sip`, `send_rtp`, timers, etc. Default: empty — plugins
    /// that only need `smiths::log` can omit the field.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// ABI revision the plugin was built against. Plugins written for
    /// a future major must be refused.
    #[serde(default = "default_abi")]
    pub abi: String,
    /// Free-form human-readable description shown in `list_ai_providers`.
    #[serde(default)]
    pub description: String,
}

fn default_abi() -> String {
    "1.0".to_string()
}

/// Which tier of the plugin ABI a manifest targets.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Subprocess + JSON-RPC over stdio (today's scope).
    Sidecar,
    /// WASM guest (future — currently rejected with a clear error).
    Wasm,
    /// Embedded DSL script (future — rejected).
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
        if m.name.is_empty() {
            return Err(Error::Manifest {
                path: path.display().to_string(),
                reason: "`name` is empty".into(),
            });
        }
        if !m.abi.starts_with("1.") {
            return Err(Error::Manifest {
                path: path.display().to_string(),
                reason: format!("unsupported abi `{}` (engine supports 1.x)", m.abi),
            });
        }
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn parses_valid_sidecar_manifest() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plugin.toml"),
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
        let m = Manifest::from_dir(dir.path()).unwrap();
        assert_eq!(m.name, "ai-tts-mock");
        assert_eq!(m.plugin_type, PluginType::Sidecar);
        assert_eq!(m.provides, vec!["ai.tts".to_string()]);
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plugin.toml"),
            r#"
name  = "x"
type  = "sidecar"
entry = "./x"
bogus = 42
"#,
        )
        .unwrap();
        assert!(matches!(
            Manifest::from_dir(dir.path()),
            Err(Error::Manifest { .. })
        ));
    }

    #[test]
    fn rejects_future_abi() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plugin.toml"),
            r#"
name  = "x"
type  = "sidecar"
entry = "./x"
abi   = "2.0"
"#,
        )
        .unwrap();
        let err = Manifest::from_dir(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("abi"), "{msg}");
    }
}
