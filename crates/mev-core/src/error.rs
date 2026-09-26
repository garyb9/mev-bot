//! Shared error type for core operations.

use std::fmt;

/// Result alias used across the workspace.
pub type Result<T> = std::result::Result<T, Error>;

/// Core error type. Library crates use typed errors; the binary may wrap them.
#[derive(Debug)]
pub enum Error {
    /// A configuration value was missing or invalid.
    Config(String),
    /// An HTTP/transport error.
    Http(String),
    /// A response could not be decoded.
    Decode(String),
    /// The exchange rejected a signed action (`status: "err"`).
    Exchange(String),
    /// An action was sent but no definitive reply was received (socket loss or
    /// timeout). The order's state is unknown and must be reconciled — never
    /// resent blindly (SPEC-0002 H-1/H-2).
    UnknownOutcome(String),
    /// A feature or code path is not yet implemented.
    Unimplemented(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "configuration error: {msg}"),
            Error::Http(msg) => write!(f, "http error: {msg}"),
            Error::Decode(msg) => write!(f, "decode error: {msg}"),
            Error::Exchange(msg) => write!(f, "exchange error: {msg}"),
            Error::UnknownOutcome(msg) => write!(f, "unknown outcome: {msg}"),
            Error::Unimplemented(what) => write!(f, "not implemented: {what}"),
        }
    }
}

impl std::error::Error for Error {}
