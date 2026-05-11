//! Main-frame domain wrapper.
//!
//! [`MainFrame`] wraps a page-scoped Juggler session pinned to the top frame
//! of a page. Every field is populated from authoritative protocol responses
//! at construction time, so a `MainFrame` cannot refer to a sub-frame.
//!
//! Created via
//! [`BrowserContext::new_main_frame`](crate::api::context::BrowserContext::new_main_frame).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::api::browser::Session;
use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};

// ---------------------------------------------------------------------------
// Supporting types
// ---------------------------------------------------------------------------

/// Options for page navigation.
///
/// Top-frame-only — there is no `frame_id` override. `MainFrame::navigate`
/// always operates on the main frame.
#[derive(Debug, Clone, Default)]
pub struct NavigateOptions {
    /// HTTP referer header to send with the navigation request.
    pub referer: Option<String>,
}

/// Options for taking a screenshot.
#[derive(Debug, Clone)]
pub struct ScreenshotOptions {
    /// MIME type: `"image/png"` or `"image/jpeg"`.
    pub mime_type: String,
    /// Clipping rectangle for the screenshot.
    pub clip: Rect,
    /// JPEG quality (0-100). Only used when `mime_type` is `"image/jpeg"`.
    pub quality: Option<u32>,
    /// Whether to omit the device scale factor from the screenshot.
    pub omit_device_scale_factor: Option<bool>,
}

/// A rectangle with floating-point coordinates.
#[derive(Debug, Clone)]
pub struct Rect {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
    /// Width.
    pub width: f64,
    /// Height.
    pub height: f64,
}

/// Parameters for `Page.dispatchKeyEvent`.
///
/// See PROTOCOL.md Section 7 for the full specification.
#[derive(Debug, Clone)]
pub struct KeyEventParams {
    /// Event type: `"keydown"` or `"keyup"`.
    pub r#type: String,
    /// Virtual key code (e.g., 65 for 'A').
    pub key_code: u32,
    /// Physical key code string (e.g., `"KeyA"`, `"Enter"`).
    pub code: String,
    /// Logical key string (e.g., `"a"`, `"Enter"`).
    pub key: String,
    /// Whether this is a key repeat.
    pub repeat: bool,
    /// Key location: 0=standard, 1=left, 2=right, 3=numpad.
    pub location: u32,
    /// Text input. `"\r"` is mapped to `""` by the browser.
    pub text: Option<String>,
}

/// Parameters for `Page.dispatchMouseEvent`.
///
/// See PROTOCOL.md Section 7 for the full specification.
#[derive(Debug, Clone)]
pub struct MouseEventParams {
    /// Event type: `"mousemove"`, `"mousedown"`, or `"mouseup"`.
    pub r#type: String,
    /// Button: 0=left, 1=middle, 2=right.
    pub button: u32,
    /// Button bitmask: 1=left, 2=right, 4=middle.
    pub buttons: u32,
    /// X coordinate (integer, floored).
    pub x: i32,
    /// Y coordinate (integer, floored).
    pub y: i32,
    /// Modifier bitmask: 1=Alt, 2=Control, 4=Shift, 8=Meta.
    pub modifiers: u32,
    /// Click count (for double-click detection, etc.).
    pub click_count: Option<u32>,
}

/// Parameters for `Page.dispatchWheelEvent`.
#[derive(Debug, Clone)]
pub struct WheelEventParams {
    /// X coordinate.
    pub x: i32,
    /// Y coordinate.
    pub y: i32,
    /// Horizontal scroll delta.
    pub delta_x: f64,
    /// Vertical scroll delta.
    pub delta_y: f64,
    /// Z-axis scroll delta.
    pub delta_z: f64,
    /// Modifier bitmask: 1=Alt, 2=Control, 4=Shift, 8=Meta.
    pub modifiers: u32,
}

/// Parameters for `Page.dispatchTapEvent`.
#[derive(Debug, Clone)]
pub struct TapEventParams {
    /// X coordinate.
    pub x: i32,
    /// Y coordinate.
    pub y: i32,
    /// Modifier bitmask: 1=Alt, 2=Control, 4=Shift, 8=Meta.
    pub modifiers: u32,
}

/// Emulated media settings for `Page.setEmulatedMedia`.
#[derive(Debug, Clone, Default)]
pub struct EmulatedMedia {
    /// Media type: `""`, `"screen"`, or `"print"`.
    pub r#type: String,
    /// Color scheme override.
    pub color_scheme: Option<String>,
    /// Reduced motion override.
    pub reduced_motion: Option<String>,
    /// Forced colors override.
    pub forced_colors: Option<String>,
    /// Contrast override.
    pub contrast: Option<String>,
}

/// A content quad (four points) returned by `Page.getContentQuads`.
#[derive(Debug, Clone)]
pub struct ContentQuad {
    /// First point.
    pub p1: Point,
    /// Second point.
    pub p2: Point,
    /// Third point.
    pub p3: Point,
    /// Fourth point.
    pub p4: Point,
}

/// A 2D point with floating-point coordinates.
#[derive(Debug, Clone)]
pub struct Point {
    /// X coordinate.
    pub x: f64,
    /// Y coordinate.
    pub y: f64,
}

// ---------------------------------------------------------------------------
// MainFrame
// ---------------------------------------------------------------------------

/// A page handle pinned to the top frame.
///
/// Every field is populated from authoritative protocol responses
/// (`Browser.attachedToTarget` filtered to `type == "page"`, the chosen
/// Layer-2 strategy, `Runtime.executionContextCreated` filtered to
/// `auxData.frameId == frame_id`) — never from "first event seen". A
/// `MainFrame` cannot be constructed referring to a sub-frame.
pub struct MainFrame {
    /// Page-scoped Juggler session.
    session: Session,
    /// Server-assigned target ID; the target has `type == "page"`.
    target_id: String,
    /// Top frame ID, populated at construction time.
    frame_id: String,
    /// Latest known main-world execution context ID for this frame.
    /// Updated by a `Runtime.executionContextCreated` listener registered
    /// in `BrowserContext::new_main_frame`, filtered on `auxData.frameId`.
    execution_context_id: Arc<Mutex<Option<String>>>,
}

impl MainFrame {
    /// Internal constructor used by `BrowserContext::new_main_frame`.
    pub(crate) fn new(
        session: Session,
        target_id: String,
        frame_id: String,
        execution_context_id: Arc<Mutex<Option<String>>>,
    ) -> Self {
        MainFrame {
            session,
            target_id,
            frame_id,
            execution_context_id,
        }
    }

    /// Returns the target ID for this page.
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// Returns the top frame ID.
    pub fn frame_id(&self) -> &str {
        &self.frame_id
    }

    /// Shared handle to the cached execution context id. Updated by the
    /// listener registered in `BrowserContext::new_main_frame`.
    pub(crate) fn execution_context_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.execution_context_id)
    }

    fn session(&self) -> &Session {
        &self.session
    }

    // -----------------------------------------------------------------------
    // Page domain methods
    // -----------------------------------------------------------------------

    /// Navigate to a URL.
    ///
    /// Always navigates the top frame (no `frame_id` override). Returns the
    /// navigation ID for cross-document navigations, `None` for same-document.
    pub fn navigate(
        &self,
        url: &str,
        options: NavigateOptions,
    ) -> Result<Option<String>, ProtocolError> {
        let mut params = json!({
            "url": url,
            "frameId": self.frame_id,
        });
        if let Some(ref referer) = options.referer {
            params["referer"] = json!(referer);
        }

        let result = self.session().send("Page.navigate", params)?;
        let nav_id = result
            .get("navigationId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());
        Ok(nav_id)
    }

    /// Reload the page.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn reload(&self) -> Result<(), ProtocolError> {
        self.session().send("Page.reload", json!({}))?;
        Ok(())
    }

    /// Go back in the navigation history.
    ///
    /// Returns `true` if the navigation was successful, `false` if there is
    /// no previous entry in the history.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn go_back(&self) -> Result<bool, ProtocolError> {
        let result = self.session().send(
            "Page.goBack",
            json!({ "frameId": self.frame_id() }),
        )?;
        Ok(result
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Go forward in the navigation history.
    ///
    /// Returns `true` if the navigation was successful, `false` if there is
    /// no next entry in the history.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn go_forward(&self) -> Result<bool, ProtocolError> {
        let result = self.session().send(
            "Page.goForward",
            json!({ "frameId": self.frame_id() }),
        )?;
        Ok(result
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Bring the page to the front (activate the tab).
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn bring_to_front(&self) -> Result<(), ProtocolError> {
        self.session().send("Page.bringToFront", json!({}))?;
        Ok(())
    }

    /// Set the viewport size for this page.
    ///
    /// Pass `None` to reset to the default viewport.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_viewport_size(
        &self,
        size: Option<(u32, u32)>,
    ) -> Result<(), ProtocolError> {
        let viewport = match size {
            Some((w, h)) => json!({ "width": w, "height": h }),
            None => serde_json::Value::Null,
        };
        self.session()
            .send("Page.setViewportSize", json!({ "viewportSize": viewport }))?;
        Ok(())
    }

    /// Set emulated media properties.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_emulated_media(&self, media: EmulatedMedia) -> Result<(), ProtocolError> {
        let mut params = json!({ "type": media.r#type });
        if let Some(ref cs) = media.color_scheme {
            params["colorScheme"] = json!(cs);
        }
        if let Some(ref rm) = media.reduced_motion {
            params["reducedMotion"] = json!(rm);
        }
        if let Some(ref fc) = media.forced_colors {
            params["forcedColors"] = json!(fc);
        }
        if let Some(ref c) = media.contrast {
            params["contrast"] = json!(c);
        }
        self.session().send("Page.setEmulatedMedia", params)?;
        Ok(())
    }

    /// Set whether the cache is disabled for this page.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_cache_disabled(&self, disabled: bool) -> Result<(), ProtocolError> {
        self.session().send(
            "Page.setCacheDisabled",
            json!({ "cacheDisabled": disabled }),
        )?;
        Ok(())
    }

    /// Set page-level init scripts.
    ///
    /// Replaces all previous init scripts for this page. Page-level scripts
    /// may include a `worldName` for isolation.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_init_scripts(
        &self,
        scripts: &[(String, Option<String>)],
    ) -> Result<(), ProtocolError> {
        let scripts_json: Vec<serde_json::Value> = scripts
            .iter()
            .map(|(script, world_name)| {
                let mut obj = json!({ "script": script });
                if let Some(ref wn) = world_name {
                    obj["worldName"] = json!(wn);
                }
                obj
            })
            .collect();

        self.session()
            .send("Page.setInitScripts", json!({ "scripts": scripts_json }))?;
        Ok(())
    }

    /// Set whether to intercept file chooser dialogs.
    ///
    /// This is a "sendMayFail" method; errors are silently swallowed.
    pub fn set_intercept_file_chooser_dialog(&self, enabled: bool) {
        {
            let s = self.session();
            s.send_may_fail(
                "Page.setInterceptFileChooserDialog",
                json!({ "enabled": enabled }),
            );
        }
    }

    /// Take a screenshot of the page.
    ///
    /// Returns the raw image bytes (decoded from base64).
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the screenshot command fails, or if
    /// the base64 data cannot be decoded.
    pub fn screenshot(
        &self,
        options: ScreenshotOptions,
    ) -> Result<Vec<u8>, ProtocolError> {
        let mut params = json!({
            "mimeType": options.mime_type,
            "clip": {
                "x": options.clip.x,
                "y": options.clip.y,
                "width": options.clip.width,
                "height": options.clip.height,
            },
        });
        if let Some(q) = options.quality {
            params["quality"] = json!(q);
        }
        if let Some(omit) = options.omit_device_scale_factor {
            params["omitDeviceScaleFactor"] = json!(omit);
        }

        let result = self.session().send("Page.screenshot", params)?;

        let b64_data = result
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Decode base64 using a simple decoder. We avoid adding a dependency
        // on the `base64` crate by implementing a minimal decoder.
        decode_base64(b64_data).map_err(|msg| ProtocolError {
            kind: crate::protocol::errors::ProtocolErrorKind::Response,
            method: Some("Page.screenshot".into()),
            message: msg,
            data: None,
            source: None,
        })
    }

    /// Describe a DOM node.
    ///
    /// Returns the `contentFrameId` and `ownerFrameId` for the given object.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn describe_node(
        &self,
        frame_id: &str,
        object_id: &str,
    ) -> Result<serde_json::Value, ProtocolError> {
        self.session().send(
            "Page.describeNode",
            json!({
                "frameId": frame_id,
                "objectId": object_id,
            }),
        )
    }

    /// Scroll a node into view if needed.
    ///
    /// # Known errors
    ///
    /// - `"Node is detached from document"` -- element no longer in DOM
    /// - `"Node does not have a layout object"` -- element not visible
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn scroll_into_view_if_needed(
        &self,
        frame_id: &str,
        object_id: &str,
        rect: Option<Rect>,
    ) -> Result<(), ProtocolError> {
        let mut params = json!({
            "frameId": frame_id,
            "objectId": object_id,
        });
        if let Some(ref r) = rect {
            params["rect"] = json!({
                "x": r.x,
                "y": r.y,
                "width": r.width,
                "height": r.height,
            });
        }
        self.session()
            .send("Page.scrollIntoViewIfNeeded", params)?;
        Ok(())
    }

    /// Get content quads for a DOM element.
    ///
    /// This is a "sendMayFail" method; returns `None` on error.
    pub fn get_content_quads(
        &self,
        frame_id: &str,
        object_id: &str,
    ) -> Option<Vec<ContentQuad>> {
        let s = self.session();
        let result = s.send_may_fail(
            "Page.getContentQuads",
            json!({
                "frameId": frame_id,
                "objectId": object_id,
            }),
        )?;

        let quads_arr = result.get("quads")?.as_array()?;
        let quads = quads_arr
            .iter()
            .filter_map(|q| {
                Some(ContentQuad {
                    p1: Point {
                        x: q.get("p1")?.get("x")?.as_f64()?,
                        y: q.get("p1")?.get("y")?.as_f64()?,
                    },
                    p2: Point {
                        x: q.get("p2")?.get("x")?.as_f64()?,
                        y: q.get("p2")?.get("y")?.as_f64()?,
                    },
                    p3: Point {
                        x: q.get("p3")?.get("x")?.as_f64()?,
                        y: q.get("p3")?.get("y")?.as_f64()?,
                    },
                    p4: Point {
                        x: q.get("p4")?.get("x")?.as_f64()?,
                        y: q.get("p4")?.get("y")?.as_f64()?,
                    },
                })
            })
            .collect();
        Some(quads)
    }

    /// Set files for a file input element.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_file_input_files(
        &self,
        frame_id: &str,
        object_id: &str,
        files: &[&str],
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Page.setFileInputFiles",
            json!({
                "frameId": frame_id,
                "objectId": object_id,
                "files": files,
            }),
        )?;
        Ok(())
    }

    /// Close this page.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn close(&self) -> Result<(), ProtocolError> {
        self.session().send("Page.close", json!({}))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Input event methods
    // -----------------------------------------------------------------------

    /// Dispatch a keyboard event.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn dispatch_key_event(
        &self,
        params: KeyEventParams,
    ) -> Result<(), ProtocolError> {
        let mut p = json!({
            "type": params.r#type,
            "keyCode": params.key_code,
            "code": params.code,
            "key": params.key,
            "repeat": params.repeat,
            "location": params.location,
        });
        if let Some(ref text) = params.text {
            p["text"] = json!(text);
        }
        self.session().send("Page.dispatchKeyEvent", p)?;
        Ok(())
    }

    /// Insert text at the current cursor position.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn insert_text(&self, text: &str) -> Result<(), ProtocolError> {
        self.session()
            .send("Page.insertText", json!({ "text": text }))?;
        Ok(())
    }

    /// Dispatch a mouse event.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn dispatch_mouse_event(
        &self,
        params: MouseEventParams,
    ) -> Result<(), ProtocolError> {
        let mut p = json!({
            "type": params.r#type,
            "button": params.button,
            "buttons": params.buttons,
            "x": params.x,
            "y": params.y,
            "modifiers": params.modifiers,
        });
        if let Some(cc) = params.click_count {
            p["clickCount"] = json!(cc);
        }
        self.session().send("Page.dispatchMouseEvent", p)?;
        Ok(())
    }

    /// Dispatch a wheel (scroll) event.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn dispatch_wheel_event(
        &self,
        params: WheelEventParams,
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Page.dispatchWheelEvent",
            json!({
                "x": params.x,
                "y": params.y,
                "deltaX": params.delta_x,
                "deltaY": params.delta_y,
                "deltaZ": params.delta_z,
                "modifiers": params.modifiers,
            }),
        )?;
        Ok(())
    }

    /// Dispatch a tap event.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn dispatch_tap_event(
        &self,
        params: TapEventParams,
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Page.dispatchTapEvent",
            json!({
                "x": params.x,
                "y": params.y,
                "modifiers": params.modifiers,
            }),
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Dialog handling
    // -----------------------------------------------------------------------

    /// Handle a dialog (alert, confirm, prompt, beforeunload).
    ///
    /// This is a "sendMayFail" method; the dialog may already be handled.
    pub fn handle_dialog(
        &self,
        dialog_id: &str,
        accept: bool,
        prompt_text: Option<&str>,
    ) {
        {
            let s = self.session();
            let mut params = json!({
                "dialogId": dialog_id,
                "accept": accept,
            });
            if let Some(text) = prompt_text {
                params["promptText"] = json!(text);
            }
            s.send_may_fail("Page.handleDialog", params);
        }
    }

    // -----------------------------------------------------------------------
    // Runtime domain methods
    // -----------------------------------------------------------------------

    /// Evaluate a JavaScript expression.
    ///
    /// Returns the result as a JSON value. The expression is evaluated in
    /// the main execution context of the main frame.
    ///
    /// # Error handling
    ///
    /// Evaluate a JavaScript expression in the top-frame main world.
    ///
    /// Polls the cached execution context (up to `timeout`); if `evaluate`
    /// fails with a "context destroyed" error (SPA navigation), retries up
    /// to 5 times after waiting for a fresh context.
    pub fn evaluate(
        &self,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, ProtocolError> {
        const MAX_RETRIES: u32 = 5;
        let deadline = Instant::now() + timeout;
        let mut bad_ctx: Option<String> = None;

        for attempt in 0..=MAX_RETRIES {
            if Instant::now() >= deadline {
                break;
            }

            // Acquire a usable execution context, skipping any known-bad one.
            let exec_ctx = loop {
                let cur = self.execution_context_id.lock().unwrap().clone();
                match cur {
                    Some(c) if bad_ctx.as_ref() != Some(&c) => break c,
                    _ => {
                        *self.execution_context_id.lock().unwrap() = None;
                        if Instant::now() >= deadline {
                            return Err(ProtocolError {
                                kind: ProtocolErrorKind::Closed,
                                method: Some("Runtime.evaluate".into()),
                                message: "timed out waiting for execution context".into(),
                                data: None,
                                source: None,
                            });
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            };

            match self.session().send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "executionContextId": &exec_ctx,
                }),
            ) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = format!("{e}");
                    let is_ctx_err = msg.contains("execution context")
                        || msg.contains("Failed to find");
                    if attempt < MAX_RETRIES && is_ctx_err {
                        bad_ctx = Some(exec_ctx);
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(ProtocolError {
            kind: ProtocolErrorKind::Closed,
            method: Some("Runtime.evaluate".into()),
            message: format!("evaluate failed after {MAX_RETRIES} retries"),
            data: None,
            source: None,
        })
    }

    /// Call a JavaScript function with arguments.
    ///
    /// `declaration` is the function source code (e.g., `"(a, b) => a + b"`).
    /// `args` is a list of argument descriptors, each with an optional
    /// `objectId` or `value`.
    ///
    /// Returns the full response including `result` and optional
    /// `exceptionDetails`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn call_function(
        &self,
        declaration: &str,
        args: Vec<serde_json::Value>,
        execution_context_id: &str,
    ) -> Result<serde_json::Value, ProtocolError> {
        self.session().send(
            "Runtime.callFunction",
            json!({
                "functionDeclaration": declaration,
                "args": args,
                "returnByValue": true,
                "executionContextId": execution_context_id,
            }),
        )
    }

    /// Get properties of a JavaScript object.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn get_object_properties(
        &self,
        execution_context_id: &str,
        object_id: &str,
    ) -> Result<serde_json::Value, ProtocolError> {
        self.session().send(
            "Runtime.getObjectProperties",
            json!({
                "executionContextId": execution_context_id,
                "objectId": object_id,
            }),
        )
    }

    /// Dispose of a JavaScript object handle.
    ///
    /// Releases the server-side reference to the object, allowing it to be
    /// garbage collected.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn dispose_object(
        &self,
        execution_context_id: &str,
        object_id: &str,
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Runtime.disposeObject",
            json!({
                "executionContextId": execution_context_id,
                "objectId": object_id,
            }),
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Network domain methods
    // -----------------------------------------------------------------------

    /// Set request interception for this page.
    ///
    /// When enabled, `Network.requestWillBeSent` events will have
    /// `isIntercepted: true`. Note: enabling interception also sends
    /// `Page.setCacheDisabled({cacheDisabled: true})` per Playwright's
    /// behavior.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_request_interception(&self, enabled: bool) -> Result<(), ProtocolError> {
        let s = self.session();
        s.send(
            "Network.setRequestInterception",
            json!({ "enabled": enabled }),
        )?;
        // Playwright also disables cache when interception is on.
        s.send(
            "Page.setCacheDisabled",
            json!({ "cacheDisabled": enabled }),
        )?;
        Ok(())
    }

    /// Set extra HTTP headers for this page.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_extra_http_headers(
        &self,
        headers: &[(&str, &str)],
    ) -> Result<(), ProtocolError> {
        let headers_json: Vec<serde_json::Value> = headers
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect();
        self.session().send(
            "Network.setExtraHTTPHeaders",
            json!({ "headers": headers_json }),
        )?;
        Ok(())
    }

    /// Get the response body for a completed request.
    ///
    /// Returns the raw body bytes (decoded from base64) and whether the
    /// body was evicted from memory.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn get_response_body(
        &self,
        request_id: &str,
    ) -> Result<(Vec<u8>, bool), ProtocolError> {
        let result = self.session().send(
            "Network.getResponseBody",
            json!({ "requestId": request_id }),
        )?;

        let b64 = result
            .get("base64body")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let evicted = result
            .get("evicted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let body = decode_base64(b64).map_err(|msg| ProtocolError {
            kind: crate::protocol::errors::ProtocolErrorKind::Response,
            method: Some("Network.getResponseBody".into()),
            message: msg,
            data: None,
            source: None,
        })?;

        Ok((body, evicted))
    }

    /// Resume an intercepted request.
    ///
    /// This is a "sendMayFail" method; the request may already be cancelled.
    pub fn resume_intercepted_request(
        &self,
        request_id: &str,
        url: Option<&str>,
        method: Option<&str>,
        headers: Option<&[(&str, &str)]>,
        post_data: Option<&str>,
    ) {
        {
            let s = self.session();
            let mut params = json!({ "requestId": request_id });
            if let Some(u) = url {
                params["url"] = json!(u);
            }
            if let Some(m) = method {
                params["method"] = json!(m);
            }
            if let Some(h) = headers {
                let headers_json: Vec<serde_json::Value> = h
                    .iter()
                    .map(|(name, value)| json!({"name": name, "value": value}))
                    .collect();
                params["headers"] = json!(headers_json);
            }
            if let Some(pd) = post_data {
                params["postData"] = json!(pd);
            }
            s.send_may_fail("Network.resumeInterceptedRequest", params);
        }
    }

    /// Fulfill an intercepted request with a custom response.
    ///
    /// This is a "sendMayFail" method; the request may already be cancelled.
    pub fn fulfill_intercepted_request(
        &self,
        request_id: &str,
        status: u16,
        status_text: &str,
        headers: &[(&str, &str)],
        base64_body: &str,
    ) {
        {
            let s = self.session();
            let headers_json: Vec<serde_json::Value> = headers
                .iter()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect();
            s.send_may_fail(
                "Network.fulfillInterceptedRequest",
                json!({
                    "requestId": request_id,
                    "status": status,
                    "statusText": status_text,
                    "headers": headers_json,
                    "base64body": base64_body,
                }),
            );
        }
    }

    /// Abort an intercepted request.
    ///
    /// `error_code` should be one of the valid abort error codes:
    /// `"aborted"`, `"accessdenied"`, `"addressunreachable"`,
    /// `"blockedbyclient"`, `"blockedbyresponse"`, `"connectionaborted"`,
    /// `"connectionclosed"`, `"connectionfailed"`, `"connectionrefused"`,
    /// `"connectionreset"`, `"internetdisconnected"`, `"namenotresolved"`,
    /// `"timedout"`, `"failed"`.
    ///
    /// This is a "sendMayFail" method; the request may already be cancelled.
    pub fn abort_intercepted_request(&self, request_id: &str, error_code: &str) {
        {
            let s = self.session();
            s.send_may_fail(
                "Network.abortInterceptedRequest",
                json!({
                    "requestId": request_id,
                    "errorCode": error_code,
                }),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Heap domain methods
    // -----------------------------------------------------------------------

    /// Force garbage collection.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn collect_garbage(&self) -> Result<(), ProtocolError> {
        self.session()
            .send("Heap.collectGarbage", json!({}))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Screencast methods
    // -----------------------------------------------------------------------

    /// Start screencast.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn start_screencast(
        &self,
        width: u32,
        height: u32,
        quality: u32,
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Page.startScreencast",
            json!({
                "width": width,
                "height": height,
                "quality": quality,
            }),
        )?;
        Ok(())
    }

    /// Stop screencast.
    ///
    /// This is a "sendMayFail" method; the page may have navigated.
    pub fn stop_screencast(&self) {
        {
            let s = self.session();
            s.send_may_fail("Page.stopScreencast", json!({}));
        }
    }

    /// Acknowledge a screencast frame.
    ///
    /// This is a "sendMayFail" method; the page may have navigated.
    pub fn screencast_frame_ack(&self) {
        {
            let s = self.session();
            s.send_may_fail("Page.screencastFrameAck", json!({}));
        }
    }

    /// Send a message to a worker.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn send_message_to_worker(
        &self,
        frame_id: &str,
        worker_id: &str,
        message: &str,
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Page.sendMessageToWorker",
            json!({
                "frameId": frame_id,
                "workerId": worker_id,
                "message": message,
            }),
        )?;
        Ok(())
    }

    /// Adopt a DOM node into a different execution context.
    ///
    /// Returns the remote object for the adopted node, or `None` if the node
    /// is detached.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn adopt_node(
        &self,
        frame_id: &str,
        object_id: Option<&str>,
        execution_context_id: &str,
    ) -> Result<Option<serde_json::Value>, ProtocolError> {
        let mut params = json!({
            "frameId": frame_id,
            "executionContextId": execution_context_id,
        });
        if let Some(oid) = object_id {
            params["objectId"] = json!(oid);
        }

        let result = self.session().send("Page.adoptNode", params)?;

        let remote_object = result.get("remoteObject").cloned();
        Ok(remote_object)
    }
}

impl std::fmt::Debug for MainFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainFrame")
            .field("target_id", &self.target_id)
            .field("frame_id", &self.frame_id)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Minimal base64 decoder (avoids external dependency)
// ---------------------------------------------------------------------------

/// Decode a base64-encoded string into raw bytes.
///
/// Supports standard base64 alphabet (RFC 4648) with optional padding.
fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    const DECODE_TABLE: [i8; 256] = {
        let mut table = [-1i8; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < 64 {
            table[alphabet[i] as usize] = i as i8;
            i += 1;
        }
        table[b'=' as usize] = -2; // padding marker
        table
    };

    if input.is_empty() {
        return Ok(Vec::new());
    }

    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;

    for &b in bytes {
        if b == b'\n' || b == b'\r' || b == b' ' {
            continue;
        }
        let val = DECODE_TABLE[b as usize];
        if val == -2 {
            // Padding -- stop processing.
            break;
        }
        if val == -1 {
            return Err(format!("invalid base64 character: {:?}", b as char));
        }
        buf = (buf << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_base64_empty() {
        assert_eq!(decode_base64("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn test_decode_base64_hello() {
        assert_eq!(
            decode_base64("SGVsbG8=").unwrap(),
            b"Hello".to_vec()
        );
    }

    #[test]
    fn test_decode_base64_no_padding() {
        assert_eq!(
            decode_base64("SGVsbG8").unwrap(),
            b"Hello".to_vec()
        );
    }

    #[test]
    fn test_decode_base64_with_whitespace() {
        assert_eq!(
            decode_base64("SGVs\nbG8=").unwrap(),
            b"Hello".to_vec()
        );
    }

    #[test]
    fn test_decode_base64_invalid_char() {
        assert!(decode_base64("SGVs!G8=").is_err());
    }
}
