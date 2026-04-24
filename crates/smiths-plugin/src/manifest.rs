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
    /// Wire format the engine uses to deliver streaming-RTP
    /// frames (and future per-packet host-function payloads) to
    /// this plugin (slice 5.2 / P18). Default `proto` keeps
    /// pre-5.2 plugins running unchanged; `flatbuffers` opts in
    /// to the zero-copy fast path that the in-tree bench measures
    /// at ≥2× round-trip throughput on `RtpFrame`-shaped
    /// messages.
    #[serde(default)]
    pub wire_format: WireFormat,
}

/// Wire format the engine uses for plugin payloads on the
/// streaming-RTP path (slice 5.2 / P18). Serialized as
/// `wire_format = "proto" | "flatbuffers"` in `plugin.toml`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WireFormat {
    /// Protobuf (prost). Pre-5.2 default.
    #[default]
    Proto,
    /// Flat zero-copy layout from `smiths-proto::flatbuffers_io`.
    /// Enables a ≥2× encode/decode-throughput win on `RtpFrame`.
    Flatbuffers,
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

    #[test]
    fn wire_format_defaults_to_proto_when_omitted() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plugin.toml"),
            r#"
name     = "x"
type     = "sidecar"
entry    = "./x"
provides = ["ai.tts"]
"#,
        )
        .unwrap();
        let m = Manifest::from_dir(dir.path()).unwrap();
        assert_eq!(m.wire_format, WireFormat::Proto);
    }

    #[test]
    fn wire_format_flatbuffers_parses() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plugin.toml"),
            r#"
name        = "rtp-tap"
type        = "sidecar"
entry       = "./rtp_tap.py"
provides    = ["media.streaming_rtp"]
wire_format = "flatbuffers"
"#,
        )
        .unwrap();
        let m = Manifest::from_dir(dir.path()).unwrap();
        assert_eq!(m.wire_format, WireFormat::Flatbuffers);
    }

    #[test]
    fn wire_format_unknown_token_is_a_manifest_error() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plugin.toml"),
            r#"
name        = "x"
type        = "sidecar"
entry       = "./x"
wire_format = "msgpack"
"#,
        )
        .unwrap();
        assert!(matches!(
            Manifest::from_dir(dir.path()),
            Err(Error::Manifest { .. })
        ));
    }
}
