//! Instance management for the daemon.
//!
//! Each `Instance` holds a running Camoufox browser with its connection,
//! context, and pages. The `InstanceManager` provides the high-level
//! operations that CLI commands map to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::time::Duration;

use serde_json::json;

use crate::api::main_frame::{
    KeyEventParams, MouseEventParams, NavigateOptions, NavigateOutcome, Rect, ScreenshotOptions,
};
use crate::api::{Browser, BrowserOptions, ContextOptions, CookieOptions, MainFrame};
use crate::config::LaunchConfig;
use crate::protocol::client::Connection;
use crate::transport::pipe::PipeTransport;

fn default_executable() -> String {
    std::env::var("CAMOUFOX_BIN").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        format!("{home}/.cache/camoufox/camoufox")
    })
}

// ---------------------------------------------------------------------------
// JavaScript helpers
// ---------------------------------------------------------------------------

/// Render a Rust string as a JavaScript string literal (quotes included).
///
/// Uses JSON encoding — a strict subset of JavaScript string syntax — then
/// escapes U+2028/U+2029, which are legal in JSON strings but terminate a
/// line in (pre-ES2019) JavaScript source.
fn js_literal(s: &str) -> String {
    serde_json::Value::String(s.to_owned())
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Map a key name to the `(code, key, key_code)` triple Juggler expects.
///
/// Accepts the common named keys plus any single printable character.
///
/// # Why no `text`
///
/// Juggler's `Page.dispatchKeyEvent` routes a keydown through
/// `commitCompositionWith(text, …)` whenever `text` is present *and differs
/// from* `key` — i.e. it treats it as an IME composition commit, and the page
/// then observes `event.key === "Process"` instead of the real key. Sending
/// `Enter` with `text: "\r"` therefore never submits a form. Omitting `text`
/// takes the plain `keydown` path and lets Gecko derive the character itself,
/// which is correct for both named keys and printable characters.
fn key_descriptor(name: &str) -> Result<(String, String, u32), String> {
    let named: Option<(&str, &str, u32)> = match name {
        "Enter" | "Return" => Some(("Enter", "Enter", 13)),
        "Tab" => Some(("Tab", "Tab", 9)),
        "Escape" | "Esc" => Some(("Escape", "Escape", 27)),
        "Backspace" => Some(("Backspace", "Backspace", 8)),
        "Delete" => Some(("Delete", "Delete", 46)),
        "ArrowUp" => Some(("ArrowUp", "ArrowUp", 38)),
        "ArrowDown" => Some(("ArrowDown", "ArrowDown", 40)),
        "ArrowLeft" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "ArrowRight" => Some(("ArrowRight", "ArrowRight", 39)),
        "Home" => Some(("Home", "Home", 36)),
        "End" => Some(("End", "End", 35)),
        "PageUp" => Some(("PageUp", "PageUp", 33)),
        "PageDown" => Some(("PageDown", "PageDown", 34)),
        "Space" => Some(("Space", " ", 32)),
        _ => None,
    };

    if let Some((code, key, key_code)) = named {
        return Ok((code.to_string(), key.to_string(), key_code));
    }

    // Single printable character.
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => {
            let upper = c.to_ascii_uppercase();
            if c.is_ascii_alphabetic() {
                Ok((format!("Key{upper}"), c.to_string(), upper as u32))
            } else if c.is_ascii_digit() {
                // Digit keyCodes are the ASCII codepoint (48–57).
                Ok((format!("Digit{c}"), c.to_string(), c as u32))
            } else if let Some((code, key_code)) = punctuation_descriptor(c) {
                Ok((code.to_string(), c.to_string(), key_code))
            } else {
                // Unmapped character (e.g. non-US-layout input): no standard
                // `code` name; fall back to the uppercase codepoint for the
                // legacy `keyCode`. Best effort — pages reading `event.key`
                // still see the correct character.
                Ok((String::new(), c.to_string(), upper as u32))
            }
        }
        _ => Err(format!(
            "unknown key {name:?}; use a single character or one of: \
             Enter, Tab, Escape, Backspace, Delete, ArrowUp, ArrowDown, \
             ArrowLeft, ArrowRight, Home, End, PageUp, PageDown, Space"
        )),
    }
}

/// Map a US-layout punctuation character to its `(code, keyCode)`.
///
/// The legacy `keyCode` for punctuation is NOT the character's codepoint —
/// e.g. `.` is codepoint 46, which is *Delete's* keyCode, so a page reading
/// `event.keyCode` would misread `press .` as Delete. This returns the real
/// US-layout `keyCode` and the physical `code`. Shifted variants (`:` `<`
/// `?` …) share the unshifted key's code. Returns `None` for anything not in
/// the standard set, so the caller can fall back to codepoint best-effort.
fn punctuation_descriptor(c: char) -> Option<(&'static str, u32)> {
    let desc = match c {
        ';' | ':' => ("Semicolon", 186),
        '=' | '+' => ("Equal", 187),
        ',' | '<' => ("Comma", 188),
        '-' | '_' => ("Minus", 189),
        '.' | '>' => ("Period", 190),
        '/' | '?' => ("Slash", 191),
        '`' | '~' => ("Backquote", 192),
        '[' | '{' => ("BracketLeft", 219),
        '\\' | '|' => ("Backslash", 220),
        ']' | '}' => ("BracketRight", 221),
        '\'' | '"' => ("Quote", 222),
        ' ' => ("Space", 32),
        _ => return None,
    };
    Some(desc)
}

// ---------------------------------------------------------------------------
// ManagedMainFrame
// ---------------------------------------------------------------------------

/// A `MainFrame` plus its CLI-facing page label tracking. The label lives
/// in the parent `HashMap` key; this struct just owns the frame.
pub struct ManagedMainFrame {
    pub main_frame: MainFrame,
    /// Extra HTTP headers set on this page so far. `Network.setExtraHTTPHeaders`
    /// replaces the whole set on every call, so the CLI keeps the accumulated
    /// list here and re-sends all of them.
    pub headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Instance
// ---------------------------------------------------------------------------

/// A running Camoufox browser instance managed by the daemon.
pub struct Instance {
    pub browser: Browser,
    pub child: Child,
    pub version: Option<String>,
    pub pid: u32,
    _profile_dir: tempfile::TempDir,
    pages: HashMap<String, ManagedMainFrame>,
    page_counter: u32,
}

impl Instance {
    /// Create a new page in this instance's context, fully wired with
    /// session, top frame id, and execution context tracking.
    pub fn create_page(&mut self, context: &crate::api::BrowserContext) -> Result<String, String> {
        let main_frame = context
            .new_main_frame()
            .map_err(|e| format!("failed to create page: {e}"))?;

        self.page_counter += 1;
        let page_id = format!("p{}", self.page_counter);
        self.pages.insert(
            page_id.clone(),
            ManagedMainFrame {
                main_frame,
                headers: Vec::new(),
            },
        );
        Ok(page_id)
    }

    /// Navigate a page to a URL.
    ///
    /// `timeout` is forwarded to the protocol layer; if the renderer fails
    /// to respond within `timeout` (e.g. the response was a download), the
    /// call returns a `navigate failed` error containing a `Timeout` kind
    /// rather than hanging the daemon.
    ///
    /// If `wait_until` is `Some("load")` or `Some("domcontentloaded")`, blocks
    /// until the matching `Page.eventFired` lifecycle event fires (bounded by
    /// `timeout`). If absent, returns immediately after the navigate ack.
    ///
    /// Clears the cached execution context so the next `evaluate` waits for
    /// the post-navigation context; the wait happens inside `MainFrame::evaluate`.
    ///
    /// Returns a `NavigateOutcome` containing `nav_id` and `status_code` (G4).
    pub fn navigate(
        &self,
        page_id: &str,
        url: &str,
        timeout: Duration,
        wait_until: Option<&str>,
    ) -> Result<NavigateOutcome, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        // Force `evaluate` to wait for a fresh post-navigation context.
        *mp.main_frame.execution_context_handle().lock().unwrap() = None;

        let options = NavigateOptions {
            wait_until: wait_until.map(|s| s.to_owned()),
            ..Default::default()
        };

        mp.main_frame
            .navigate(url, options, timeout)
            .map_err(|e| format!("navigate failed: {e}"))
    }

    /// Evaluate JavaScript on a page.
    pub fn evaluate(
        &self,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        let result = mp
            .main_frame
            .evaluate(expression, timeout)
            .map_err(|e| format!("evaluate failed: {e}"))?;

        // Unwrap `{result: {value: …}}` to just the value, matching today's
        // CLI output shape.
        let value = result
            .get("result")
            .and_then(|r| r.get("value"))
            .or_else(|| result.get("value"))
            .cloned()
            .unwrap_or(result);
        Ok(value)
    }

    // -----------------------------------------------------------------------
    // Shared evaluate helpers
    // -----------------------------------------------------------------------

    /// Evaluate `expression` and return the plain JS value, turning a thrown
    /// page-side exception into an `Err` instead of leaking the raw protocol
    /// envelope to the caller.
    ///
    /// `undefined` (no `result.value`) comes back as `Value::Null`.
    fn eval_checked(
        &self,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        let raw = mp
            .main_frame
            .evaluate(expression, timeout)
            .map_err(|e| format!("evaluate failed: {e}"))?;

        if let Some(exc) = raw.get("exceptionDetails") {
            let msg = exc
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
                .unwrap_or_else(|| exc.to_string());
            return Err(format!("page script error: {msg}"));
        }

        Ok(raw
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    /// Resolve a CSS selector to its viewport-space bounding box, scrolling it
    /// into view first. Also returns the page's scroll offsets so callers can
    /// convert to document coordinates.
    fn element_rect(
        &self,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<(Rect, f64, f64), String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
             try {{ el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}}); }} \
             catch (e) {{ el.scrollIntoView(); }} \
             const r = el.getBoundingClientRect(); \
             return [r.x, r.y, r.width, r.height, window.scrollX, window.scrollY]; }})()",
            sel = js_literal(selector)
        );
        let value = self.eval_checked(page_id, &expr, timeout)?;
        let arr = value
            .as_array()
            .ok_or_else(|| format!("selector not found: {selector}"))?;
        let n = |i: usize| arr.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rect = Rect {
            x: n(0),
            y: n(1),
            width: n(2),
            height: n(3),
        };
        if rect.width <= 0.0 && rect.height <= 0.0 {
            return Err(format!(
                "element {selector} has a zero-size box (hidden or not laid out)"
            ));
        }
        Ok((rect, n(4), n(5)))
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    /// Extract the page's rendered text (`innerText`), optionally scoped to a
    /// CSS selector.
    ///
    /// `innerText` — not `textContent` — because it is what a reader actually
    /// sees: inline `<script>`/`<style>` source is excluded and block layout
    /// becomes line breaks. Note that per spec `innerText` already falls back
    /// to `textContent` for an element that is not being rendered, so a
    /// `display: none` subtree still yields its text. An element that *is*
    /// rendered but whose only content is unrendered (e.g. a lone `<noscript>`)
    /// correctly yields `""`.
    pub fn text(
        &self,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<String, String> {
        let expr = match selector {
            Some(sel) => format!(
                "(() => {{ const el = document.querySelector({sel}); \
                 return el ? el.innerText : null; }})()",
                sel = js_literal(sel)
            ),
            None => "(() => (document.body || document.documentElement).innerText)()".to_string(),
        };
        match self.eval_checked(page_id, &expr, timeout)? {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Null => Err(match selector {
                Some(sel) => format!("selector not found: {sel}"),
                None => "page has no text content".to_string(),
            }),
            other => Ok(other.to_string()),
        }
    }

    /// Extract page HTML: `outerHTML` of the selector match, or the whole
    /// document when no selector is given.
    pub fn html(
        &self,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<String, String> {
        let expr = match selector {
            Some(sel) => format!(
                "(() => {{ const el = document.querySelector({sel}); \
                 return el ? el.outerHTML : null; }})()",
                sel = js_literal(sel)
            ),
            None => "document.documentElement.outerHTML".to_string(),
        };
        match self.eval_checked(page_id, &expr, timeout)? {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Null => Err(match selector {
                Some(sel) => format!("selector not found: {sel}"),
                None => "page has no document element".to_string(),
            }),
            other => Ok(other.to_string()),
        }
    }

    /// Collect every `<a href>` on the page (or inside `selector`) as
    /// `{text, href}` objects. Whitespace in the link text is collapsed.
    pub fn links(
        &self,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let root = match selector {
            Some(sel) => format!("document.querySelector({})", js_literal(sel)),
            None => "document".to_string(),
        };
        let expr = format!(
            "(() => {{ const root = {root}; if (!root) return null; \
             return Array.from(root.querySelectorAll('a[href]')).map(a => ({{ \
             text: (a.innerText || a.textContent || '').trim().replace(/\\s+/g, ' '), \
             href: a.href }})); }})()"
        );
        match self.eval_checked(page_id, &expr, timeout)? {
            serde_json::Value::Null => Err(match selector {
                Some(sel) => format!("selector not found: {sel}"),
                None => "no links found".to_string(),
            }),
            other => Ok(other),
        }
    }

    /// Extract structured page metadata. Any combination of Open Graph tags,
    /// `application/ld+json` blocks and named `<meta>` tags.
    pub fn data(
        &self,
        page_id: &str,
        og: bool,
        jsonld: bool,
        meta: bool,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        // No flags => everything.
        let (og, jsonld, meta) = if !og && !jsonld && !meta {
            (true, true, true)
        } else {
            (og, jsonld, meta)
        };

        let expr = "(() => { const out = { title: document.title, url: location.href }; \
             out.og = {}; \
             document.querySelectorAll('meta[property], meta[name]').forEach(m => { \
               const k = m.getAttribute('property') || m.getAttribute('name') || ''; \
               if (/^(og|twitter|article|fb|al):/i.test(k)) out.og[k] = m.getAttribute('content'); \
             }); \
             out.meta = {}; \
             document.querySelectorAll('meta[name][content]').forEach(m => { \
               out.meta[m.getAttribute('name')] = m.getAttribute('content'); \
             }); \
             out.jsonld = []; \
             document.querySelectorAll('script[type=\"application/ld+json\"]').forEach(s => { \
               try { out.jsonld.push(JSON.parse(s.textContent)); } \
               catch (e) { out.jsonld.push({ _parseError: String(e), _raw: s.textContent }); } \
             }); \
             return out; })()";

        let mut value = self.eval_checked(page_id, expr, timeout)?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| "unexpected metadata shape".to_string())?;
        if !og {
            obj.remove("og");
        }
        if !jsonld {
            obj.remove("jsonld");
        }
        if !meta {
            obj.remove("meta");
        }
        Ok(value)
    }

    // -----------------------------------------------------------------------
    // Navigation / waiting
    // -----------------------------------------------------------------------

    /// Read the page's current URL and title.
    pub fn url(&self, page_id: &str, timeout: Duration) -> Result<(String, String), String> {
        let value = self.eval_checked(page_id, "[location.href, document.title || '']", timeout)?;
        let url = value
            .get(0)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = value
            .get(1)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok((url, title))
    }

    /// Go back one session-history entry. Returns `false` if there was none.
    pub fn go_back(&self, page_id: &str) -> Result<bool, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        mp.main_frame
            .go_back()
            .map_err(|e| format!("go back failed: {e}"))
    }

    /// Go forward one session-history entry. Returns `false` if there was none.
    pub fn go_forward(&self, page_id: &str) -> Result<bool, String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        mp.main_frame
            .go_forward()
            .map_err(|e| format!("go forward failed: {e}"))
    }

    /// Reload the page.
    pub fn reload(&self, page_id: &str) -> Result<(), String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        mp.main_frame
            .reload()
            .map_err(|e| format!("reload failed: {e}"))
    }

    /// Poll until `selector` matches an element, or `timeout` elapses.
    ///
    /// Returns the elapsed milliseconds on success.
    pub fn wait_for_selector(
        &self,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<u128, String> {
        let expr = format!("!!document.querySelector({})", js_literal(selector));
        let started = std::time::Instant::now();
        let deadline = started + timeout;
        // Each poll gets the remaining budget so an unresponsive page can't
        // stretch the overall wait past `timeout` by more than one poll.
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let poll_budget = remaining.max(Duration::from_secs(1));
            match self.eval_checked(page_id, &expr, poll_budget) {
                Ok(serde_json::Value::Bool(true)) => return Ok(started.elapsed().as_millis()),
                Ok(_) => {}
                // A page-script error means `querySelector` itself threw — an
                // invalid CSS selector. That is permanent: polling will never
                // make it valid, so fail fast with the real error instead of
                // burning the whole timeout (and holding the daemon lock) only
                // to report a generic "timed out".
                Err(e) if e.starts_with("page script error:") => {
                    return Err(format!("invalid selector {selector:?}: {e}"));
                }
                // Any other error (an in-flight navigation tears down the
                // execution context) is transient; keep polling until the
                // deadline rather than failing immediately.
                Err(e) if std::time::Instant::now() >= deadline => return Err(e),
                Err(_) => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out after {}s waiting for selector: {selector}",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    // -----------------------------------------------------------------------
    // Headers
    // -----------------------------------------------------------------------

    /// Add (or replace) an extra HTTP request header for this page.
    ///
    /// Returns the full accumulated header set.
    pub fn set_header(
        &mut self,
        page_id: &str,
        name: &str,
        value: &str,
    ) -> Result<Vec<(String, String)>, String> {
        let mp = self
            .pages
            .get_mut(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        mp.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        mp.headers.push((name.to_string(), value.to_string()));

        let borrowed: Vec<(&str, &str)> = mp
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        mp.main_frame
            .set_extra_http_headers(&borrowed)
            .map_err(|e| format!("set header failed: {e}"))?;

        Ok(mp.headers.clone())
    }

    // -----------------------------------------------------------------------
    // Interaction
    // -----------------------------------------------------------------------

    /// Click the element matching `selector`: scroll it into view, then
    /// dispatch a trusted click at the centre of its box.
    ///
    /// Returns the viewport coordinates that were clicked.
    pub fn click_selector(
        &self,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        let (rect, _, _) = self.element_rect(page_id, selector, timeout)?;
        let x = (rect.x + rect.width / 2.0).round().max(0.0) as i32;
        let y = (rect.y + rect.height / 2.0).round().max(0.0) as i32;
        self.click(page_id, x, y)?;
        Ok((x, y))
    }

    /// Move the mouse over the element matching `selector` without clicking.
    pub fn hover(
        &self,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        let (rect, _, _) = self.element_rect(page_id, selector, timeout)?;
        let x = (rect.x + rect.width / 2.0).round().max(0.0) as i32;
        let y = (rect.y + rect.height / 2.0).round().max(0.0) as i32;

        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        mp.main_frame
            .dispatch_mouse_event(MouseEventParams {
                r#type: "mousemove".to_string(),
                button: 0,
                buttons: 0,
                x,
                y,
                modifiers: 0,
                click_count: None,
            })
            .map_err(|e| format!("hover failed: {e}"))?;
        Ok((x, y))
    }

    /// Insert text into whichever element currently has focus.
    pub fn insert_text(&self, page_id: &str, text: &str) -> Result<(), String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        mp.main_frame
            .insert_text(text)
            .map_err(|e| format!("type failed: {e}"))
    }

    /// Focus the element matching `selector`, clear it, then type `value`.
    ///
    /// Returns the tag name of the element that was filled.
    pub fn fill(
        &self,
        page_id: &str,
        selector: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
             try {{ el.scrollIntoView({{block:'center', behavior:'instant'}}); }} \
             catch (e) {{ el.scrollIntoView(); }} \
             el.focus(); \
             if ('value' in el) {{ el.value = ''; \
               el.dispatchEvent(new Event('input', {{bubbles: true}})); }} \
             else if (el.isContentEditable) {{ el.textContent = ''; }} \
             return el.tagName.toLowerCase(); }})()",
            sel = js_literal(selector)
        );
        let tag = match self.eval_checked(page_id, &expr, timeout)? {
            serde_json::Value::String(s) => s,
            _ => return Err(format!("selector not found: {selector}")),
        };
        self.insert_text(page_id, value)?;
        Ok(tag)
    }

    /// Press a named key (keydown followed by keyup).
    pub fn press(&self, page_id: &str, key: &str) -> Result<(), String> {
        let (code, key_name, key_code) = key_descriptor(key)?;
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        // `text` is deliberately omitted — see `key_descriptor`.
        let make = |kind: &str| KeyEventParams {
            r#type: kind.to_string(),
            key_code,
            code: code.clone(),
            key: key_name.clone(),
            repeat: false,
            location: 0,
            text: None,
        };

        mp.main_frame
            .dispatch_key_event(make("keydown"))
            .map_err(|e| format!("press (keydown) failed: {e}"))?;
        mp.main_frame
            .dispatch_key_event(make("keyup"))
            .map_err(|e| format!("press (keyup) failed: {e}"))?;
        Ok(())
    }

    /// Choose an option in a `<select>` by value, label, or visible text.
    pub fn select_option(
        &self,
        page_id: &str,
        selector: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); \
             if (!el) return {{ok: false, reason: 'selector not found'}}; \
             if (el.tagName.toLowerCase() !== 'select') \
               return {{ok: false, reason: 'element is a <' + el.tagName.toLowerCase() + '>, not a <select>'}}; \
             const want = {val}; const opts = Array.from(el.options); \
             const m = opts.find(o => o.value === want) || opts.find(o => o.label === want) \
                    || opts.find(o => (o.text || '').trim() === want); \
             if (!m) return {{ok: false, reason: 'no option matching ' + JSON.stringify(want), \
               options: opts.map(o => o.value)}}; \
             m.selected = true; el.value = m.value; \
             el.dispatchEvent(new Event('input', {{bubbles: true}})); \
             el.dispatchEvent(new Event('change', {{bubbles: true}})); \
             return {{ok: true, value: el.value, text: (m.text || '').trim()}}; }})()",
            sel = js_literal(selector),
            val = js_literal(value)
        );
        let result = self.eval_checked(page_id, &expr, timeout)?;
        if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let reason = result
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("select failed");
            return Err(format!("select {selector}: {reason}"));
        }
        Ok(result)
    }

    /// Scroll an element into view, or scroll to the bottom of the page when
    /// no selector is given.
    pub fn scroll(
        &self,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let expr = match selector {
            Some(sel) => format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
                 try {{ el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}}); }} \
                 catch (e) {{ el.scrollIntoView(); }} \
                 return {{scrollX: window.scrollX, scrollY: window.scrollY, \
                   scrollHeight: document.documentElement.scrollHeight}}; }})()",
                sel = js_literal(sel)
            ),
            None => "(() => { window.scrollTo(0, document.documentElement.scrollHeight); \
                 return {scrollX: window.scrollX, scrollY: window.scrollY, \
                   scrollHeight: document.documentElement.scrollHeight}; })()"
                .to_string(),
        };
        match self.eval_checked(page_id, &expr, timeout)? {
            serde_json::Value::Null => Err(format!(
                "selector not found: {}",
                selector.unwrap_or_default()
            )),
            other => Ok(other),
        }
    }

    // -----------------------------------------------------------------------
    // Tabs
    // -----------------------------------------------------------------------

    /// List this instance's pages with their URL and title. A page that cannot
    /// be evaluated (still loading, crashed) reports `null` for both.
    pub fn tabs(&self, timeout: Duration) -> Vec<serde_json::Value> {
        self.page_ids()
            .into_iter()
            .map(|page_id| match self.url(&page_id, timeout) {
                Ok((url, title)) => json!({"page_id": page_id, "url": url, "title": title}),
                Err(_) => json!({"page_id": page_id, "url": null, "title": null}),
            })
            .collect()
    }

    /// Close a page and forget it.
    pub fn close_tab(&mut self, page_id: &str) -> Result<(), String> {
        let mp = self
            .pages
            .remove(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        mp.main_frame
            .close()
            .map_err(|e| format!("close tab failed: {e}"))
    }

    /// Dispatch a trusted left-click (mousemove → mousedown → mouseup) at
    /// viewport coordinates (x, y). Trusted browser-level input, so it drives
    /// cross-origin / closed-shadow widgets (e.g. Cloudflare Turnstile) that
    /// synthetic JS events cannot reach.
    pub fn click(&self, page_id: &str, x: i32, y: i32) -> Result<(), String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        let mv = MouseEventParams {
            r#type: "mousemove".to_string(),
            button: 0,
            buttons: 0,
            x,
            y,
            modifiers: 0,
            click_count: None,
        };
        mp.main_frame
            .dispatch_mouse_event(mv)
            .map_err(|e| format!("click (move) failed: {e}"))?;

        let down = MouseEventParams {
            r#type: "mousedown".to_string(),
            button: 0,
            buttons: 1,
            x,
            y,
            modifiers: 0,
            click_count: Some(1),
        };
        mp.main_frame
            .dispatch_mouse_event(down)
            .map_err(|e| format!("click (down) failed: {e}"))?;

        let up = MouseEventParams {
            r#type: "mouseup".to_string(),
            button: 0,
            buttons: 0,
            x,
            y,
            modifiers: 0,
            click_count: Some(1),
        };
        mp.main_frame
            .dispatch_mouse_event(up)
            .map_err(|e| format!("click (up) failed: {e}"))?;

        Ok(())
    }

    /// Take a screenshot of a page.
    ///
    /// The clip rectangle is resolved in this order:
    /// 1. `clip` — an explicit `[x, y, width, height]` in document coordinates.
    /// 2. `selector` — the bounding box of the first matching element (scrolled
    ///    into view first), converted to document coordinates.
    /// 3. otherwise the current viewport.
    ///
    /// Juggler's `Page.screenshot` clip is in *document* coordinates (page
    /// origin, not scroll origin), so the viewport default adds
    /// `window.scrollX/scrollY`.
    #[allow(clippy::too_many_arguments)]
    pub fn screenshot(
        &self,
        page_id: &str,
        format: Option<&str>,
        quality: Option<u32>,
        path: Option<&str>,
        selector: Option<&str>,
        clip: Option<[f64; 4]>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, String, Rect), String> {
        let mp = self
            .pages
            .get(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;

        let rect = match (clip, selector) {
            (Some([x, y, width, height]), _) => Rect {
                x,
                y,
                width,
                height,
            },
            (None, Some(sel)) => {
                let (r, scroll_x, scroll_y) = self.element_rect(page_id, sel, timeout)?;
                Rect {
                    x: r.x + scroll_x,
                    y: r.y + scroll_y,
                    width: r.width,
                    height: r.height,
                }
            }
            (None, None) => {
                // Viewport, expressed in document coordinates.
                let dims = self.eval_checked(
                    page_id,
                    "[window.innerWidth, window.innerHeight, window.scrollX, window.scrollY]",
                    timeout,
                )?;
                let f = |i: usize, d: f64| dims.get(i).and_then(|v| v.as_f64()).unwrap_or(d);
                Rect {
                    x: f(2, 0.0),
                    y: f(3, 0.0),
                    width: f(0, 1280.0),
                    height: f(1, 720.0),
                }
            }
        };

        if rect.width <= 0.0 || rect.height <= 0.0 {
            return Err(format!(
                "screenshot clip has zero area ({}x{}) — the element may be hidden",
                rect.width, rect.height
            ));
        }

        let mime = match format {
            Some("jpeg") | Some("jpg") => "image/jpeg",
            _ => "image/png",
        };

        let options = ScreenshotOptions {
            mime_type: mime.to_string(),
            clip: rect.clone(),
            quality,
            omit_device_scale_factor: None,
        };

        let bytes = mp
            .main_frame
            .screenshot(options)
            .map_err(|e| format!("screenshot failed: {e}"))?;

        let ext = if mime == "image/jpeg" { "jpg" } else { "png" };
        let out_path = match path {
            Some(p) => p.to_string(),
            None => format!("/tmp/screenshot-{page_id}.{ext}"),
        };

        std::fs::write(&out_path, &bytes)
            .map_err(|e| format!("failed to write screenshot: {e}"))?;

        Ok((bytes, out_path, rect))
    }

    /// Get page IDs.
    pub fn page_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.pages.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Shut down this instance.
    pub fn stop(self) -> Result<(), String> {
        let Instance {
            browser, mut child, ..
        } = self;
        let _ = browser.close();

        // Wait for graceful exit, kill if needed.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return Ok(()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// InstanceManager
// ---------------------------------------------------------------------------

/// Manages all browser instances for the daemon.
#[derive(Default)]
pub struct InstanceManager {
    instances: HashMap<String, Instance>,
    /// Per-instance context stored separately so we can borrow mutably.
    contexts: HashMap<String, crate::api::BrowserContext>,
    counter: u32,
}

impl InstanceManager {
    pub fn new() -> Self {
        InstanceManager {
            instances: HashMap::new(),
            contexts: HashMap::new(),
            counter: 0,
        }
    }

    /// Launch a new browser instance.
    pub fn launch(
        &mut self,
        headless: Option<bool>,
        executable: Option<&str>,
    ) -> Result<(String, Option<String>, u32), String> {
        let profile_dir =
            tempfile::tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;

        let config = LaunchConfig {
            executable: PathBuf::from(
                executable
                    .map(|s| s.to_owned())
                    .unwrap_or_else(default_executable),
            ),
            profile_dir: Some(profile_dir.path().to_owned()),
            headless: headless.unwrap_or(true),
            ..Default::default()
        };

        let mut launched = crate::process::unix::spawn(&config)
            .map_err(|e| format!("failed to spawn browser: {e}"))?;

        let pid = launched.child.id();

        let _ = crate::process::readiness::wait_for_ready(&mut launched.child, config.timeout)
            .map_err(|e| format!("browser did not become ready: {e}"))?;

        let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
        let conn = Connection::new(Box::new(transport));
        let session = conn.root_session();
        let browser = Browser::connect(conn, session, BrowserOptions::default())
            .map_err(|e| format!("bootstrap failed: {e}"))?;

        let version = browser.version().map(|s| s.to_owned());

        // Create a default context.
        let context = browser
            .new_context(ContextOptions::default())
            .map_err(|e| format!("failed to create context: {e}"))?;

        // Assign instance ID.
        self.counter += 1;
        let instance_id = format!("{:08x}", self.counter);

        let instance = Instance {
            browser,
            child: launched.child,
            version: version.clone(),
            pid,
            _profile_dir: profile_dir,
            pages: HashMap::new(),
            page_counter: 0,
        };

        self.instances.insert(instance_id.clone(), instance);
        self.contexts.insert(instance_id.clone(), context);

        Ok((instance_id, version, pid))
    }

    /// List all instances.
    pub fn list(&self) -> Vec<serde_json::Value> {
        let mut result = Vec::new();
        for (id, inst) in &self.instances {
            result.push(json!({
                "instance_id": id,
                "pid": inst.pid,
                "version": inst.version,
                "pages": inst.page_ids(),
            }));
        }
        result.sort_by(|a, b| {
            a.get("instance_id")
                .and_then(|v| v.as_str())
                .cmp(&b.get("instance_id").and_then(|v| v.as_str()))
        });
        result
    }

    /// Stop an instance.
    pub fn stop(&mut self, instance_id: &str) -> Result<(), String> {
        let inst = self
            .instances
            .remove(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        self.contexts.remove(instance_id);
        inst.stop()
    }

    /// Create a new page in an instance.
    pub fn new_page(&mut self, instance_id: &str) -> Result<String, String> {
        let context = self
            .contexts
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        // We need a raw pointer dance because we need &mut Instance and &BrowserContext
        // at the same time, but they're in different HashMaps so this is safe.
        let inst = self
            .instances
            .get_mut(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.create_page(context)
    }

    /// Navigate a page.
    ///
    /// Returns a `NavigateOutcome` with `nav_id` and `status_code` (G4).
    pub fn navigate(
        &self,
        instance_id: &str,
        page_id: &str,
        url: &str,
        timeout: Duration,
        wait_until: Option<&str>,
    ) -> Result<NavigateOutcome, String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.navigate(page_id, url, timeout, wait_until)
    }

    /// Evaluate JavaScript.
    pub fn evaluate(
        &self,
        instance_id: &str,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.evaluate(page_id, expression, timeout)
    }

    /// Dispatch a trusted left-click at viewport coordinates (x, y).
    pub fn click(&self, instance_id: &str, page_id: &str, x: i32, y: i32) -> Result<(), String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.click(page_id, x, y)
    }

    /// Take a screenshot.
    #[allow(clippy::too_many_arguments)]
    pub fn screenshot(
        &self,
        instance_id: &str,
        page_id: &str,
        format: Option<&str>,
        quality: Option<u32>,
        path: Option<&str>,
        selector: Option<&str>,
        clip: Option<[f64; 4]>,
        timeout: Duration,
    ) -> Result<(Vec<u8>, String, Rect), String> {
        let inst = self
            .instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        inst.screenshot(page_id, format, quality, path, selector, clip, timeout)
    }

    /// Borrow an instance by id, or report a clear "not found" error.
    fn instance(&self, instance_id: &str) -> Result<&Instance, String> {
        self.instances
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))
    }

    /// Mutably borrow an instance by id.
    fn instance_mut(&mut self, instance_id: &str) -> Result<&mut Instance, String> {
        self.instances
            .get_mut(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    /// Extract page text.
    pub fn text(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<String, String> {
        self.instance(instance_id)?.text(page_id, selector, timeout)
    }

    /// Extract page HTML.
    pub fn html(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<String, String> {
        self.instance(instance_id)?.html(page_id, selector, timeout)
    }

    /// Collect page links.
    pub fn links(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.instance(instance_id)?
            .links(page_id, selector, timeout)
    }

    /// Extract structured page metadata.
    pub fn data(
        &self,
        instance_id: &str,
        page_id: &str,
        og: bool,
        jsonld: bool,
        meta: bool,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.instance(instance_id)?
            .data(page_id, og, jsonld, meta, timeout)
    }

    // -----------------------------------------------------------------------
    // Navigation / waiting
    // -----------------------------------------------------------------------

    /// Read a page's URL and title.
    pub fn url(
        &self,
        instance_id: &str,
        page_id: &str,
        timeout: Duration,
    ) -> Result<(String, String), String> {
        self.instance(instance_id)?.url(page_id, timeout)
    }

    /// Go back one history entry.
    pub fn go_back(&self, instance_id: &str, page_id: &str) -> Result<bool, String> {
        self.instance(instance_id)?.go_back(page_id)
    }

    /// Go forward one history entry.
    pub fn go_forward(&self, instance_id: &str, page_id: &str) -> Result<bool, String> {
        self.instance(instance_id)?.go_forward(page_id)
    }

    /// Reload a page.
    pub fn reload(&self, instance_id: &str, page_id: &str) -> Result<(), String> {
        self.instance(instance_id)?.reload(page_id)
    }

    /// Wait for a selector to appear.
    pub fn wait_for_selector(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<u128, String> {
        self.instance(instance_id)?
            .wait_for_selector(page_id, selector, timeout)
    }

    // -----------------------------------------------------------------------
    // Cookies / headers
    // -----------------------------------------------------------------------

    /// Set a single cookie on the instance's browser context.
    ///
    /// When neither `url` nor `domain` is supplied, the cookie is bound to the
    /// page's current URL — `Browser.setCookies` requires one of the two.
    #[allow(clippy::too_many_arguments)]
    pub fn set_cookie(
        &self,
        instance_id: &str,
        page_id: &str,
        name: &str,
        value: &str,
        url: Option<&str>,
        domain: Option<&str>,
        path: Option<&str>,
        secure: bool,
        http_only: bool,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let ctx = self
            .contexts
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;

        let resolved_url = match (url, domain) {
            (Some(u), _) => Some(u.to_string()),
            (None, Some(_)) => None,
            (None, None) => {
                let (page_url, _) = self.instance(instance_id)?.url(page_id, timeout)?;
                if !page_url.starts_with("http") {
                    return Err(format!(
                        "page {page_id} is at {page_url:?}; pass --url or --domain to \
                         say where the cookie belongs"
                    ));
                }
                Some(page_url)
            }
        };

        let options = CookieOptions {
            name: name.to_string(),
            value: value.to_string(),
            url: resolved_url.clone(),
            domain: domain.map(|d| d.to_string()),
            path: path.map(|p| p.to_string()),
            secure: if secure { Some(true) } else { None },
            http_only: if http_only { Some(true) } else { None },
            same_site: None,
            expires: None,
        };

        ctx.set_cookies(std::slice::from_ref(&options))
            .map_err(|e| format!("set_cookies failed: {e}"))?;

        Ok(json!({
            "cookie_set": name,
            "value": value,
            "url": resolved_url,
            "domain": domain,
        }))
    }

    /// Add an extra HTTP request header to a page.
    pub fn set_header(
        &mut self,
        instance_id: &str,
        page_id: &str,
        name: &str,
        value: &str,
    ) -> Result<Vec<(String, String)>, String> {
        self.instance_mut(instance_id)?
            .set_header(page_id, name, value)
    }

    // -----------------------------------------------------------------------
    // Interaction
    // -----------------------------------------------------------------------

    /// Click an element by CSS selector.
    pub fn click_selector(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        self.instance(instance_id)?
            .click_selector(page_id, selector, timeout)
    }

    /// Hover an element by CSS selector.
    pub fn hover(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        self.instance(instance_id)?
            .hover(page_id, selector, timeout)
    }

    /// Fill an element by CSS selector.
    pub fn fill(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<String, String> {
        self.instance(instance_id)?
            .fill(page_id, selector, value, timeout)
    }

    /// Insert text into the focused element.
    pub fn insert_text(&self, instance_id: &str, page_id: &str, text: &str) -> Result<(), String> {
        self.instance(instance_id)?.insert_text(page_id, text)
    }

    /// Press a named key.
    pub fn press(&self, instance_id: &str, page_id: &str, key: &str) -> Result<(), String> {
        self.instance(instance_id)?.press(page_id, key)
    }

    /// Select an option in a `<select>`.
    pub fn select_option(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        value: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.instance(instance_id)?
            .select_option(page_id, selector, value, timeout)
    }

    /// Scroll an element into view, or to the page bottom.
    pub fn scroll(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.instance(instance_id)?
            .scroll(page_id, selector, timeout)
    }

    // -----------------------------------------------------------------------
    // Tabs
    // -----------------------------------------------------------------------

    /// List an instance's open pages with URL + title.
    pub fn tabs(
        &self,
        instance_id: &str,
        timeout: Duration,
    ) -> Result<Vec<serde_json::Value>, String> {
        Ok(self.instance(instance_id)?.tabs(timeout))
    }

    /// Close a page.
    pub fn close_tab(&mut self, instance_id: &str, page_id: &str) -> Result<(), String> {
        self.instance_mut(instance_id)?.close_tab(page_id)
    }

    /// Export all cookies for an instance's browser context (including HttpOnly).
    ///
    /// Calls `Browser.getCookies` on the root session with the instance's
    /// `browserContextId`. HttpOnly cookies are included — the Juggler protocol
    /// returns them in the same array as ordinary cookies.
    pub fn cookies(&self, instance_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let ctx = self
            .contexts
            .get(instance_id)
            .ok_or_else(|| format!("instance {instance_id} not found"))?;
        let cookies = ctx
            .get_cookies()
            .map_err(|e| format!("get_cookies failed: {e}"))?;
        let values: Vec<serde_json::Value> = cookies
            .iter()
            .map(|c| serde_json::to_value(c).expect("Cookie is always serializable"))
            .collect();
        Ok(values)
    }

    /// Number of running instances.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    /// Shut down all instances.
    pub fn shutdown_all(&mut self) {
        let ids: Vec<String> = self.instances.keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `js_literal` produces a quoted literal that survives being pasted into
    /// a JavaScript expression — quotes, backslashes and newlines escaped.
    #[test]
    fn js_literal_escapes_quotes_and_backslashes() {
        assert_eq!(js_literal("a"), "\"a\"");
        assert_eq!(js_literal("he said \"hi\""), "\"he said \\\"hi\\\"\"");
        assert_eq!(js_literal("back\\slash"), "\"back\\\\slash\"");
        assert_eq!(js_literal("two\nlines"), "\"two\\nlines\"");
    }

    /// A selector crafted to break out of the string literal stays inside it.
    /// The rendered literal must not contain an unescaped closing quote.
    #[test]
    fn js_literal_neutralises_breakout_attempt() {
        let hostile = r#"x"); window.__pwned = 1; ("#;
        let literal = js_literal(hostile);
        // Exactly two unescaped quotes: the opening and closing delimiters.
        let unescaped_quotes = literal
            .char_indices()
            .filter(|(i, c)| *c == '"' && (*i == 0 || literal.as_bytes()[i - 1] != b'\\'))
            .count();
        assert_eq!(
            unescaped_quotes, 2,
            "literal {literal} must have only its delimiters unescaped"
        );
        assert!(literal.starts_with('"') && literal.ends_with('"'));
    }

    /// U+2028/U+2029 are valid in a JSON string but terminate a JavaScript
    /// line, so they must be escaped too.
    #[test]
    fn js_literal_escapes_line_separators() {
        let literal = js_literal("a\u{2028}b\u{2029}c");
        assert!(!literal.contains('\u{2028}'), "U+2028 escaped");
        assert!(!literal.contains('\u{2029}'), "U+2029 escaped");
        assert!(literal.contains("\\u2028") && literal.contains("\\u2029"));
    }

    /// Named keys map to the codes Gecko expects.
    #[test]
    fn key_descriptor_maps_named_keys() {
        assert_eq!(
            key_descriptor("Enter").unwrap(),
            ("Enter".to_string(), "Enter".to_string(), 13)
        );
        assert_eq!(
            key_descriptor("ArrowDown").unwrap(),
            ("ArrowDown".to_string(), "ArrowDown".to_string(), 40)
        );
        // Aliases resolve to the canonical key.
        assert_eq!(key_descriptor("Return").unwrap().1, "Enter");
        assert_eq!(key_descriptor("Esc").unwrap().1, "Escape");
    }

    /// Single printable characters get a `KeyX`/`DigitN` code and the
    /// uppercase key code, matching a US layout.
    #[test]
    fn key_descriptor_maps_single_characters() {
        assert_eq!(
            key_descriptor("a").unwrap(),
            ("KeyA".to_string(), "a".to_string(), 'A' as u32)
        );
        assert_eq!(
            key_descriptor("7").unwrap(),
            ("Digit7".to_string(), "7".to_string(), '7' as u32)
        );
        // Punctuation resolves to its real US-layout `code` and legacy
        // `keyCode` — NOT the raw codepoint. `.` must be Period/190, never
        // codepoint 46 (which is Delete's keyCode).
        assert_eq!(
            key_descriptor(".").unwrap(),
            ("Period".to_string(), ".".to_string(), 190)
        );
        assert_eq!(
            key_descriptor("/").unwrap(),
            ("Slash".to_string(), "/".to_string(), 191)
        );
        assert_eq!(
            key_descriptor(";").unwrap(),
            ("Semicolon".to_string(), ";".to_string(), 186)
        );
        // Shifted variants share the unshifted key's code/keyCode.
        assert_eq!(
            key_descriptor("?").unwrap(),
            ("Slash".to_string(), "?".to_string(), 191)
        );
    }

    /// Unknown multi-character key names are rejected with a helpful message
    /// rather than silently dispatching a bogus event.
    #[test]
    fn key_descriptor_rejects_unknown_names() {
        let err = key_descriptor("Frobnicate").unwrap_err();
        assert!(err.contains("unknown key"), "got: {err}");
        assert!(err.contains("Enter"), "error lists valid keys: {err}");
    }
}
