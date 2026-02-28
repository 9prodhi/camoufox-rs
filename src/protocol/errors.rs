use crate::protocol::types::ErrorData;
use crate::transport::errors::TransportError;
use std::fmt;

/// The type of protocol error, matching Playwright's ProtocolError.type values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    /// Protocol error from the server (response had `error` field).
    Response,
    /// Session or connection was closed.
    Closed,
    /// Page crashed.
    Crashed,
    /// Transport-level failure.
    Transport,
}

/// A protocol-level error.
#[derive(Debug)]
pub struct ProtocolError {
    pub kind: ProtocolErrorKind,
    pub method: Option<String>,
    pub message: String,
    pub data: Option<String>,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl ProtocolError {
    pub fn response(method: impl Into<String>, error: ErrorData) -> Self {
        Self {
            kind: ProtocolErrorKind::Response,
            method: Some(method.into()),
            message: error.message,
            data: error.data,
            source: None,
        }
    }

    pub fn closed(method: Option<String>) -> Self {
        Self {
            kind: ProtocolErrorKind::Closed,
            method,
            message: "Session closed".into(),
            data: None,
            source: None,
        }
    }

    pub fn crashed(method: Option<String>) -> Self {
        Self {
            kind: ProtocolErrorKind::Crashed,
            method,
            message: "Page crashed".into(),
            data: None,
            source: None,
        }
    }

    pub fn transport(err: TransportError) -> Self {
        Self {
            kind: ProtocolErrorKind::Transport,
            method: None,
            message: err.to_string(),
            data: None,
            source: Some(Box::new(err)),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(m) = &self.method {
            write!(f, " ({m})")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl From<TransportError> for ProtocolError {
    fn from(e: TransportError) -> Self {
        Self::transport(e)
    }
}
