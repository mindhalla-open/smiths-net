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
}
