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
use crate::protocol::client::Connection;
use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};
use crate::protocol::events::HandlerId;

/// Upper bound (ms) on how long `navigate` spins waiting for the main-document
/// HTTP status after the navigate ack. Kept short so callers without
/// `--wait-until` are not meaningfully delayed when Network events don't flow.
/// Lowered under `cfg(test)` so the no-event unit tests don't pay the full
/// production budget.
#[cfg(not(test))]
const MAX_STATUS_WAIT_MS: u64 = 500;
#[cfg(test)]
const MAX_STATUS_WAIT_MS: u64 = 80;

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
    /// If set, block after the `Page.navigate` ack until the named lifecycle
    /// event fires on the page session.
    ///
    /// Supported values: `"load"` and `"domcontentloaded"`.
    /// - `"load"` waits for the `Page.eventFired { name: "load" }` event.
    /// - `"domcontentloaded"` waits for `Page.eventFired { name: "DOMContentLoaded" }`.
    ///
    /// Any other value (including `"networkidle"`) is rejected with a clear error
    /// before the navigate is issued.
    pub wait_until: Option<String>,
}

/// The outcome of a `MainFrame::navigate` call.
///
/// `navigation_id` carries the server-assigned navigation id (same semantics
/// as before: `None` for same-document navigations). `status_code` carries
/// the HTTP status of the **main-document** response (`None` when the status
/// could not be captured — e.g. for `file://` URLs or network errors before
/// a response arrived). `navigate` **never** fails because of a non-2xx
/// status; callers inspect `status_code` themselves.
#[derive(Debug, Clone)]
pub struct NavigateOutcome {
    /// Server-assigned navigation id, or `None` for same-document navigations.
    pub nav_id: Option<String>,
    /// HTTP status code of the main-document response, or `None` if not
    /// captured (e.g. `file://` URL, connection error, or the browser did not
    /// emit the network events).
    pub status_code: Option<u16>,
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
// SubscriptionGuard
// ---------------------------------------------------------------------------

/// RAII guard that deregisters an event handler when dropped.
///
/// Holding one of these guarantees the `(session_key, method, id)` handler is
/// removed from the connection's [`EventRouter`](crate::protocol::events::EventRouter)
/// on every exit path of the enclosing scope — normal return, early return,
/// or panic — so transient subscriptions (e.g. a single-shot lifecycle wait)
/// can never leak a dead closure.
struct SubscriptionGuard<'a> {
    connection: &'a Connection,
    session_key: &'a str,
    method: &'static str,
    id: HandlerId,
}

impl Drop for SubscriptionGuard<'_> {
    fn drop(&mut self) {
        self.connection
            .off_event(self.session_key, self.method, self.id);
    }
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
    /// Shared connection — used for `on_event` subscriptions (e.g. lifecycle wait).
    connection: Arc<Connection>,
}

impl MainFrame {
    /// Internal constructor used by `BrowserContext::new_main_frame`.
    pub(crate) fn new(
        session: Session,
        target_id: String,
        frame_id: String,
        execution_context_id: Arc<Mutex<Option<String>>>,
        connection: Arc<Connection>,
    ) -> Self {
        MainFrame {
            session,
            target_id,
            frame_id,
            execution_context_id,
            connection,
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
    #[cfg_attr(not(feature = "cli"), allow(dead_code))]
    pub(crate) fn execution_context_handle(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.execution_context_id)
    }

    fn session(&self) -> &Session {
        &self.session
    }

    // -----------------------------------------------------------------------
    // Lifecycle-event helpers
    // -----------------------------------------------------------------------

    /// Map a CLI `wait_until` value to the Juggler `Page.eventFired` name.
    ///
    /// - `"load"` → `"load"`
    /// - `"domcontentloaded"` → `"DOMContentLoaded"`
    /// - anything else → `Err` with a clear message naming the supported set.
    ///
    /// This is the canonical validation point; call it before issuing
    /// `Page.navigate` so unsupported values error before any network I/O.
    pub fn map_wait_until(value: &str) -> Result<&'static str, ProtocolError> {
        match value {
            "load" => Ok("load"),
            "domcontentloaded" => Ok("DOMContentLoaded"),
            other => Err(ProtocolError {
                kind: ProtocolErrorKind::Response,
                method: Some("Page.navigate".into()),
                message: format!(
                    "unsupported --wait-until value {:?}; supported values are: load, domcontentloaded",
                    other
                ),
                data: None,
                source: None,
                download_info: None,
            }),
        }
    }

    /// Block until a `Page.eventFired` event with the given `name` fires on
    /// this page's session and frame, or until `timeout` elapses.
    ///
    /// The Juggler protocol emits `Page.eventFired { frameId, name }` on the
    /// page session. This helper subscribes to that event on the page session
    /// key, filters to the matching `frameId` and `name`, forwards the match
    /// to an `mpsc` channel, then `recv_timeout`s on the channel.
    ///
    /// # Handler cleanup (no leak)
    ///
    /// The handler is registered via [`Connection::on_event`], which returns a
    /// `HandlerId`. An RAII guard ([`SubscriptionGuard`]) deregisters the
    /// handler via [`Connection::off_event`] on **every** exit path — success,
    /// timeout, error, or panic — so a `--wait-until` navigate never leaks a
    /// closure into the `EventRouter`.
    ///
    /// # Arguments
    ///
    /// - `event_name` — the Juggler event name, e.g. `"load"` or `"DOMContentLoaded"`.
    /// - `timeout` — maximum time to wait.
    ///
    /// # Errors
    ///
    /// Returns a `Timeout` error if the event does not arrive within `timeout`,
    /// leaving the session in a usable state.
    pub fn wait_for_lifecycle(
        &self,
        event_name: &str,
        timeout: Duration,
    ) -> Result<(), ProtocolError> {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel::<()>();
        let frame_id = self.frame_id.clone();
        let expected_name = event_name.to_owned();
        let session_key = self.session.key().to_owned();

        let handler_id = self.connection.on_event(
            &session_key,
            "Page.eventFired",
            Box::new(move |event| {
                let ev_name = event
                    .params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let ev_frame = event
                    .params
                    .get("frameId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if ev_name == expected_name && ev_frame == frame_id {
                    // Ignore send error: the receiver may have already dropped
                    // (e.g. timeout path ran first). This is benign.
                    let _ = tx.send(());
                }
            }),
        );

        // RAII: deregister the handler on every exit path (success, timeout,
        // error, panic) so the closure cannot leak in the router.
        let _guard = SubscriptionGuard {
            connection: &self.connection,
            session_key: &session_key,
            method: "Page.eventFired",
            id: handler_id,
        };

        match rx.recv_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ProtocolError {
                kind: ProtocolErrorKind::Timeout,
                method: Some("Page.navigate".into()),
                message: format!(
                    "timed out waiting for lifecycle event {:?} after {:?}",
                    event_name, timeout
                ),
                data: None,
                source: None,
                download_info: None,
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProtocolError {
                kind: ProtocolErrorKind::Closed,
                method: Some("Page.navigate".into()),
                message: format!(
                    "channel disconnected while waiting for lifecycle event {:?}",
                    event_name
                ),
                data: None,
                source: None,
                download_info: None,
            }),
        }
    }

    // -----------------------------------------------------------------------
    // Page domain methods
    // -----------------------------------------------------------------------

    /// Navigate to a URL.
    ///
    /// Always navigates the top frame (no `frame_id` override). Returns a
    /// [`NavigateOutcome`] that includes:
    /// - `nav_id`: the navigation ID for cross-document navigations, `None`
    ///   for same-document (unchanged semantics).
    /// - `status_code`: the HTTP status of the **main-document** response
    ///   (e.g. 200, 404). `None` when the status cannot be captured (e.g.
    ///   `file://` URLs, `about:blank`, or when `Network.responseReceived`
    ///   did not arrive within the deadline). **`navigate` never fails because
    ///   of a non-2xx status** — callers inspect `status_code` themselves.
    ///
    /// `timeout` bounds the **whole** operation — both the wait for the
    /// renderer's response to `Page.navigate` AND (when `wait_until` is set)
    /// the subsequent wait for the lifecycle event. A single deadline is
    /// captured before the RPC and the lifecycle wait gets only the time that
    /// remains, so navigate+wait can never exceed `timeout`. If the renderer
    /// never responds (e.g. the response was a download and no DOM was
    /// created), the call returns
    /// [`ProtocolErrorKind::Timeout`](crate::protocol::errors::ProtocolErrorKind::Timeout)
    /// rather than hanging.
    ///
    /// Invalidates the cached execution context on cross-document navigation
    /// so a subsequent [`evaluate`](Self::evaluate) call waits for the new
    /// document's main-world context rather than racing against the stale
    /// pre-navigation context. (The Layer-3 destroyed-event listener also
    /// clears the cache, but the event can arrive after the next evaluate.)
    pub fn navigate(
        &self,
        url: &str,
        options: NavigateOptions,
        timeout: Duration,
    ) -> Result<NavigateOutcome, ProtocolError> {
        // Validate wait_until before issuing any network I/O.
        let lifecycle_event: Option<&'static str> = match options.wait_until.as_deref() {
            None | Some("") => None,
            Some(v) => Some(Self::map_wait_until(v)?),
        };

        // Single deadline bounds the navigate RPC AND the lifecycle wait, so
        // the combined operation never exceeds `timeout` (no double-counting).
        let deadline = Instant::now() + timeout;

        let session_key = self.session.key().to_owned();
        let frame_id = self.frame_id.clone();

        // -----------------------------------------------------------------------
        // G4: Subscribe Network event handlers BEFORE issuing Page.navigate.
        //
        // Correlation approach (redirect-aware):
        // 1. `Network.requestWillBeSent { requestId, navigationId, cause,
        //    redirectedFrom }`:
        //    - LOCK: when `cause` is a document type, record the first matching
        //      `requestId` as `main_request_id`. We accept the first document
        //      request speculatively (expected_nav_id is None until the navigate
        //      ack returns the nav_id); after the ack the nav_id is stored so the
        //      handler can gate on it.
        //    - CHAIN FORWARD: when `redirectedFrom` equals the currently-tracked
        //      `main_request_id`, the navigation followed a redirect (301/302/
        //      307/308) and the browser issued a new request for the redirect
        //      target. Update `main_request_id` to this new `requestId` so the
        //      FINAL hop's status wins, not the redirect hop's.
        // 2. `Network.responseReceived { requestId, status }`:
        //    capture the status for whatever the CURRENT `main_request_id` is,
        //    and ALLOW OVERWRITE — on a redirect chain `responseReceived` fires
        //    for each hop (301 then 200/404…); the last one (the final target)
        //    must win.
        //
        // Both handlers are RAII-guarded — they deregister on all exit paths.
        // -----------------------------------------------------------------------

        /// Shared state written by the two handlers.
        #[derive(Default)]
        struct NetState {
            /// The requestId of the main-document request being tracked. Advances
            /// forward along the redirect chain as `redirectedFrom` links match.
            main_request_id: Option<String>,
            /// Status of the most recent matching response. Overwritten as the
            /// redirect chain progresses so the FINAL hop's status is reported.
            status: Option<u16>,
        }
        let net_state: Arc<Mutex<NetState>> = Arc::new(Mutex::new(NetState::default()));
        // Populated after Page.navigate returns; handlers read it under the same Mutex.
        let expected_nav_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        // Handler 1: Network.requestWillBeSent
        let state_rws = Arc::clone(&net_state);
        let nav_id_rws = Arc::clone(&expected_nav_id);
        let frame_id_rws = frame_id.clone();
        let rws_id = self.connection.on_event(
            &session_key,
            "Network.requestWillBeSent",
            Box::new(move |event| {
                let req_id = match event.params.get("requestId").and_then(|v| v.as_str()) {
                    Some(id) => id.to_owned(),
                    None => return,
                };

                // Frame filter: skip requests for other frames if frameId present.
                if let Some(ev_frame) = event.params.get("frameId").and_then(|v| v.as_str()) {
                    if !ev_frame.is_empty() && ev_frame != frame_id_rws {
                        return;
                    }
                }

                let ev_nav_id = event
                    .params
                    .get("navigationId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_owned());
                let expected = nav_id_rws.lock().unwrap().clone();
                // Accept if: nav_id not yet known (speculative), OR nav_ids match,
                // OR event carries no nav_id (some same-doc navigations).
                let nav_id_ok = match (&expected, &ev_nav_id) {
                    (None, _) => true,
                    (Some(exp), Some(ev)) => exp == ev,
                    (Some(_), None) => true,
                };
                if !nav_id_ok {
                    return;
                }

                let redirected_from = event.params.get("redirectedFrom").and_then(|v| v.as_str());

                let mut st = state_rws.lock().unwrap();

                // CHAIN FORWARD: a redirect hop's continuation request whose
                // `redirectedFrom` is the request we're tracking. Advance the
                // tracked id to this hop so the final response's status wins.
                // Checked before the document-cause gate because redirect
                // continuations are tied to the chain, not re-discovered.
                if let Some(from) = redirected_from {
                    if st.main_request_id.as_deref() == Some(from) {
                        st.main_request_id = Some(req_id);
                        return;
                    }
                    // A `redirectedFrom` that does not match our chain belongs to
                    // some other request; ignore it (cannot hijack the chain).
                    return;
                }

                // LOCK: only the FIRST document-type request (no redirectedFrom)
                // establishes the chain head. Accept both the spec-mapped value
                // ("document") and the raw Firefox type names emitted in practice
                // ("TYPE_DOCUMENT", "TYPE_SUBDOCUMENT", "TYPE_REFRESH").
                let cause = event
                    .params
                    .get("cause")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let is_document_cause = matches!(
                    cause,
                    "document" | "TYPE_DOCUMENT" | "TYPE_SUBDOCUMENT" | "TYPE_REFRESH"
                );
                if is_document_cause && st.main_request_id.is_none() {
                    st.main_request_id = Some(req_id);
                }
            }),
        );
        let _guard_rws = SubscriptionGuard {
            connection: &self.connection,
            session_key: &session_key,
            method: "Network.requestWillBeSent",
            id: rws_id,
        };

        // Handler 2: Network.responseReceived
        let state_rr = Arc::clone(&net_state);
        let rr_id = self.connection.on_event(
            &session_key,
            "Network.responseReceived",
            Box::new(move |event| {
                let req_id = match event.params.get("requestId").and_then(|v| v.as_str()) {
                    Some(id) => id,
                    None => return,
                };
                let mut st = state_rr.lock().unwrap();
                // Capture for the CURRENT tracked request and ALLOW OVERWRITE so
                // that on a redirect chain (A→B), the final hop's status wins:
                // respReceived A sets 301, then after the chain advances to B,
                // respReceived B overwrites to the final 200/404.
                if st.main_request_id.as_deref() == Some(req_id) {
                    if let Some(s) = event.params.get("status").and_then(|v| v.as_u64()) {
                        st.status = u16::try_from(s).ok();
                    }
                }
            }),
        );
        let _guard_rr = SubscriptionGuard {
            connection: &self.connection,
            session_key: &session_key,
            method: "Network.responseReceived",
            id: rr_id,
        };

        // -----------------------------------------------------------------------

        let mut params = json!({
            "url": url,
            "frameId": self.frame_id,
        });
        if let Some(ref referer) = options.referer {
            params["referer"] = json!(referer);
        }

        let result = self.session().send_navigate_with_timeout(
            "Page.navigate",
            params,
            &self.frame_id,
            timeout,
        )?;
        let nav_id = result
            .get("navigationId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned());

        // Publish the nav_id so the requestWillBeSent handler can validate
        // any speculatively accepted request.
        *expected_nav_id.lock().unwrap() = nav_id.clone();

        // Cross-document navigation invalidates the pre-nav exec context.
        // Clear the cache so subsequent evaluate() calls wait for the new
        // document's main-world context.
        if nav_id.is_some() {
            *self.execution_context_id.lock().unwrap() = None;
        }

        // If a lifecycle event was requested, block until it fires — but only
        // for the time remaining against the single navigate deadline.
        if let Some(event_name) = lifecycle_event {
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.wait_for_lifecycle(event_name, remaining)?;
        }

        // Poll briefly for the status code. The response typically arrives
        // near-instantly after the navigate ack (or during the lifecycle wait).
        // Cap at 500 ms so we don't meaningfully delay callers without
        // --wait-until when Network events don't flow.
        //
        // Redirect-aware: a 3xx status means the chain is still in flight (the
        // browser will issue the redirect-target request next), so we keep
        // polling until a FINAL (non-3xx) status arrives or the cap elapses —
        // otherwise we'd return the redirect hop's status (e.g. 301) instead of
        // the final 200/404.
        const MAX_STATUS_WAIT: Duration = Duration::from_millis(MAX_STATUS_WAIT_MS);
        let status_cap = Instant::now() + MAX_STATUS_WAIT;
        let effective_deadline = status_cap.min(deadline);
        let is_final = |s: Option<u16>| matches!(s, Some(code) if !(300..400).contains(&code));
        let status_code = loop {
            {
                let st = net_state.lock().unwrap();
                if is_final(st.status) {
                    break st.status;
                }
            }
            if Instant::now() >= effective_deadline {
                // Return whatever we have (could be a 3xx if a redirect never
                // resolved within the budget, or None if nothing arrived).
                break net_state.lock().unwrap().status;
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        // _guard_rws and _guard_rr drop here — both handlers are deregistered
        // on every exit path (success, error, timeout, panic).
        Ok(NavigateOutcome {
            nav_id,
            status_code,
        })
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
        let result = self
            .session()
            .send("Page.goBack", json!({ "frameId": self.frame_id() }))?;
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
        let result = self
            .session()
            .send("Page.goForward", json!({ "frameId": self.frame_id() }))?;
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
    pub fn set_viewport_size(&self, size: Option<(u32, u32)>) -> Result<(), ProtocolError> {
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
    pub fn screenshot(&self, options: ScreenshotOptions) -> Result<Vec<u8>, ProtocolError> {
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

        let b64_data = result.get("data").and_then(|v| v.as_str()).unwrap_or("");

        // Decode base64 using a simple decoder. We avoid adding a dependency
        // on the `base64` crate by implementing a minimal decoder.
        decode_base64(b64_data).map_err(|msg| ProtocolError {
            kind: crate::protocol::errors::ProtocolErrorKind::Response,
            method: Some("Page.screenshot".into()),
            message: msg,
            data: None,
            source: None,
            download_info: None,
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
        self.session().send("Page.scrollIntoViewIfNeeded", params)?;
        Ok(())
    }

    /// Get content quads for a DOM element.
    ///
    /// This is a "sendMayFail" method; returns `None` on error.
    pub fn get_content_quads(&self, frame_id: &str, object_id: &str) -> Option<Vec<ContentQuad>> {
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
    pub fn dispatch_key_event(&self, params: KeyEventParams) -> Result<(), ProtocolError> {
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
    pub fn dispatch_mouse_event(&self, params: MouseEventParams) -> Result<(), ProtocolError> {
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
    pub fn dispatch_wheel_event(&self, params: WheelEventParams) -> Result<(), ProtocolError> {
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
    pub fn dispatch_tap_event(&self, params: TapEventParams) -> Result<(), ProtocolError> {
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
    pub fn handle_dialog(&self, dialog_id: &str, accept: bool, prompt_text: Option<&str>) {
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

    /// Evaluate a JavaScript expression in the top-frame main world.
    ///
    /// Polls the cached execution context (up to `timeout`); if `evaluate`
    /// fails with a "context destroyed" error (typical during SPA
    /// navigation), retries up to 5 times after waiting for a fresh
    /// context.
    ///
    /// # Note on `Runtime.executionContextDestroyed`
    ///
    /// The Layer-3 listener installed by
    /// [`BrowserContext::new_main_frame`](crate::api::context::BrowserContext::new_main_frame)
    /// proactively clears the cached execution context when its
    /// `Runtime.executionContextDestroyed` matches, so the next call here
    /// sees `None` and waits for a fresh context.
    ///
    /// As a safety net, this method ALSO retries up to 5 times on a
    /// "context destroyed" error response — useful when an evaluate
    /// happens to race the destroyed event over the wire.
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
                                download_info: None,
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
                    let is_ctx_err =
                        msg.contains("execution context") || msg.contains("Failed to find");
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
            download_info: None,
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
        s.send("Page.setCacheDisabled", json!({ "cacheDisabled": enabled }))?;
        Ok(())
    }

    /// Set extra HTTP headers for this page.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_extra_http_headers(&self, headers: &[(&str, &str)]) -> Result<(), ProtocolError> {
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
    pub fn get_response_body(&self, request_id: &str) -> Result<(Vec<u8>, bool), ProtocolError> {
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
            download_info: None,
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
        self.session().send("Heap.collectGarbage", json!({}))?;
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
        assert_eq!(decode_base64("SGVsbG8=").unwrap(), b"Hello".to_vec());
    }

    #[test]
    fn test_decode_base64_no_padding() {
        assert_eq!(decode_base64("SGVsbG8").unwrap(), b"Hello".to_vec());
    }

    #[test]
    fn test_decode_base64_with_whitespace() {
        assert_eq!(decode_base64("SGVs\nbG8=").unwrap(), b"Hello".to_vec());
    }

    #[test]
    fn test_decode_base64_invalid_char() {
        assert!(decode_base64("SGVs!G8=").is_err());
    }

    // -----------------------------------------------------------------------
    // Regression: `navigate` must honor its `timeout: Duration` parameter.
    //
    // Before the fix, `MainFrame::navigate` accepted no timeout and called
    // `Session::send`, which blocked on `rx.recv()` indefinitely. This test
    // builds a MainFrame around a MockTransport that never responds, then
    // confirms that `navigate(url, opts, deadline)` returns a Timeout error
    // within the deadline.
    // -----------------------------------------------------------------------

    use crate::protocol::client::Connection;
    use crate::protocol::errors::ProtocolErrorKind;
    use crate::protocol::types::RawMessage;
    use crate::transport::errors::TransportError;
    use crate::transport::Transport;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc as StdArc;

    /// Silent mock transport: send drops messages, receive blocks until
    /// the transport is closed (then returns Closed).
    struct SilentMockTransport {
        closed: StdArc<AtomicBool>,
        // Capture outgoing messages so tests can inspect them.
        outgoing: mpsc::Sender<serde_json::Value>,
    }

    impl Transport for SilentMockTransport {
        fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError> {
            if self.closed.load(Ordering::SeqCst) {
                return Err(TransportError::Closed);
            }
            let _ = self.outgoing.send(message.clone());
            Ok(())
        }

        fn receive(&mut self) -> Result<RawMessage, TransportError> {
            loop {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(TransportError::Closed);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn close(&mut self) -> Result<(), TransportError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn navigate_honors_timeout_argument() {
        let closed = StdArc::new(AtomicBool::new(false));
        let (out_tx, out_rx) = mpsc::channel();
        let transport = SilentMockTransport {
            closed: StdArc::clone(&closed),
            outgoing: out_tx,
        };
        let conn = Connection::new(Box::new(transport));
        let conn_arc = StdArc::new(conn);
        let session = conn_arc.root_session(); // root session is fine for this test
        let exec_ctx = StdArc::new(Mutex::new(None));
        let main_frame = MainFrame::new(
            session,
            "target-test".to_owned(),
            "frame-test".to_owned(),
            exec_ctx,
            StdArc::clone(&conn_arc),
        );

        let start = Instant::now();
        let result = main_frame.navigate(
            "https://example.com/never-responds",
            Default::default(),
            Duration::from_millis(150),
        );
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout, got {:?}", result.ok());
        let err = result.unwrap_err();
        assert_eq!(
            err.kind,
            ProtocolErrorKind::Timeout,
            "expected Timeout kind, got {:?}",
            err.kind
        );
        assert_eq!(err.method.as_deref(), Some("Page.navigate"));
        assert!(
            elapsed >= Duration::from_millis(140),
            "returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "returned too late: {elapsed:?}"
        );

        // Confirm the outgoing wire request was a Page.navigate.
        let sent = out_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("Page.navigate was not actually sent");
        assert_eq!(sent["method"], "Page.navigate");
        assert_eq!(sent["params"]["url"], "https://example.com/never-responds");
        assert_eq!(sent["params"]["frameId"], "frame-test");

        // Force the connection to shut down so the reader thread exits.
        closed.store(true, Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // G3: navigate --wait-until unit tests
    //
    // Uses a `MockTransport` that supports both outgoing capture AND incoming
    // message injection, mirroring the pattern in protocol::client::tests.
    // -----------------------------------------------------------------------

    /// A full mock transport: `receive()` blocks on a channel of injected
    /// `RawMessage`s; `send()` forwards to an outgoing channel.
    struct MockTransport {
        incoming_rx: mpsc::Receiver<RawMessage>,
        outgoing_tx: mpsc::Sender<serde_json::Value>,
        closed: StdArc<AtomicBool>,
    }

    impl Transport for MockTransport {
        fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError> {
            if self.closed.load(Ordering::SeqCst) {
                return Err(TransportError::Closed);
            }
            let _ = self.outgoing_tx.send(message.clone());
            Ok(())
        }

        fn receive(&mut self) -> Result<RawMessage, TransportError> {
            loop {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(TransportError::Closed);
                }
                match self.incoming_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(msg) => return Ok(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(TransportError::Closed)
                    }
                }
            }
        }

        fn close(&mut self) -> Result<(), TransportError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    /// Build a `MainFrame` backed by a `MockTransport` where:
    /// - `in_tx`: inject `RawMessage`s for the reader thread to process.
    /// - `out_rx`: receive outgoing JSON messages the frame sends.
    /// - the frame is on a page session with key `session_key`.
    struct MockFrameHarness {
        conn: StdArc<Connection>,
        frame: MainFrame,
        in_tx: mpsc::Sender<RawMessage>,
        out_rx: mpsc::Receiver<serde_json::Value>,
        closed: StdArc<AtomicBool>,
    }

    fn build_mock_frame(session_key: &str, frame_id: &str) -> MockFrameHarness {
        let (in_tx, in_rx) = mpsc::channel::<RawMessage>();
        let (out_tx, out_rx) = mpsc::channel::<serde_json::Value>();
        let closed = StdArc::new(AtomicBool::new(false));

        let transport = MockTransport {
            incoming_rx: in_rx,
            outgoing_tx: out_tx,
            closed: StdArc::clone(&closed),
        };

        let conn = Connection::new(Box::new(transport));
        // Create the page session.
        let page_session = conn.create_session(session_key.to_owned());
        let conn_arc = StdArc::new(conn);

        let exec_ctx = StdArc::new(Mutex::new(None::<String>));
        let frame = MainFrame::new(
            page_session,
            "target-test".to_owned(),
            frame_id.to_owned(),
            StdArc::clone(&exec_ctx),
            StdArc::clone(&conn_arc),
        );

        MockFrameHarness {
            conn: conn_arc,
            frame,
            in_tx,
            out_rx,
            closed,
        }
    }

    /// Helper: build a `Page.navigate` success response for a given request id.
    fn navigate_success_response(id: i64, session_key: &str) -> RawMessage {
        RawMessage {
            id: Some(id),
            method: None,
            params: None,
            result: Some(serde_json::json!({ "navigationId": "nav-1" })),
            error: None,
            session_id: Some(session_key.to_owned()),
        }
    }

    /// Helper: build a `Page.eventFired` event message.
    fn event_fired_message(name: &str, frame_id: &str, session_key: &str) -> RawMessage {
        RawMessage {
            id: None,
            method: Some("Page.eventFired".to_owned()),
            params: Some(serde_json::json!({
                "name": name,
                "frameId": frame_id,
            })),
            result: None,
            error: None,
            session_id: Some(session_key.to_owned()),
        }
    }

    /// G3 TDD case 1: navigate with `wait_until=Some("load")` blocks until
    /// the `Page.eventFired { name: "load" }` event fires on the page session.
    ///
    /// Script: ack `Page.navigate` → emit `Page.eventFired{name:"load"}` after
    /// a short delay; assert that `navigate()` returns only after the event.
    #[test]
    fn navigate_wait_until_load_returns_after_event_fires() {
        let h = build_mock_frame("page-A", "frame-1");

        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        // Responder thread: wait for the outgoing Page.navigate, ack it,
        // then emit the Page.eventFired{load} event after a small delay.
        let responder = std::thread::spawn(move || {
            // Drain the Page.navigate request.
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate was not sent");
            let id = sent["id"].as_i64().expect("id present");
            assert_eq!(sent["method"], "Page.navigate");

            // Ack the navigate.
            in_tx
                .send(navigate_success_response(id, "page-A"))
                .expect("send nav response");

            // Small delay to prove the caller is *actually* blocking.
            std::thread::sleep(Duration::from_millis(50));

            // Emit the lifecycle event.
            in_tx
                .send(event_fired_message("load", "frame-1", "page-A"))
                .expect("send eventFired");
        });

        let start = Instant::now();
        let result = h.frame.navigate(
            "https://example.com",
            NavigateOptions {
                wait_until: Some("load".into()),
                ..Default::default()
            },
            Duration::from_secs(10),
        );
        let elapsed = start.elapsed();

        assert!(result.is_ok(), "navigate should succeed: {result:?}");
        // Must have waited for the event (≥ 50ms delay).
        assert!(
            elapsed >= Duration::from_millis(40),
            "returned too early ({elapsed:?}); should have waited for event"
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);
    }

    /// G3 TDD case 2: lifecycle event never arrives → Timeout error within
    /// the bound; session remains usable (no panic, no leak).
    #[test]
    fn navigate_wait_until_timeout_returns_timeout_error() {
        let h = build_mock_frame("page-B", "frame-2");

        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        // Responder: ack the navigate but never send the lifecycle event.
        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate was not sent");
            let id = sent["id"].as_i64().expect("id present");
            in_tx
                .send(navigate_success_response(id, "page-B"))
                .expect("send nav response");
            // Deliberately do NOT send Page.eventFired.
        });

        let start = Instant::now();
        let result = h.frame.navigate(
            "https://example.com",
            NavigateOptions {
                wait_until: Some("load".into()),
                ..Default::default()
            },
            Duration::from_millis(200),
        );
        let elapsed = start.elapsed();

        let err = result.expect_err("should time out waiting for lifecycle event");
        assert_eq!(
            err.kind,
            ProtocolErrorKind::Timeout,
            "expected Timeout, got {err:?}"
        );
        assert!(
            err.message.contains("load"),
            "error message should mention the event name: {}",
            err.message
        );
        assert!(
            elapsed >= Duration::from_millis(190),
            "returned too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "returned too late: {elapsed:?}"
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);
    }

    /// G3 TDD case 3a: `--wait-until=networkidle` returns a clear error naming
    /// the supported values BEFORE any network I/O (no Page.navigate is sent).
    #[test]
    fn navigate_wait_until_networkidle_returns_error() {
        let h = build_mock_frame("page-C", "frame-3");

        let result = h.frame.navigate(
            "https://example.com",
            NavigateOptions {
                wait_until: Some("networkidle".into()),
                ..Default::default()
            },
            Duration::from_secs(10),
        );

        let err = result.expect_err("networkidle must error");
        assert_eq!(err.kind, ProtocolErrorKind::Response);
        assert!(
            err.message.contains("load"),
            "error must mention 'load': {}",
            err.message
        );
        assert!(
            err.message.contains("domcontentloaded"),
            "error must mention 'domcontentloaded': {}",
            err.message
        );
        assert!(
            err.message.contains("networkidle"),
            "error must echo the bad value: {}",
            err.message
        );

        // No Page.navigate should have been sent.
        assert!(
            h.out_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "Page.navigate must NOT be sent for unsupported wait_until"
        );

        h.closed.store(true, Ordering::SeqCst);
    }

    /// G3 TDD case 3b: `map_wait_until` validates values correctly.
    #[test]
    fn map_wait_until_validates_values() {
        assert_eq!(MainFrame::map_wait_until("load").unwrap(), "load");
        assert_eq!(
            MainFrame::map_wait_until("domcontentloaded").unwrap(),
            "DOMContentLoaded"
        );
        assert!(MainFrame::map_wait_until("networkidle").is_err());
        assert!(MainFrame::map_wait_until("").is_err());
        assert!(MainFrame::map_wait_until("LOAD").is_err());
    }

    /// G3 TDD case 3c: absent `wait_until` (None) returns after ack — no event
    /// wait, so no timeout from missing event.
    #[test]
    fn navigate_without_wait_until_returns_after_ack() {
        let h = build_mock_frame("page-D", "frame-4");

        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate was not sent");
            let id = sent["id"].as_i64().expect("id present");
            in_tx
                .send(navigate_success_response(id, "page-D"))
                .expect("send nav response");
            // No eventFired emitted — if wait_until is absent, navigate must
            // return without needing one.
        });

        let result = h.frame.navigate(
            "https://example.com",
            NavigateOptions {
                wait_until: None,
                ..Default::default()
            },
            Duration::from_secs(10),
        );
        assert!(
            result.is_ok(),
            "navigate (no wait_until) should succeed: {result:?}"
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);
    }

    /// G3 TDD case 3d: IPC serde round-trip — `DaemonRequest::Navigate` with
    /// `wait_until` field serialises and deserialises correctly.
    #[test]
    fn navigate_ipc_wait_until_serde_round_trip() {
        use crate::cli::ipc::DaemonRequest;

        // With wait_until present.
        let req = DaemonRequest::Navigate {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            url: "https://example.com".into(),
            timeout_secs: 30,
            wait_until: Some("load".into()),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: DaemonRequest = serde_json::from_str(&json).expect("deserialize");
        match back {
            DaemonRequest::Navigate { wait_until, .. } => {
                assert_eq!(wait_until.as_deref(), Some("load"));
            }
            other => panic!("expected Navigate, got {other:?}"),
        }

        // With wait_until absent — must not appear in serialised JSON.
        let req_no_wait = DaemonRequest::Navigate {
            instance_id: "00000001".into(),
            page_id: "p1".into(),
            url: "https://example.com".into(),
            timeout_secs: 30,
            wait_until: None,
        };
        let json_no_wait = serde_json::to_string(&req_no_wait).expect("serialize");
        assert!(
            !json_no_wait.contains("wait_until"),
            "wait_until must be absent from serialised JSON when None: {json_no_wait}"
        );
        let back_no_wait: DaemonRequest = serde_json::from_str(&json_no_wait).expect("deserialize");
        match back_no_wait {
            DaemonRequest::Navigate { wait_until, .. } => {
                assert!(wait_until.is_none(), "wait_until must deserialise to None");
            }
            other => panic!("expected Navigate, got {other:?}"),
        }

        // Legacy wire (no wait_until field at all) must deserialise to None.
        let legacy = r#"{"method":"Navigate","params":{"instance_id":"00000001","page_id":"p1","url":"https://example.com","timeout_secs":30}}"#;
        let back_legacy: DaemonRequest = serde_json::from_str(legacy).expect("deserialize legacy");
        match back_legacy {
            DaemonRequest::Navigate { wait_until, .. } => {
                assert!(
                    wait_until.is_none(),
                    "legacy wire (no wait_until) must deserialise to None"
                );
            }
            other => panic!("expected Navigate, got {other:?}"),
        }
    }

    /// G3 TDD case: `wait_until=domcontentloaded` maps to DOMContentLoaded event.
    #[test]
    fn navigate_wait_until_domcontentloaded_fires_on_dce_event() {
        let h = build_mock_frame("page-E", "frame-5");

        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().unwrap();
            in_tx.send(navigate_success_response(id, "page-E")).unwrap();

            std::thread::sleep(Duration::from_millis(30));

            // Emit DOMContentLoaded (the Juggler name).
            in_tx
                .send(event_fired_message("DOMContentLoaded", "frame-5", "page-E"))
                .unwrap();
        });

        let result = h.frame.navigate(
            "https://example.com",
            NavigateOptions {
                wait_until: Some("domcontentloaded".into()),
                ..Default::default()
            },
            Duration::from_secs(10),
        );
        assert!(
            result.is_ok(),
            "domcontentloaded wait should succeed: {result:?}"
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);
    }

    /// MUST-FIX regression guard: `wait_for_lifecycle` must NOT leak a handler.
    ///
    /// After N successful waits AND N timed-out waits, the handler count for
    /// `(session_key, "Page.eventFired")` must be back to 0 — proving the RAII
    /// guard deregisters on both the success and the timeout exit paths.
    #[test]
    fn wait_for_lifecycle_does_not_leak_handlers() {
        let h = build_mock_frame("page-leak", "frame-leak");
        let in_tx = h.in_tx.clone();

        // Baseline: no handler registered yet.
        assert_eq!(
            h.conn
                .event_handler_count_for("page-leak", "Page.eventFired"),
            0,
            "no Page.eventFired handler should exist before any wait"
        );

        // N successful waits. Each fires the event on a background thread so
        // wait_for_lifecycle returns Ok, then its guard deregisters.
        for i in 0..3 {
            let tx = in_tx.clone();
            let responder = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                tx.send(event_fired_message("load", "frame-leak", "page-leak"))
                    .unwrap();
                let _ = i;
            });
            h.frame
                .wait_for_lifecycle("load", Duration::from_secs(5))
                .expect("wait should succeed");
            responder.join().unwrap();

            // Handler must be gone after each successful wait.
            assert_eq!(
                h.conn
                    .event_handler_count_for("page-leak", "Page.eventFired"),
                0,
                "handler leaked after successful wait #{i}"
            );
        }

        // N timed-out waits. No event is emitted; each must time out and the
        // guard must still deregister.
        for i in 0..3 {
            let err = h
                .frame
                .wait_for_lifecycle("load", Duration::from_millis(50))
                .expect_err("wait should time out");
            assert_eq!(err.kind, ProtocolErrorKind::Timeout);

            assert_eq!(
                h.conn
                    .event_handler_count_for("page-leak", "Page.eventFired"),
                0,
                "handler leaked after timed-out wait #{i}"
            );
        }

        // Final guard: the count never grew.
        assert_eq!(
            h.conn
                .event_handler_count_for("page-leak", "Page.eventFired"),
            0,
            "handler count must be 0 after all waits"
        );

        h.closed.store(true, Ordering::SeqCst);
    }

    /// MUST-FIX regression guard: navigate + wait_until is bounded by a SINGLE
    /// `timeout`, not 2× it.
    ///
    /// The responder acks the navigate after a delay, then never emits the
    /// lifecycle event. With the double-counting bug, total elapsed could reach
    /// ~2× timeout (timeout for the ack wait that succeeds + a fresh full
    /// timeout for the lifecycle wait). With the fix, the lifecycle wait only
    /// gets the time remaining after the ack, so total ≈ timeout.
    #[test]
    fn navigate_wait_until_combined_timeout_is_single_bounded() {
        let h = build_mock_frame("page-bound", "frame-bound");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        // Ack the navigate ~100 ms in; never emit the lifecycle event.
        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            in_tx
                .send(navigate_success_response(id, "page-bound"))
                .unwrap();
            // Deliberately never send Page.eventFired.
        });

        let timeout = Duration::from_millis(400);
        let start = Instant::now();
        let result = h.frame.navigate(
            "https://example.com",
            NavigateOptions {
                wait_until: Some("load".into()),
                ..Default::default()
            },
            timeout,
        );
        let elapsed = start.elapsed();

        let err = result.expect_err("should time out waiting for lifecycle event");
        assert_eq!(err.kind, ProtocolErrorKind::Timeout);

        // The whole operation must be bounded by ~1× timeout (plus slack), not
        // 2× (which would be ~100ms ack + 400ms lifecycle = ~500ms, exceeding
        // a 1.5× ceiling). We allow generous slack for CI scheduling.
        assert!(
            elapsed < timeout + Duration::from_millis(150),
            "navigate+wait exceeded a single timeout bound: {elapsed:?} (timeout {timeout:?}); \
             double-counting likely regressed"
        );
        // And it must not return early before the deadline either.
        assert!(
            elapsed >= Duration::from_millis(380),
            "returned too early: {elapsed:?}"
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // G4: HTTP status capture unit tests
    // -----------------------------------------------------------------------

    /// Helper: build a `Network.requestWillBeSent` event with `cause="document"`.
    fn request_will_be_sent_msg(
        request_id: &str,
        nav_id: Option<&str>,
        frame_id: &str,
        session_key: &str,
    ) -> RawMessage {
        let mut params = serde_json::json!({
            "requestId": request_id,
            "cause": "document",
            "frameId": frame_id,
            "url": "https://example.com/",
            "method": "GET",
            "headers": [],
            "isIntercepted": false,
        });
        if let Some(nid) = nav_id {
            params["navigationId"] = serde_json::json!(nid);
        }
        RawMessage {
            id: None,
            method: Some("Network.requestWillBeSent".to_owned()),
            params: Some(params),
            result: None,
            error: None,
            session_id: Some(session_key.to_owned()),
        }
    }

    /// Helper: build a `Network.requestWillBeSent` event for a redirect-target
    /// request — i.e. one whose `redirectedFrom` points at a prior requestId.
    /// Redirect continuations carry document cause and the same navigationId.
    fn redirect_request_msg(
        request_id: &str,
        redirected_from: &str,
        nav_id: Option<&str>,
        frame_id: &str,
        session_key: &str,
    ) -> RawMessage {
        let mut params = serde_json::json!({
            "requestId": request_id,
            "redirectedFrom": redirected_from,
            "cause": "TYPE_DOCUMENT",
            "frameId": frame_id,
            "url": "https://example.com/redirected",
            "method": "GET",
            "headers": [],
            "isIntercepted": false,
        });
        if let Some(nid) = nav_id {
            params["navigationId"] = serde_json::json!(nid);
        }
        RawMessage {
            id: None,
            method: Some("Network.requestWillBeSent".to_owned()),
            params: Some(params),
            result: None,
            error: None,
            session_id: Some(session_key.to_owned()),
        }
    }

    /// Helper: build a `Network.responseReceived` event.
    fn response_received_msg(request_id: &str, status: u16, session_key: &str) -> RawMessage {
        RawMessage {
            id: None,
            method: Some("Network.responseReceived".to_owned()),
            params: Some(serde_json::json!({
                "requestId": request_id,
                "status": status,
                "statusText": "OK",
                "headers": [],
                "fromServiceWorker": false,
            })),
            result: None,
            error: None,
            session_id: Some(session_key.to_owned()),
        }
    }

    /// G4 TDD case 1: `Network.requestWillBeSent{cause:"document",
    /// navigationId:"nav-1"}` then `Network.responseReceived{status:404}`
    /// around `Page.navigate` returning `navigationId:"nav-1"`.
    ///
    /// - `navigate` returns `Ok` (not an error for 4xx).
    /// - `outcome.status_code == Some(404)`.
    /// - `outcome.nav_id == Some("nav-1")`.
    #[test]
    fn navigate_captures_404_status_and_returns_ok() {
        let h = build_mock_frame("page-g4a", "frame-g4a");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        // Responder: ack Page.navigate, inject Network events.
        let responder = std::thread::spawn(move || {
            // Wait for Page.navigate outgoing request.
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let nav_req_id = sent["id"].as_i64().expect("id");
            assert_eq!(sent["method"], "Page.navigate");

            // Inject Network.requestWillBeSent BEFORE the navigate ack to
            // test the speculative path.
            in_tx
                .send(request_will_be_sent_msg(
                    "req-001",
                    Some("nav-1"),
                    "frame-g4a",
                    "page-g4a",
                ))
                .expect("send rws");

            // Now ack the navigate.
            in_tx
                .send(navigate_success_response(nav_req_id, "page-g4a"))
                .expect("send nav response");

            // Inject the response event.
            std::thread::sleep(Duration::from_millis(10));
            in_tx
                .send(response_received_msg("req-001", 404, "page-g4a"))
                .expect("send responseReceived");
        });

        let result = h.frame.navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(10),
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);

        let outcome = result.expect("navigate must return Ok even for 404");
        assert_eq!(
            outcome.nav_id.as_deref(),
            Some("nav-1"),
            "nav_id must be preserved"
        );
        assert_eq!(
            outcome.status_code,
            Some(404),
            "status_code must be Some(404)"
        );
    }

    /// G4 TDD case 2: no `responseReceived` arrives ⇒ `status_code == None`,
    /// navigate still `Ok`. Also verifies that neither `Network.requestWillBeSent`
    /// nor `Network.responseReceived` handler leaks.
    #[test]
    fn navigate_status_code_is_none_when_no_response_event() {
        let h = build_mock_frame("page-g4b", "frame-g4b");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().expect("id");
            // Ack navigate but do NOT inject any Network events.
            in_tx
                .send(navigate_success_response(id, "page-g4b"))
                .expect("send nav response");
            // Deliberately send NO Network.responseReceived.
        });

        // Use a short timeout so the status-poll drain exits quickly.
        let result = h.frame.navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(10),
        );

        responder.join().unwrap();

        let outcome = result.expect("navigate must succeed even with no Network events");
        assert_eq!(
            outcome.status_code, None,
            "status_code must be None when no responseReceived event arrives"
        );

        // Verify handlers were deregistered (no leak).
        assert_eq!(
            h.conn
                .event_handler_count_for("page-g4b", "Network.requestWillBeSent"),
            0,
            "Network.requestWillBeSent handler must not leak"
        );
        assert_eq!(
            h.conn
                .event_handler_count_for("page-g4b", "Network.responseReceived"),
            0,
            "Network.responseReceived handler must not leak"
        );

        h.closed.store(true, Ordering::SeqCst);
    }

    /// G4 TDD case 3: navigate response IPC serde includes `status_code`.
    /// Absence is backward-compatible (legacy callers ignore unknown fields).
    #[test]
    fn navigate_response_serde_includes_status_code() {
        use crate::cli::ipc::DaemonResponse;

        // Response WITH status_code.
        let resp = DaemonResponse::ok(serde_json::json!({
            "navigation_id": "nav-1",
            "status_code": 200_u16,
        }));
        let serialized = serde_json::to_string(&resp).expect("serialize");
        assert!(
            serialized.contains("status_code"),
            "status_code must appear in JSON: {serialized}"
        );
        let back: DaemonResponse = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(
            back.data
                .as_ref()
                .and_then(|d| d.get("status_code"))
                .and_then(|v| v.as_u64()),
            Some(200),
            "status_code round-trips"
        );
        assert_eq!(
            back.data
                .as_ref()
                .and_then(|d| d.get("navigation_id"))
                .and_then(|v| v.as_str()),
            Some("nav-1"),
            "navigation_id still present"
        );

        // Response WITHOUT status_code (legacy / null) must deserialise fine.
        let legacy = r#"{"ok":true,"data":{"navigation_id":"nav-2"}}"#;
        let legacy_back: DaemonResponse =
            serde_json::from_str(legacy).expect("deserialize legacy navigate response");
        assert!(legacy_back.ok);
        assert!(
            legacy_back
                .data
                .as_ref()
                .and_then(|d| d.get("status_code"))
                .is_none(),
            "legacy callers without status_code must deserialise fine (field absent is ok)"
        );
    }

    /// G4 TDD case 4: `print_response` in human mode prints the status code
    /// when present.
    #[test]
    fn print_response_shows_status_code() {
        use crate::cli::ipc::DaemonResponse;
        use crate::cli::output::print_response;

        // This test cannot easily capture stdout, but it exercises the code
        // path and asserts non-panic. The formatted string is checked by
        // inspecting the serialised JSON.
        let resp_with_status = DaemonResponse::ok(serde_json::json!({
            "navigation_id": "nav-1",
            "status_code": 404_u16,
        }));
        let json_out = serde_json::to_string_pretty(&resp_with_status).expect("serialize");
        assert!(
            json_out.contains("404"),
            "status_code 404 must appear in JSON output: {json_out}"
        );
        // print_response must not panic.
        print_response(&resp_with_status, false);
        print_response(&resp_with_status, true);

        // Response with null status_code must also not panic.
        let resp_null_status = DaemonResponse::ok(serde_json::json!({
            "navigation_id": null,
            "status_code": null,
        }));
        print_response(&resp_null_status, false);
    }

    /// G4 TDD case 5: `response_received_msg` with 200 status is captured.
    #[test]
    fn navigate_captures_200_status() {
        let h = build_mock_frame("page-g4c", "frame-g4c");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().expect("id");

            in_tx
                .send(navigate_success_response(id, "page-g4c"))
                .expect("send nav response");

            // Inject Network events after ack (common timing).
            in_tx
                .send(request_will_be_sent_msg(
                    "req-200",
                    Some("nav-1"),
                    "frame-g4c",
                    "page-g4c",
                ))
                .expect("send rws");
            in_tx
                .send(response_received_msg("req-200", 200, "page-g4c"))
                .expect("send responseReceived");
        });

        let result = h.frame.navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(10),
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);

        let outcome = result.expect("navigate must succeed");
        assert_eq!(
            outcome.status_code,
            Some(200),
            "status_code must be Some(200)"
        );
    }

    /// G4 redirect TDD: a 4-event redirect sequence must report the FINAL
    /// response status, not the redirect hop's.
    ///
    /// Sequence (mirrors a real cross-host redirect `old.example → new.example`, 301 → 200):
    ///   1. requestWillBeSent A (document, navId)        → locks main = A
    ///   2. responseReceived  A (status 301)             → status = 301
    ///   3. requestWillBeSent B (redirectedFrom = A)     → chain forward main = B
    ///   4. responseReceived  B (status 200)             → status overwritten = 200
    ///
    /// Asserts `status_code == Some(200)` (the final hop), NOT `Some(301)`.
    #[test]
    fn navigate_follows_redirect_chain_reports_final_status() {
        let h = build_mock_frame("page-g4r", "frame-g4r");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().expect("id");

            // Ack the navigate (nav id "nav-1", per navigate_success_response).
            in_tx
                .send(navigate_success_response(id, "page-g4r"))
                .expect("send nav response");

            // 1. Initial document request A.
            in_tx
                .send(request_will_be_sent_msg(
                    "req-A",
                    Some("nav-1"),
                    "frame-g4r",
                    "page-g4r",
                ))
                .expect("send rws A");
            // 2. Redirect response for A (301).
            in_tx
                .send(response_received_msg("req-A", 301, "page-g4r"))
                .expect("send respReceived A");
            // 3. Redirect-target request B (redirectedFrom = A).
            in_tx
                .send(redirect_request_msg(
                    "req-B",
                    "req-A",
                    Some("nav-1"),
                    "frame-g4r",
                    "page-g4r",
                ))
                .expect("send rws B");
            // 4. Final response for B (200).
            in_tx
                .send(response_received_msg("req-B", 200, "page-g4r"))
                .expect("send respReceived B");
        });

        let result = h.frame.navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(10),
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);

        let outcome = result.expect("navigate must succeed on redirect");
        assert_eq!(
            outcome.status_code,
            Some(200),
            "redirect chain must report the FINAL status (200), not the redirect hop (301)"
        );
    }

    /// G4 redirect TDD: a sub-resource (e.g. an image whose redirectedFrom does
    /// NOT match the tracked main request) must NOT hijack the chain. The
    /// main-document final status must still be reported.
    #[test]
    fn navigate_subresource_redirect_does_not_hijack_chain() {
        let h = build_mock_frame("page-g4h", "frame-g4h");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().expect("id");
            in_tx
                .send(navigate_success_response(id, "page-g4h"))
                .expect("send nav response");

            // Main document request A → 200 final.
            in_tx
                .send(request_will_be_sent_msg(
                    "req-A",
                    Some("nav-1"),
                    "frame-g4h",
                    "page-g4h",
                ))
                .expect("send rws A");
            in_tx
                .send(response_received_msg("req-A", 200, "page-g4h"))
                .expect("send respReceived A");

            // A sub-resource redirect whose redirectedFrom is some OTHER request
            // ("img-1", which we never tracked). It must not advance the chain
            // nor overwrite the main-document status.
            in_tx
                .send(redirect_request_msg(
                    "req-img-2",
                    "img-1",
                    Some("nav-1"),
                    "frame-g4h",
                    "page-g4h",
                ))
                .expect("send rws sub-resource");
            in_tx
                .send(response_received_msg("req-img-2", 500, "page-g4h"))
                .expect("send respReceived sub-resource");
        });

        let result = h.frame.navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(10),
        );

        responder.join().unwrap();
        h.closed.store(true, Ordering::SeqCst);

        let outcome = result.expect("navigate must succeed");
        assert_eq!(
            outcome.status_code,
            Some(200),
            "sub-resource redirect must not hijack the main-document status (expected 200)"
        );
    }

    /// G4 redirect leak guard: after a redirect navigate, BOTH Network handlers
    /// must be deregistered (the added chain logic must not leak).
    #[test]
    fn navigate_redirect_does_not_leak_handlers() {
        let h = build_mock_frame("page-g4l", "frame-g4l");
        let in_tx = h.in_tx.clone();
        let out_rx = h.out_rx;

        let responder = std::thread::spawn(move || {
            let sent = out_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Page.navigate not sent");
            let id = sent["id"].as_i64().expect("id");
            in_tx
                .send(navigate_success_response(id, "page-g4l"))
                .expect("send nav response");
            in_tx
                .send(request_will_be_sent_msg(
                    "req-A",
                    Some("nav-1"),
                    "frame-g4l",
                    "page-g4l",
                ))
                .expect("send rws A");
            in_tx
                .send(response_received_msg("req-A", 301, "page-g4l"))
                .expect("send respReceived A");
            in_tx
                .send(redirect_request_msg(
                    "req-B",
                    "req-A",
                    Some("nav-1"),
                    "frame-g4l",
                    "page-g4l",
                ))
                .expect("send rws B");
            in_tx
                .send(response_received_msg("req-B", 200, "page-g4l"))
                .expect("send respReceived B");
        });

        let _ = h.frame.navigate(
            "https://example.com",
            Default::default(),
            Duration::from_secs(10),
        );

        responder.join().unwrap();

        assert_eq!(
            h.conn
                .event_handler_count_for("page-g4l", "Network.requestWillBeSent"),
            0,
            "Network.requestWillBeSent handler must not leak after a redirect navigate"
        );
        assert_eq!(
            h.conn
                .event_handler_count_for("page-g4l", "Network.responseReceived"),
            0,
            "Network.responseReceived handler must not leak after a redirect navigate"
        );

        h.closed.store(true, Ordering::SeqCst);
    }
}
