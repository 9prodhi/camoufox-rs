use crate::codec::errors::CodecError;
use std::fmt;

/// Errors from the transport layer.
#[derive(Debug)]
pub enum TransportError {
    /// The transport is closed.
    Closed,
    /// Codec-level error (framing or JSON).
    Codec(CodecError),
    /// I/O error.
    Io(std::io::Error),
    /// The child process exited unexpectedly.
    ProcessExited(Option<i32>),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "transport closed"),
            Self::Codec(e) => write!(f, "codec error: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::ProcessExited(code) => {
                write!(f, "process exited")?;
                if let Some(c) = code {
                    write!(f, " with code {c}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<CodecError> for TransportError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
