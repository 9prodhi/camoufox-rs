//! IPC types for daemon ↔ client communication.
//!
//! Newline-delimited JSON over Unix socket. One request → one response per connection.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

/// A request from a CLI client to the daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum DaemonRequest {
    /// Ping the daemon. Returns instance count.
    Ping,

    /// Launch a new browser instance.
    Launch {
        #[serde(default)]
        headless: Option<bool>,
        #[serde(default)]
        executable: Option<String>,
    },

    /// List all running instances.
    List,

    /// Stop a browser instance.
    Stop { instance_id: String },

    /// Create a new page in an instance.
    NewPage { instance_id: String },

    /// Navigate a page to a URL.
    Navigate {
        instance_id: String,
        page_id: String,
        url: String,
    },

    /// Evaluate JavaScript on a page.
    Evaluate {
        instance_id: String,
        page_id: String,
        expression: String,
    },

    /// Take a screenshot of a page.
    Screenshot {
        instance_id: String,
        page_id: String,
        #[serde(default)]
        format: Option<String>,
        #[serde(default)]
        quality: Option<u32>,
        #[serde(default)]
        path: Option<String>,
    },

    /// Shut down the daemon and all instances.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

/// A response from the daemon to a CLI client.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl DaemonResponse {
    /// Create a success response with data.
    pub fn ok(data: Value) -> Self {
        DaemonResponse {
            ok: true,
            error: None,
            data: Some(data),
        }
    }

    /// Create a success response with no data.
    pub fn ok_empty() -> Self {
        DaemonResponse {
            ok: true,
            error: None,
            data: None,
        }
    }

    /// Create an error response.
    pub fn err(message: impl Into<String>) -> Self {
        DaemonResponse {
            ok: false,
            error: Some(message.into()),
            data: None,
        }
    }
}
