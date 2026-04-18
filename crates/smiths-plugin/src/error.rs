//! Plugin-layer errors.

use thiserror::Error;

/// Errors surfaced during plugin load / registry operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Filesystem I/O error reading a manifest or scanning the plugins
    /// directory.
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Manifest file couldn't be parsed as TOML or failed schema checks.
    #[error("manifest error in {path}: {reason}")]
    Manifest {
        /// Path to the offending `plugin.toml`.
        path: String,
        /// Human-readable diagnostic.
        reason: String,
    },

    /// Plugin couldn't be spawned (process failed to start, handshake
    /// errored, or `describe_capabilities` returned a malformed body).
    #[error("plugin `{plugin}` failed to load: {reason}")]
    Load {
        /// Plugin name as declared in the manifest.
        plugin: String,
        /// Human-readable diagnostic.
        reason: String,
    },

    /// Underlying sidecar transport error.
    #[error(transparent)]
    Sidecar(#[from] smiths_sidecar::Error),
}
