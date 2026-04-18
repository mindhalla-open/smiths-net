//! SDP parse errors.

use thiserror::Error;

/// Errors raised during SDP parsing.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// A mandatory session-level line (`v=`, `o=`, `s=`) is missing.
    #[error("missing required session line: {0}")]
    MissingSessionLine(&'static str),

    /// A line is syntactically invalid.
    #[error("malformed line {line}: {reason}")]
    Malformed {
        /// 1-based line index.
        line: usize,
        /// Human-readable description of the problem.
        reason: String,
    },

    /// Unsupported feature or version.
    #[error("unsupported: {0}")]
    Unsupported(String),
}
