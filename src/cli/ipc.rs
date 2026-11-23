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
        #[serde(default)]
        downloads_dir: Option<std::path::PathBuf>,
        #[serde(default)]
        bypass_csp: bool,
        #[serde(default)]
        dialogs: crate::api::DialogPolicy,
    },

    /// Accept or dismiss a page's open JavaScript dialog.
    Dialog {
        instance_id: String,
        page_id: String,
        action: String,
        #[serde(default)]
        text: Option<String>,
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
        #[serde(default)]
        main_world: bool,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
    },

    /// Extract page HTML (`outerHTML` of a selector, or the whole document).
    Html {
        instance_id: String,
        page_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
    },

    /// Collect every `<a href>` on the page.
    Links {
        instance_id: String,
        page_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
    },

    /// Insert text into the currently focused element.
    Type {
        instance_id: String,
        page_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Press a named key.
    Press {
        instance_id: String,
        page_id: String,
        key: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },

    /// Move the mouse over the element matching a selector.
    Hover {
        instance_id: String,
        page_id: String,
        selector: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
    },

    /// Choose an option in a `<select>`.
    Select {
        instance_id: String,
        page_id: String,
        selector: String,
        value: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        frames: Vec<String>,
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

    /// View or wait for downloads on an instance.
    Downloads {
        instance_id: String,
        #[serde(default)]
        wait_secs: Option<u64>,
    },
}

impl DaemonRequest {
    /// Operation budget, before the client's transport margin.
    pub fn timeout_secs(&self) -> u64 {
        match self {
            Self::Launch { .. } => 180,
            Self::Shutdown => 20,
            Self::Downloads { wait_secs, .. } => wait_secs.unwrap_or(0).saturating_add(10),
            Self::Navigate { timeout_secs, .. }
            | Self::Evaluate { timeout_secs, .. }
            | Self::ClickSelector { timeout_secs, .. }
            | Self::Screenshot { timeout_secs, .. }
            | Self::Text { timeout_secs, .. }
            | Self::Html { timeout_secs, .. }
            | Self::Links { timeout_secs, .. }
            | Self::Data { timeout_secs, .. }
            | Self::Url { timeout_secs, .. }
            | Self::Wait { timeout_secs, .. }
            | Self::SetCookie { timeout_secs, .. }
            | Self::Type { timeout_secs, .. }
            | Self::Press { timeout_secs, .. }
            | Self::Fill { timeout_secs, .. }
            | Self::Hover { timeout_secs, .. }
            | Self::Select { timeout_secs, .. }
            | Self::Scroll { timeout_secs, .. }
            | Self::Tabs { timeout_secs, .. } => *timeout_secs,
            _ => 60,
        }
    }

    pub fn read_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs().saturating_add(30))
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

pub use super::error_kind::ErrorKind;

/// A response from the daemon to a CLI client.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable error category; absent on successes and older daemon responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl DaemonResponse {
    /// Create a success response with data.
    pub fn ok(data: Value) -> Self {
        DaemonResponse {
            ok: true,
            error: None,
            kind: None,
            data: Some(data),
        }
    }

    /// Create a success response with no data.
    pub fn ok_empty() -> Self {
        DaemonResponse {
            ok: true,
            error: None,
            kind: None,
            data: None,
        }
    }

    /// Create an error response.
    pub fn err(message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = ErrorKind::classify(&message).as_str().to_owned();
        DaemonResponse {
            ok: false,
            error: Some(message),
            kind: Some(kind),
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

    macro_rules! error_kind_case {
        ($name:ident, $message:expr, $kind:literal) => {
            #[test]
            fn $name() {
                let message = $message;
                let value = serde_json::to_value(DaemonResponse::err(message)).unwrap();
                assert_eq!(value["kind"], $kind);
                assert_eq!(value["error"], message);
                assert_eq!(value["ok"], false);
                assert!(value.get("data").is_none());
            }
        };
    }

    // src/cli/instance.rs: selector_box_in_frame, selector-not-found branch.
    error_kind_case!(
        selector_not_found_kind,
        "selector not found: #missing",
        "selector_not_found"
    );
    // src/cli/instance.rs: wait_deadline_normalizes_a_final_protocol_timeout.
    error_kind_case!(
        timeout_kind,
        "timed out after 0s waiting for selector: #never",
        "timeout"
    );
    // src/protocol/errors.rs: ProtocolError::dialog_opened.
    error_kind_case!(
        dialog_open_kind,
        "open alert dialog: \"hello\"",
        "dialog_open"
    );
    // src/cli/instance.rs: scoped_error.
    error_kind_case!(frame_detached_kind, "frame was detached", "frame_detached");
    // src/api/main_frame.rs: resolve_child_frame, missing objectId branch.
    error_kind_case!(
        frame_not_found_kind,
        "--frame #missing: no element matches",
        "frame_not_found"
    );
    // src/api/main_frame.rs: resolve_child_frame, missing contentFrameId branch.
    error_kind_case!(
        not_iframe_kind,
        "--frame #text: element is not an iframe",
        "not_iframe"
    );
    // src/api/selectors.rs: selector_exception and js_query's missing-root error.
    error_kind_case!(shadow_root_kind, "shadow root: #closed >>> button: element at segment 1 has no open shadow root (absent or closed; cannot pierce)", "shadow_root");
    // src/protocol/errors.rs: ProtocolError::navigation_became_download.
    error_kind_case!(
        navigation_kind,
        "navigation diverted into download flow (url https://example.com/fixture.pdf)",
        "navigation"
    );
    // src/cli/instance.rs: InstanceManager::missing.
    error_kind_case!(
        instance_not_found_kind,
        "instance 00000001 not found",
        "instance_not_found"
    );
    // src/cli/instance.rs: page_any.
    error_kind_case!(page_not_found_kind, "page p1 not found", "page_not_found");
    // src/cli/instance.rs: InstanceManager::launch_with.
    error_kind_case!(
        invalid_argument_kind,
        "--downloads-dir must be an absolute path",
        "invalid_argument"
    );
    // src/cli/instance.rs: write_screenshot, OpenOptions::open failure.
    error_kind_case!(
        io_kind,
        "failed to open screenshot: Permission denied (os error 13)",
        "io"
    );
    // src/cli/daemon.rs: dispatch_guarded.
    error_kind_case!(daemon_kind, "daemon shutting down", "daemon");
    // src/cli/instance.rs: data, unexpected metadata branch.
    error_kind_case!(other_kind, "unexpected metadata shape", "other");

    #[test]
    fn unknown_error_preserves_message_bytes_and_field_order() {
        let message = "unknown: é\n\t\0  ";
        let response = DaemonResponse::err(message);
        let serialized = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serialized,
            r#"{"ok":false,"error":"unknown: é\n\t\u0000  ","kind":"other"}"#
        );
        let decoded: DaemonResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(decoded.error.unwrap().as_bytes(), message.as_bytes());
    }

    #[test]
    fn success_responses_omit_kind() {
        assert_eq!(
            serde_json::to_string(&DaemonResponse::ok_empty()).unwrap(),
            r#"{"ok":true}"#
        );
        assert_eq!(
            serde_json::to_string(&DaemonResponse::ok(json!({"value": 1}))).unwrap(),
            r#"{"ok":true,"data":{"value":1}}"#
        );
    }

    #[test]
    fn legacy_error_without_kind_still_deserializes() {
        let response: DaemonResponse =
            serde_json::from_str(r#"{"ok":false,"error":"legacy error"}"#).unwrap();
        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("legacy error"));
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"ok":false,"error":"legacy error"}"#
        );
    }

    #[test]
    fn evaluate_main_world_round_trip_and_legacy_default() {
        for main_world in [false, true] {
            let payload = json!({
                "method": "Evaluate",
                "params": {
                    "instance_id": "i", "page_id": "p1",
                    "expression": "window.__app.n", "timeout_secs": 10,
                    "main_world": main_world
                }
            });
            let request: DaemonRequest = serde_json::from_value(payload.clone()).unwrap();
            assert!(
                matches!(request, DaemonRequest::Evaluate { main_world: flag, .. } if flag == main_world)
            );
            assert_eq!(serde_json::to_value(&request).unwrap(), payload);
        }

        let request: DaemonRequest = serde_json::from_value(json!({
            "method": "Evaluate",
            "params": {
                "instance_id": "i", "page_id": "p1", "expression": "1"
            }
        }))
        .unwrap();
        assert!(matches!(
            request,
            DaemonRequest::Evaluate {
                main_world: false,
                timeout_secs: 30,
                ..
            }
        ));
        let serialized = serde_json::to_value(&request).unwrap();
        assert_eq!(serialized["params"]["main_world"], false);
        let round_trip: DaemonRequest = serde_json::from_value(serialized).unwrap();
        assert!(matches!(
            round_trip,
            DaemonRequest::Evaluate {
                main_world: false,
                ..
            }
        ));
    }

    #[test]
    fn client_timeout_budget_per_request_type() {
        let timed = [
            "Navigate",
            "Evaluate",
            "ClickSelector",
            "Screenshot",
            "Text",
            "Html",
            "Links",
            "Data",
            "Url",
            "Wait",
            "SetCookie",
            "Type",
            "Press",
            "Fill",
            "Hover",
            "Select",
            "Scroll",
            "Tabs",
        ];
        for method in timed {
            let request: DaemonRequest = serde_json::from_value(json!({
                "method": method, "params": {
                    "instance_id": "i", "page_id": "p1", "url": "about:blank",
                    "expression": "1", "selector": "input", "name": "n", "value": "v",
                    "timeout_secs": 400, "text":"abc", "key":"Enter"
                }
            }))
            .unwrap();
            assert_eq!(request.timeout_secs(), 400, "{method}");
            assert_eq!(request.read_timeout().as_secs(), 430, "{method}");
        }
        let default_methods = [
            "Ping",
            "List",
            "Stop",
            "NewPage",
            "Click",
            "Cookies",
            "Back",
            "Forward",
            "Reload",
            "SetHeader",
            "CloseTab",
        ];
        for method in default_methods {
            let request: DaemonRequest = serde_json::from_value(if matches!(method, "Ping" | "List" | "Shutdown") {
                json!({"method": method})
            } else {
                json!({"method": method, "params": {"instance_id": "i", "page_id": "p1", "x": 1, "y": 2, "name": "n", "value": "v", "text": "t", "key": "Enter"}})
            }).unwrap();
            assert_eq!(request.read_timeout().as_secs(), 90, "{method}");
        }
        assert_eq!(DaemonRequest::Shutdown.read_timeout().as_secs(), 50);
        let launch = DaemonRequest::Launch {
            headless: None,
            executable: None,
            downloads_dir: None,
            bypass_csp: false,
            dialogs: Default::default(),
        };
        assert_eq!(launch.read_timeout().as_secs(), 210);
    }

    /// Old clients send `Launch` with only headless/executable (or nothing).
    /// Every launch flag added later must be `#[serde(default)]` so this
    /// payload keeps parsing.
    #[test]
    fn launch_minimal_payload_still_parses() {
        let request: DaemonRequest =
            serde_json::from_value(json!({"method": "Launch", "params": {}})).unwrap();
        assert!(matches!(request, DaemonRequest::Launch { .. }));
        let request: DaemonRequest = serde_json::from_value(
            json!({"method": "Launch", "params": {"headless": true, "executable": "/x"}}),
        )
        .unwrap();
        assert!(matches!(request, DaemonRequest::Launch { .. }));
    }

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
            frames: vec![],
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
            frames: vec![],
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
            frames: vec![],
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

    #[test]
    fn downloads_request_serde_round_trip() {
        let req = DaemonRequest::Downloads {
            instance_id: "00000001".into(),
            wait_secs: Some(10),
        };
        let serialized = serde_json::to_string(&req).expect("serialize");
        let deserialized: DaemonRequest = serde_json::from_str(&serialized).expect("deserialize");
        match deserialized {
            DaemonRequest::Downloads {
                instance_id,
                wait_secs,
            } => {
                assert_eq!(instance_id, "00000001");
                assert_eq!(wait_secs, Some(10));
            }
            other => panic!("expected Downloads, got {other:?}"),
        }
        assert_eq!(req.timeout_secs(), 20);

        // Minimal payload without wait_secs defaults to None and 10s timeout
        let minimal = r#"{"method":"Downloads","params":{"instance_id":"00000001"}}"#;
        let deserialized: DaemonRequest =
            serde_json::from_str(minimal).expect("deserialize minimal");
        match &deserialized {
            DaemonRequest::Downloads {
                instance_id,
                wait_secs,
            } => {
                assert_eq!(instance_id, "00000001");
                assert_eq!(*wait_secs, None);
            }
            other => panic!("expected Downloads, got {other:?}"),
        }
        assert_eq!(deserialized.timeout_secs(), 10);
    }

    #[test]
    fn frame_scoped_variants_parse_without_frames_and_round_trip() {
        let minimal_cases = [
            (
                r#"{"method":"Evaluate","params":{"instance_id":"i1","page_id":"p1","expression":"1+1"}}"#,
                "Evaluate",
            ),
            (
                r#"{"method":"ClickSelector","params":{"instance_id":"i1","page_id":"p1","selector":"button"}}"#,
                "ClickSelector",
            ),
            (
                r#"{"method":"Text","params":{"instance_id":"i1","page_id":"p1"}}"#,
                "Text",
            ),
            (
                r#"{"method":"Html","params":{"instance_id":"i1","page_id":"p1"}}"#,
                "Html",
            ),
            (
                r#"{"method":"Links","params":{"instance_id":"i1","page_id":"p1"}}"#,
                "Links",
            ),
            (
                r#"{"method":"Wait","params":{"instance_id":"i1","page_id":"p1","selector":"div"}}"#,
                "Wait",
            ),
            (
                r#"{"method":"Fill","params":{"instance_id":"i1","page_id":"p1","selector":"input","value":"abc"}}"#,
                "Fill",
            ),
            (
                r#"{"method":"Type","params":{"instance_id":"i1","page_id":"p1","text":"abc"}}"#,
                "Type",
            ),
            (
                r#"{"method":"Press","params":{"instance_id":"i1","page_id":"p1","key":"Enter"}}"#,
                "Press",
            ),
            (
                r#"{"method":"Hover","params":{"instance_id":"i1","page_id":"p1","selector":"a"}}"#,
                "Hover",
            ),
            (
                r#"{"method":"Select","params":{"instance_id":"i1","page_id":"p1","selector":"select","value":"opt"}}"#,
                "Select",
            ),
        ];

        for (json_str, name) in minimal_cases {
            let req: DaemonRequest = serde_json::from_str(json_str)
                .unwrap_or_else(|e| panic!("failed to parse minimal {name}: {e}"));
            match req {
                DaemonRequest::Evaluate { frames, .. }
                | DaemonRequest::ClickSelector { frames, .. }
                | DaemonRequest::Text { frames, .. }
                | DaemonRequest::Html { frames, .. }
                | DaemonRequest::Links { frames, .. }
                | DaemonRequest::Wait { frames, .. }
                | DaemonRequest::Fill { frames, .. }
                | DaemonRequest::Type { frames, .. }
                | DaemonRequest::Press { frames, .. }
                | DaemonRequest::Hover { frames, .. }
                | DaemonRequest::Select { frames, .. } => {
                    assert!(
                        frames.is_empty(),
                        "{name} frames should be empty by default"
                    );
                }
                _ => panic!("unexpected variant for {name}"),
            }
        }

        // Test roundtrip with explicit frames
        let req = DaemonRequest::Evaluate {
            instance_id: "i1".into(),
            page_id: "p1".into(),
            expression: "document.title".into(),
            main_world: false,
            timeout_secs: 10,
            frames: vec!["#outer".into(), "#inner".into()],
        };
        let s = serde_json::to_string(&req).expect("serialize");
        assert!(s.contains(r##""frames":["#outer","#inner"]"##));
        let de: DaemonRequest = serde_json::from_str(&s).expect("deserialize");
        match de {
            DaemonRequest::Evaluate { frames, .. } => {
                assert_eq!(frames, vec!["#outer".to_string(), "#inner".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }
}
