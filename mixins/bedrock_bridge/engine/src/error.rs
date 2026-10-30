//! The crate's error type.

use std::fmt;

/// Everything that can go wrong below the translator.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// A truncated or malformed packet.
    #[error("decode error: {0}")]
    Decode(String),
    /// A transport-level failure (RakNet session or socket).
    #[error("transport error: {0}")]
    Transport(String),
    /// The sign-in or login handshake failed.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// The server refused us (a disconnect packet, a bad login, a full server).
    #[error("server refused: {0}")]
    Refused(String),
    /// A mapping table has no entry for a Bedrock id.
    #[error("unmapped value: {0}")]
    Unmapped(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn decode(msg: impl fmt::Display) -> Self {
        Error::Decode(msg.to_string())
    }
    pub fn transport(msg: impl fmt::Display) -> Self {
        Error::Transport(msg.to_string())
    }
    pub fn auth(msg: impl fmt::Display) -> Self {
        Error::Auth(msg.to_string())
    }
    pub fn other(msg: impl fmt::Display) -> Self {
        Error::Other(msg.to_string())
    }
}
