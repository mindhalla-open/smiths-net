//! Shared error type for `smiths-core`.

use thiserror::Error;

/// Errors raised by the core runtime.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Underlying I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Configuration could not be parsed or validated.
    #[error("config error: {0}")]
    Config(String),

    /// Event bus has no live receivers or was dropped.
    #[error("event bus closed")]
    BusClosed,
}
