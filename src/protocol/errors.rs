use crate::protocol::types::ErrorData;
use crate::transport::errors::TransportError;
use std::fmt;
use std::time::Duration;

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
    /// The request was not answered before its deadline.
    Timeout,
    /// The navigation was diverted into a download flow by the renderer;
    /// no DOM was loaded and the page state is unchanged from pre-navigation.
    ///
    /// Structured details live in [`ProtocolError::download_info`] when this
    /// kind is set.
    NavigationBecameDownload,
}

/// Structured details for a [`ProtocolErrorKind::NavigationBecameDownload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadInfo {
    /// The URL the renderer fetched (from the `Browser.downloadCreated` event).
    pub url: String,
    /// The frame that was navigating, when known.
    pub frame_id: Option<String>,
    /// The UUID assigned to the download by the browser.
    pub download_id: Option<String>,
}

/// A protocol-level error.
#[derive(Debug)]
pub struct ProtocolError {
    pub kind: ProtocolErrorKind,
    pub method: Option<String>,
    pub message: String,
    pub data: Option<String>,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
    /// Populated only when [`kind`](Self::kind) is
    /// [`ProtocolErrorKind::NavigationBecameDownload`].
    ///
    /// Boxed so the rare-but-large download metadata doesn't inflate
    /// `ProtocolError` for the common error paths (which would trigger
    /// clippy's `result_large_err`).
    pub download_info: Option<Box<DownloadInfo>>,
}

impl ProtocolError {
    pub fn response(method: impl Into<String>, error: ErrorData) -> Self {
        Self {
            kind: ProtocolErrorKind::Response,
            method: Some(method.into()),
            message: error.message,
            data: error.data,
            source: None,
            download_info: None,
        }
    }

    pub fn closed(method: Option<String>) -> Self {
        Self {
            kind: ProtocolErrorKind::Closed,
            method,
            message: "Session closed".into(),
            data: None,
            source: None,
            download_info: None,
        }
    }

    pub fn crashed(method: Option<String>) -> Self {
        Self {
            kind: ProtocolErrorKind::Crashed,
            method,
            message: "Page crashed".into(),
            data: None,
            source: None,
            download_info: None,
        }
    }

    pub fn transport(err: TransportError) -> Self {
        Self {
            kind: ProtocolErrorKind::Transport,
            method: None,
            message: err.to_string(),
            data: None,
            source: Some(Box::new(err)),
            download_info: None,
        }
    }

    /// Construct a [`ProtocolErrorKind::NavigationBecameDownload`] error.
    ///
    /// Use when a `Browser.downloadCreated` event arrives while a
    /// `Page.navigate` was in flight for the same frame — the browser
    /// diverted the response into a download flow rather than creating
    /// a document, so the navigate caller will never get a response.
    pub fn navigation_became_download(method: Option<String>, info: DownloadInfo) -> Self {
        let message = match (&info.url, &info.frame_id) {
            (url, Some(frame)) => {
                format!("navigation diverted into download flow (frame {frame}, url {url})")
            }
            (url, None) => format!("navigation diverted into download flow (url {url})"),
        };
        Self {
            kind: ProtocolErrorKind::NavigationBecameDownload,
            method,
            message,
            data: None,
            source: None,
            download_info: Some(Box::new(info)),
        }
    }

    /// Build a `Timeout` error for a request that exceeded its deadline.
    ///
    /// The pending slot on the sending side is the caller's responsibility
    /// to free before constructing this — see
    /// [`Session::send_with_timeout`](crate::protocol::client::Session::send_with_timeout).
    pub fn timeout(method: impl Into<String>, deadline: Duration) -> Self {
        let method = method.into();
        Self {
            kind: ProtocolErrorKind::Timeout,
            message: format!("Request '{method}' timed out after {:?}", deadline,),
            method: Some(method),
            data: None,
            source: None,
            download_info: None,
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
