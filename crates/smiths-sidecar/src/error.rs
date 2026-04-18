//! Errors produced by the sidecar host.

use thiserror::Error;

/// Sidecar host errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Child process spawn / I/O failure.
    #[error("sidecar I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Plugin responded with a JSON-RPC error frame.
    #[error("plugin error (code {code}): {message}")]
    Plugin {
        /// JSON-RPC error code.
        code: i64,
        /// Human-readable message.
        message: String,
    },

    /// RPC timed out waiting for a response.
    #[error("rpc timeout after {timeout_ms} ms on method `{method}`")]
    Timeout {
        /// Method name the timeout fired on.
        method: String,
        /// Configured timeout.
        timeout_ms: u64,
    },

    /// Plugin closed stdout / exited before we got the response.
    #[error("plugin closed unexpectedly")]
    Closed,

    /// Wire-level JSON parse failure.
    #[error("malformed JSON from plugin: {0}")]
    MalformedJson(#[from] serde_json::Error),

    /// Caller asked for an invocation after the supervisor shut down.
    #[error("sidecar is shut down")]
    ShutDown,
}
