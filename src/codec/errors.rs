use std::fmt;

/// Errors from the null-byte JSON codec layer.
#[derive(Debug)]
pub enum CodecError {
    /// Incomplete message (no `\0` delimiter found yet).
    Incomplete,
    /// The bytes between delimiters are not valid UTF-8.
    InvalidUtf8(std::string::FromUtf8Error),
    /// The UTF-8 string is not valid JSON.
    InvalidJson(serde_json::Error),
    /// I/O error during read or write.
    Io(std::io::Error),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => write!(f, "incomplete message"),
            Self::InvalidUtf8(e) => write!(f, "invalid UTF-8: {e}"),
            Self::InvalidJson(e) => write!(f, "invalid JSON: {e}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUtf8(e) => Some(e),
            Self::InvalidJson(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Incomplete => None,
        }
    }
}

impl From<std::io::Error> for CodecError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for CodecError {
    fn from(e: serde_json::Error) -> Self {
        Self::InvalidJson(e)
    }
}

impl From<std::string::FromUtf8Error> for CodecError {
    fn from(e: std::string::FromUtf8Error) -> Self {
        Self::InvalidUtf8(e)
    }
}
