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
        /// Optional lifecycle event to wait for after the Page.navigate ack.
        /// Supported values: "load", "domcontentloaded".
        /// Absent (null/missing) means return after ack — existing behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_until: Option<String>,
    },

    /// Evaluate JavaScript on a page.
    Evaluate {
        instance_id: String,
        page_id: String,
        expression: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Dispatch a trusted left-click at viewport coordinates (x, y).
    Click {
        instance_id: String,
        page_id: String,
        x: i32,
        y: i32,
    },

    /// Click the element matching a CSS selector (resolve → scroll → click centre).
    ClickSelector {
        instance_id: String,
        page_id: String,
        selector: String,
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
        /// Crop to the element matching this CSS selector.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        /// Explicit clip rectangle `[x, y, width, height]` in CSS pixels.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clip: Option<[f64; 4]>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Shut down the daemon and all instances.
    Shutdown,

    /// Export all cookies for a browser instance (including HttpOnly).
    Cookies { instance_id: String },

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------
    /// Extract the page's visible text, optionally scoped to a selector.
    Text {
        instance_id: String,
        page_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Extract page HTML (`outerHTML` of a selector, or the whole document).
    Html {
        instance_id: String,
        page_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Collect every `<a href>` on the page.
    Links {
        instance_id: String,
        page_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Extract structured metadata (Open Graph / JSON-LD / meta tags).
    Data {
        instance_id: String,
        page_id: String,
        #[serde(default)]
        og: bool,
        #[serde(default)]
        jsonld: bool,
        #[serde(default)]
        meta: bool,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    // -----------------------------------------------------------------------
    // Navigation / waiting
    // -----------------------------------------------------------------------
    /// Read the page's current URL and title.
    Url {
        instance_id: String,
        page_id: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Go back one session-history entry.
    Back {
        instance_id: String,
        page_id: String,
    },

    /// Go forward one session-history entry.
    Forward {
        instance_id: String,
        page_id: String,
    },

    /// Reload the page.
    Reload {
        instance_id: String,
        page_id: String,
    },

    /// Poll until an element matching a CSS selector exists.
    Wait {
        instance_id: String,
        page_id: String,
        selector: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    // -----------------------------------------------------------------------
    // Cookies / headers
    // -----------------------------------------------------------------------
    /// Set a single cookie on the instance's browser context.
    SetCookie {
        instance_id: String,
        page_id: String,
        name: String,
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default)]
        secure: bool,
        #[serde(default)]
        http_only: bool,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Add an extra HTTP request header to a page.
    SetHeader {
        instance_id: String,
        page_id: String,
        name: String,
        value: String,
    },

    // -----------------------------------------------------------------------
    // Interaction
    // -----------------------------------------------------------------------
    /// Focus, clear and type into the element matching a selector.
    Fill {
        instance_id: String,
        page_id: String,
        selector: String,
        value: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Insert text into the currently focused element.
    Type {
        instance_id: String,
        page_id: String,
        text: String,
    },

    /// Press a named key.
    Press {
        instance_id: String,
        page_id: String,
        key: String,
    },

    /// Move the mouse over the element matching a selector.
    Hover {
        instance_id: String,
        page_id: String,
        selector: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Choose an option in a `<select>`.
    Select {
        instance_id: String,
        page_id: String,
        selector: String,
        value: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Scroll an element into view, or scroll to the page bottom.
    Scroll {
        instance_id: String,
        page_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    // -----------------------------------------------------------------------
    // Tabs
    // -----------------------------------------------------------------------
    /// List an instance's open pages with URL + title.
    Tabs {
        instance_id: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Close a page.
    CloseTab {
        instance_id: String,
        page_id: String,
    },
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

    /// The reading commands round-trip through the IPC envelope, including the
    /// optional `selector` (absent when `None`, so old daemons ignore it).
    #[test]
    fn text_request_serde_round_trip() {
        let req = DaemonRequest::Text {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            selector: Some("h1".into()),
            timeout_secs: 30,
        };
        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(serialized.contains("\"method\":\"Text\""));
        match serde_json::from_str::<DaemonRequest>(&serialized).expect("deserialize") {
            DaemonRequest::Text {
                page_id, selector, ..
            } => {
                assert_eq!(page_id, "p1");
                assert_eq!(selector.as_deref(), Some("h1"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    /// A `None` selector is omitted from the wire payload rather than sent as
    /// `null`, matching the `wait_until` convention already in use.
    #[test]
    fn absent_selector_is_omitted_not_null() {
        let req = DaemonRequest::Text {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            selector: None,
            timeout_secs: 30,
        };
        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(
            !serialized.contains("selector"),
            "absent selector omitted: {serialized}"
        );
    }

    /// The selector-click variant is distinct from the coordinate `Click`, so
    /// the legacy coordinate form cannot be mistaken for it on the wire.
    #[test]
    fn click_and_click_selector_are_distinct_variants() {
        let coord = serde_json::to_string(&DaemonRequest::Click {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            x: 10,
            y: 20,
        })
        .expect("serialize");
        let sel = serde_json::to_string(&DaemonRequest::ClickSelector {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            selector: "#go".into(),
            timeout_secs: 30,
        })
        .expect("serialize");

        assert!(coord.contains("\"method\":\"Click\""));
        assert!(sel.contains("\"method\":\"ClickSelector\""));

        match serde_json::from_str::<DaemonRequest>(&coord).expect("deserialize") {
            DaemonRequest::Click { x, y, .. } => assert_eq!((x, y), (10, 20)),
            other => panic!("expected Click, got {other:?}"),
        }
    }

    /// The screenshot request carries the new crop options and still
    /// deserialises when they are absent (backwards compatible).
    #[test]
    fn screenshot_request_crop_options_round_trip() {
        let req = DaemonRequest::Screenshot {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            format: Some("png".into()),
            quality: None,
            path: None,
            selector: None,
            clip: Some([1.0, 2.0, 300.0, 400.0]),
            timeout_secs: 30,
        };
        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(!serialized.contains("selector"), "None selector omitted");
        match serde_json::from_str::<DaemonRequest>(&serialized).expect("deserialize") {
            DaemonRequest::Screenshot { clip, selector, .. } => {
                assert_eq!(clip, Some([1.0, 2.0, 300.0, 400.0]));
                assert!(selector.is_none());
            }
            other => panic!("expected Screenshot, got {other:?}"),
        }

        // A payload predating the crop options still parses.
        let legacy = r#"{"method":"Screenshot","params":{"instance_id":"1","page_id":"p1"}}"#;
        match serde_json::from_str::<DaemonRequest>(legacy).expect("deserialize legacy") {
            DaemonRequest::Screenshot {
                clip,
                selector,
                timeout_secs,
                ..
            } => {
                assert!(clip.is_none() && selector.is_none());
                assert_eq!(timeout_secs, 30, "default timeout applied");
            }
            other => panic!("expected Screenshot, got {other:?}"),
        }
    }

    /// `SetCookie` carries every optional attribute without losing flags.
    #[test]
    fn set_cookie_request_round_trip() {
        let req = DaemonRequest::SetCookie {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            name: "session".into(),
            value: "abc".into(),
            url: None,
            domain: Some("example.com".into()),
            path: Some("/".into()),
            secure: true,
            http_only: true,
            timeout_secs: 30,
        };
        let serialized = serde_json::to_string(&req).expect("serialize");
        match serde_json::from_str::<DaemonRequest>(&serialized).expect("deserialize") {
            DaemonRequest::SetCookie {
                name,
                domain,
                secure,
                http_only,
                ..
            } => {
                assert_eq!(name, "session");
                assert_eq!(domain.as_deref(), Some("example.com"));
                assert!(secure && http_only);
            }
            other => panic!("expected SetCookie, got {other:?}"),
        }
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
