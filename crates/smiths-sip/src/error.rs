//! Errors raised by the SIP subsystem.

use thiserror::Error;

/// SIP subsystem errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// I/O failure on a SIP transport.
    #[error("transport I/O: {0}")]
    Io(#[from] std::io::Error),

    /// Failure to parse an incoming SIP message.
    #[error("parse: {0}")]
    Parse(String),

    /// Configuration rejected at startup.
    #[error("config: {0}")]
    Config(String),

    /// Catch-all for subsystem-specific failures the caller wants
    /// to flatten into the SIP error type (today: snapshot I/O —
    /// slice 6.1).
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Convenience constructor for [`Self::Other`]. Used by the
    /// snapshot module to turn its own error into a SIP error.
    #[must_use]
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
