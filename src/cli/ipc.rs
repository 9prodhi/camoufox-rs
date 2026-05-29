//! IPC types for daemon ↔ client communication.
//!
//! Newline-delimited JSON over Unix socket. One request → one response per connection.

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_timeout() -> u64 {
    30
}

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
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Evaluate JavaScript on a page.
    Evaluate {
        instance_id: String,
        page_id: String,
        expression: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
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
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Shut down the daemon and all instances.
    Shutdown,

    /// Export all cookies for a browser instance (including HttpOnly).
    Cookies { instance_id: String },
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `DaemonRequest::Cookies` serialises to the expected JSON shape and
    /// deserialises back to the same variant (round-trip).
    #[test]
    fn cookies_request_serde_round_trip() {
        let req = DaemonRequest::Cookies {
            instance_id: "00000001".into(),
        };
        let serialized = serde_json::to_string(&req).expect("serialize");
        let deserialized: DaemonRequest = serde_json::from_str(&serialized).expect("deserialize");
        match deserialized {
            DaemonRequest::Cookies { instance_id } => {
                assert_eq!(instance_id, "00000001");
            }
            other => panic!("expected Cookies, got {other:?}"),
        }
    }

    /// A cookies response carrying an HttpOnly cookie round-trips through
    /// `DaemonResponse` without losing the `httpOnly` flag.
    #[test]
    fn cookies_response_preserves_http_only_flag() {
        let cookie_json = json!([{
            "name": "PHPSESSID",
            "value": "secret",
            "domain": "example.com",
            "path": "/",
            "expires": -1.0,
            "size": 13,
            "httpOnly": true,
            "secure": true,
            "session": true,
            "sameSite": "Strict"
        }]);
        let resp = DaemonResponse::ok(json!({ "cookies": cookie_json }));
        let serialized = serde_json::to_string(&resp).expect("serialize");
        let back: DaemonResponse = serde_json::from_str(&serialized).expect("deserialize");

        assert!(back.ok);
        let cookies = back
            .data
            .as_ref()
            .and_then(|d| d.get("cookies"))
            .and_then(|v| v.as_array())
            .expect("cookies array present");
        assert_eq!(cookies.len(), 1);
        assert_eq!(
            cookies[0]["httpOnly"], true,
            "httpOnly flag must survive round-trip"
        );
    }

    /// `DaemonResponse::err` round-trips correctly.
    #[test]
    fn error_response_round_trip() {
        let resp = DaemonResponse::err("instance not found");
        let serialized = serde_json::to_string(&resp).expect("serialize");
        let back: DaemonResponse = serde_json::from_str(&serialized).expect("deserialize");
        assert!(!back.ok);
        assert_eq!(back.error.as_deref(), Some("instance not found"));
        assert!(back.data.is_none());
    }
}
