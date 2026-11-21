//! Browser context (isolated profile) wrapper.
//!
//! A [`BrowserContext`] represents an isolated browser context within the
//! Camoufox browser. Each context has its own cookies, cache, local storage,
//! and other browsing data. Contexts are created via
//! [`Browser::new_context`](crate::api::browser::Browser::new_context)
//! and destroyed via [`BrowserContext::close`].

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::browser::{Connection, ProxyConfig, Session};
use crate::api::main_frame::MainFrame;
use crate::protocol::errors::ProtocolError;

// ---------------------------------------------------------------------------
// Context configuration types
// ---------------------------------------------------------------------------

/// Options for configuring a browser context.
///
/// All fields are optional. Only the non-`None` / non-default fields result
/// in protocol commands being sent. Per PROTOCOL.md Section 12, step 5, all
/// configuration commands for a context are sent in parallel.
#[derive(Debug, Clone, Default)]
pub struct ContextOptions {
    /// Policy for JavaScript dialogs on newly created pages.
    pub dialog_policy: crate::api::DialogPolicy,
    /// Override the user-agent string for this context.
    pub user_agent: Option<String>,

    /// Set the default viewport size. Pass `None` for the browser default.
    pub viewport: Option<Viewport>,

    /// Device scale factor override.
    pub device_scale_factor: Option<f64>,

    /// Override the locale (e.g., `"en-US"`). Also sets the `Accept-Language`
    /// header via `Browser.setExtraHTTPHeaders`.
    pub locale: Option<String>,

    /// Override the timezone (IANA timezone ID, e.g., `"America/New_York"`).
    /// Invalid IDs will cause an error.
    pub timezone_id: Option<String>,

    /// Override geolocation.
    pub geolocation: Option<Geolocation>,

    /// Preferred color scheme: `"dark"`, `"light"`, or `"no-preference"`.
    pub color_scheme: Option<String>,

    /// Preferred reduced motion: `"reduce"` or `"no-preference"`.
    pub reduced_motion: Option<String>,

    /// Forced colors: `"active"` or `"none"`.
    pub forced_colors: Option<String>,

    /// Contrast preference: `"less"`, `"more"`, `"custom"`, or `"no-preference"`.
    pub contrast: Option<String>,

    /// Whether to bypass Content-Security-Policy.
    pub bypass_csp: bool,

    /// Whether to ignore HTTPS errors (invalid certificates, etc.).
    pub ignore_https_errors: bool,

    /// Whether JavaScript is disabled. Defaults to `false` (JS enabled).
    pub java_script_disabled: bool,

    /// Extra HTTP headers to send with every request in this context.
    pub extra_http_headers: Vec<(String, String)>,

    /// Whether the context should simulate offline mode.
    pub offline: bool,

    /// Whether touch events are enabled.
    pub has_touch: bool,

    /// HTTP authentication credentials. Set to `None` to clear.
    pub http_credentials: Option<HttpCredentials>,

    /// Context-level proxy configuration.
    pub proxy: Option<ProxyConfig>,

    /// Override `navigator.platform`.
    pub platform: Option<String>,

    /// Init scripts to inject into every page in this context.
    pub init_scripts: Vec<InitScript>,

    /// Download behavior configuration.
    pub download_options: Option<DownloadOptions>,

    /// Screencast (video recording) options.
    pub screencast_options: Option<ScreencastRecordingOptions>,
}

/// Viewport dimensions.
#[derive(Debug, Clone)]
pub struct Viewport {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Geolocation coordinates.
#[derive(Debug, Clone)]
pub struct Geolocation {
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
    /// Optional accuracy in meters.
    pub accuracy: Option<f64>,
}

/// HTTP authentication credentials.
#[derive(Debug, Clone)]
pub struct HttpCredentials {
    /// Username.
    pub username: String,
    /// Password.
    pub password: String,
    /// Optional origin to scope the credentials to.
    pub origin: Option<String>,
}

/// An init script to inject into every page.
#[derive(Debug, Clone)]
pub struct InitScript {
    /// The JavaScript source code.
    pub script: String,
    /// Optional world name for isolation.
    pub world_name: Option<String>,
}

/// Download behavior options.
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Download behavior: `"saveToDisk"` or `"cancel"`.
    pub behavior: Option<String>,
    /// Directory to save downloads to.
    pub downloads_dir: Option<String>,
}

/// Screencast (video recording) options for context-level setup.
#[derive(Debug, Clone)]
pub struct ScreencastRecordingOptions {
    /// Recording width in pixels (10-10000).
    pub width: u32,
    /// Recording height in pixels (10-10000).
    pub height: u32,
    /// Quality (0-100). Playwright typically uses 90.
    pub quality: u32,
}

// ---------------------------------------------------------------------------
// Cookie types (PROTOCOL.md Section 11)
// ---------------------------------------------------------------------------

/// Parameters for setting a cookie via `Browser.setCookies`.
///
/// All optional fields use `#[serde(skip_serializing_if = "Option::is_none")]`
/// to satisfy the protocol's `t.Optional` semantics (absence, not null).
#[derive(Debug, Clone, Serialize)]
pub struct CookieOptions {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// URL to associate the cookie with. Either `url` or `domain` must be set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Cookie domain. If absent, derived from `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Cookie path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether the cookie is secure (HTTPS only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    /// Whether the cookie is HTTP-only (not accessible via JavaScript).
    #[serde(rename = "httpOnly", skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    /// SameSite attribute: `"Strict"`, `"Lax"`, or `"None"`.
    #[serde(rename = "sameSite", skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    /// Expiry as a Unix timestamp in seconds. `-1` for session cookies.
    /// Capped at 400 days from now by the browser.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
}

/// A cookie returned by `Browser.getCookies`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Cookie domain.
    pub domain: String,
    /// Cookie path.
    pub path: String,
    /// Expiry as a Unix timestamp in seconds. `-1` for session cookies.
    pub expires: f64,
    /// Size in bytes (`name.length + value.length`).
    pub size: u32,
    /// Whether the cookie is HTTP-only.
    #[serde(rename = "httpOnly")]
    pub http_only: bool,
    /// Whether the cookie is secure.
    pub secure: bool,
    /// Whether this is a session cookie.
    pub session: bool,
    /// SameSite attribute: `"Strict"`, `"Lax"`, or `"None"`.
    #[serde(rename = "sameSite")]
    pub same_site: String,
}

// ---------------------------------------------------------------------------
// BrowserContext
// ---------------------------------------------------------------------------

/// A browser context (isolated profile).
///
/// Each context is an independent browsing session with its own cookies,
/// cache, and local storage. Pages created within a context share its
/// configuration.
///
/// # Creating pages
///
/// Use [`new_main_frame`](BrowserContext::new_main_frame) to create a new
/// page in this context. The returned [`MainFrame`] is pinned to the top
/// frame and inherits the context's configuration.
///
/// # Cleanup
///
/// Call [`close`](BrowserContext::close) to destroy the context and all its
/// pages. If the context was created with `removeOnDetach: true` (the default
/// from [`Browser::new_context`](crate::api::browser::Browser::new_context)),
/// it will also be cleaned up automatically when the pipe disconnects.
pub struct BrowserContext {
    /// The server-assigned context ID.
    context_id: String,
    /// Cloned root session handle (context commands go through root).
    /// `Session` is `Clone` and backed by `Arc<Mutex<...>>` internally.
    session: Session,
    /// Shared connection reference.
    connection: Arc<Connection>,
    registry: Arc<crate::api::page_registry::PageRegistry>,
}

impl BrowserContext {
    /// Create a new `BrowserContext`.
    ///
    /// This is an internal constructor. External users should use
    /// [`Browser::new_context`](crate::api::browser::Browser::new_context).
    pub(crate) fn new(context_id: String, session: &Session, connection: Arc<Connection>) -> Self {
        let registry = crate::api::page_registry::PageRegistry::install(
            &connection,
            &context_id,
            crate::api::DialogPolicy::Manual,
        );
        BrowserContext {
            context_id,
            session: session.clone(),
            connection,
            registry,
        }
    }

    /// Access the persistent page registry for this context.
    #[cfg(any(feature = "cli", test))]
    pub(crate) fn registry(&self) -> &Arc<crate::api::page_registry::PageRegistry> {
        &self.registry
    }

    /// Current dialog policy.
    pub fn dialog_policy(&self) -> crate::api::DialogPolicy {
        self.registry.dialog_policy()
    }

    /// Set the policy used for newly created pages.
    pub fn set_dialog_policy(&self, policy: crate::api::DialogPolicy) {
        self.registry.set_dialog_policy(policy);
    }

    /// Return all popups opened by pages in this context.
    pub fn take_popups(&self) -> Vec<MainFrame> {
        self.registry
            .take_popups()
            .into_iter()
            .filter_map(|tracked| {
                MainFrame::from_tracked(
                    tracked,
                    Arc::clone(&self.connection),
                    Arc::clone(&self.registry),
                )
                .ok()
            })
            .collect()
    }

    /// Returns the context ID assigned by the browser.
    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    /// Convenience alias for [`context_id`](Self::context_id).
    pub fn id(&self) -> &str {
        &self.context_id
    }

    /// Returns a reference to the shared connection.
    pub fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }

    /// Get a reference to the root session.
    fn session(&self) -> &Session {
        &self.session
    }

    /// Helper: build a JSON object with `browserContextId` included.
    fn ctx_params(&self, extra: serde_json::Value) -> serde_json::Value {
        let mut obj = if extra.is_object() { extra } else { json!({}) };
        obj["browserContextId"] = json!(self.context_id);
        obj
    }

    /// Apply all context options.
    ///
    /// Sends the appropriate `Browser.set*` commands for each non-default
    /// option. Per PROTOCOL.md Section 12, step 5, these are conceptually
    /// sent in parallel (the protocol layer handles pipelining). In the
    /// current sync implementation they are sent sequentially.
    ///
    /// # Errors
    ///
    /// Returns the first [`ProtocolError`] encountered. Subsequent commands
    /// are not sent after a failure.
    pub fn configure(&self, options: &ContextOptions) -> Result<(), ProtocolError> {
        self.set_dialog_policy(options.dialog_policy);
        let s = self.session();

        // Viewport
        if let Some(ref vp) = options.viewport {
            let mut viewport_obj = json!({
                "viewportSize": { "width": vp.width, "height": vp.height }
            });
            if let Some(dsf) = options.device_scale_factor {
                viewport_obj["deviceScaleFactor"] = json!(dsf);
            }
            s.send(
                "Browser.setDefaultViewport",
                self.ctx_params(json!({ "viewport": viewport_obj })),
            )?;
        }

        // User-agent
        if let Some(ref ua) = options.user_agent {
            s.send(
                "Browser.setUserAgentOverride",
                self.ctx_params(json!({ "userAgent": ua })),
            )?;
        }

        // Platform
        if let Some(ref platform) = options.platform {
            s.send(
                "Browser.setPlatformOverride",
                self.ctx_params(json!({ "platform": platform })),
            )?;
        }

        // Bypass CSP
        if options.bypass_csp {
            s.send(
                "Browser.setBypassCSP",
                self.ctx_params(json!({ "bypassCSP": true })),
            )?;
        }

        // Ignore HTTPS errors
        if options.ignore_https_errors {
            s.send(
                "Browser.setIgnoreHTTPSErrors",
                self.ctx_params(json!({ "ignoreHTTPSErrors": true })),
            )?;
        }

        // JavaScript disabled
        if options.java_script_disabled {
            s.send(
                "Browser.setJavaScriptDisabled",
                self.ctx_params(json!({ "javaScriptDisabled": true })),
            )?;
        }

        // Locale
        if let Some(ref locale) = options.locale {
            s.send(
                "Browser.setLocaleOverride",
                self.ctx_params(json!({ "locale": locale })),
            )?;
        }

        // Timezone
        if let Some(ref tz) = options.timezone_id {
            s.send(
                "Browser.setTimezoneOverride",
                self.ctx_params(json!({ "timezoneId": tz })),
            )?;
        }

        // Extra HTTP headers (including Accept-Language from locale)
        if !options.extra_http_headers.is_empty() || options.locale.is_some() {
            let mut headers: Vec<serde_json::Value> = options
                .extra_http_headers
                .iter()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect();

            // If locale is set, add Accept-Language header.
            if let Some(ref locale) = options.locale {
                headers.push(json!({"name": "Accept-Language", "value": locale}));
            }

            s.send(
                "Browser.setExtraHTTPHeaders",
                self.ctx_params(json!({ "headers": headers })),
            )?;
        }

        // HTTP credentials
        if let Some(ref creds) = options.http_credentials {
            let mut cred_obj = json!({
                "username": creds.username,
                "password": creds.password,
            });
            if let Some(ref origin) = creds.origin {
                cred_obj["origin"] = json!(origin);
            }
            s.send(
                "Browser.setHTTPCredentials",
                self.ctx_params(json!({ "credentials": cred_obj })),
            )?;
        }

        // Geolocation
        if let Some(ref geo) = options.geolocation {
            let mut geo_obj = json!({
                "latitude": geo.latitude,
                "longitude": geo.longitude,
            });
            if let Some(acc) = geo.accuracy {
                geo_obj["accuracy"] = json!(acc);
            }
            s.send(
                "Browser.setGeolocationOverride",
                self.ctx_params(json!({ "geolocation": geo_obj })),
            )?;
        }

        // Online/offline
        if options.offline {
            s.send(
                "Browser.setOnlineOverride",
                self.ctx_params(json!({ "override": "offline" })),
            )?;
        }

        // Touch
        if options.has_touch {
            s.send(
                "Browser.setTouchOverride",
                self.ctx_params(json!({ "hasTouch": true })),
            )?;
        }

        // Color scheme
        if let Some(ref cs) = options.color_scheme {
            s.send(
                "Browser.setColorScheme",
                self.ctx_params(json!({ "colorScheme": cs })),
            )?;
        }

        // Reduced motion
        if let Some(ref rm) = options.reduced_motion {
            s.send(
                "Browser.setReducedMotion",
                self.ctx_params(json!({ "reducedMotion": rm })),
            )?;
        }

        // Forced colors
        if let Some(ref fc) = options.forced_colors {
            s.send(
                "Browser.setForcedColors",
                self.ctx_params(json!({ "forcedColors": fc })),
            )?;
        }

        // Contrast
        if let Some(ref c) = options.contrast {
            s.send(
                "Browser.setContrast",
                self.ctx_params(json!({ "contrast": c })),
            )?;
        }

        // Download options
        if let Some(ref dl) = options.download_options {
            let mut dl_obj = json!({});
            let behavior = dl.behavior.as_deref().unwrap_or("cancel");
            dl_obj["behavior"] = json!(behavior);
            if let Some(ref dir) = dl.downloads_dir {
                dl_obj["downloadsDir"] = json!(dir);
            }
            s.send(
                "Browser.setDownloadOptions",
                self.ctx_params(json!({ "downloadOptions": dl_obj })),
            )?;
        }

        // Screencast options
        if let Some(ref sc) = options.screencast_options {
            s.send(
                "Browser.setScreencastOptions",
                self.ctx_params(json!({
                    "options": {
                        "width": sc.width,
                        "height": sc.height,
                        "quality": sc.quality,
                    }
                })),
            )?;
        }

        // Context-level proxy
        if let Some(ref proxy) = options.proxy {
            s.send(
                "Browser.setContextProxy",
                proxy.to_context_json(&self.context_id),
            )?;
        }

        // Init scripts (only sent if non-empty)
        if !options.init_scripts.is_empty() {
            let scripts: Vec<serde_json::Value> = options
                .init_scripts
                .iter()
                .map(|is| {
                    let mut obj = json!({ "script": is.script });
                    if let Some(ref wn) = is.world_name {
                        obj["worldName"] = json!(wn);
                    }
                    obj
                })
                .collect();
            s.send(
                "Browser.setInitScripts",
                self.ctx_params(json!({ "scripts": scripts })),
            )?;
        }

        Ok(())
    }

    /// Timeout for waiting on target attach and main frame attach.
    #[cfg(not(test))]
    const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
    #[cfg(test)]
    const ATTACH_TIMEOUT: Duration = Duration::from_millis(100);

    /// Create a new page and return a fully-wired [`MainFrame`].
    ///
    /// Marks that a page creation is in flight, sends `Browser.newPage`,
    /// claims the target from the registry (waiting for its main frame attach),
    /// and constructs the `MainFrame`.
    pub fn new_main_frame(&self) -> Result<MainFrame, ProtocolError> {
        let mut inflight = self.registry.begin_new_page();
        let result = match self.session().send(
            "Browser.newPage",
            json!({ "browserContextId": self.context_id }),
        ) {
            Ok(res) => res,
            Err(e) => {
                inflight.mark_failed();
                return Err(e);
            }
        };
        let target_id = match result
            .get("targetId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(tid) => tid.to_owned(),
            None => {
                inflight.mark_failed();
                return Err(ProtocolError::closed_with(
                    "Browser.newPage",
                    "missing or empty targetId in Browser.newPage response",
                ));
            }
        };

        match self.registry.claim(&target_id, Self::ATTACH_TIMEOUT) {
            Ok(tracked) => {
                drop(inflight);
                MainFrame::from_tracked(
                    tracked,
                    Arc::clone(&self.connection),
                    Arc::clone(&self.registry),
                )
            }
            Err(e) => {
                self.registry.abandon_target(&target_id);
                drop(inflight);
                Err(e)
            }
        }
    }

    /// Set cookies for this context.
    ///
    /// # Cookie domain resolution
    ///
    /// If `domain` is absent on a cookie, `url` must be provided.
    ///
    /// # Cookie expiry
    ///
    /// - `None` or `-1` creates a session cookie.
    /// - Values are capped at 400 days by the browser.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_cookies(&self, cookies: &[CookieOptions]) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.setCookies",
            self.ctx_params(json!({ "cookies": cookies })),
        )?;
        Ok(())
    }

    /// Get all cookies for this context.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn get_cookies(&self) -> Result<Vec<Cookie>, ProtocolError> {
        let result = self.session().send(
            "Browser.getCookies",
            json!({ "browserContextId": self.context_id }),
        )?;

        let cookies: Vec<Cookie> = result
            .get("cookies")
            .cloned()
            .map(|v| serde_json::from_value(v).unwrap_or_default())
            .unwrap_or_default();

        Ok(cookies)
    }

    /// Clear all cookies for this context.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn clear_cookies(&self) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.clearCookies",
            json!({ "browserContextId": self.context_id }),
        )?;
        Ok(())
    }

    /// Grant permissions to an origin in this context.
    ///
    /// # Permission values
    ///
    /// - `"geo"` -- geolocation
    /// - `"persistent-storage"` -- storage access
    /// - `"push"` -- push notifications
    /// - `"desktop-notification"` -- desktop notifications
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn grant_permissions(
        &self,
        origin: &str,
        permissions: &[&str],
    ) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.grantPermissions",
            json!({
                "browserContextId": self.context_id,
                "origin": origin,
                "permissions": permissions,
            }),
        )?;
        Ok(())
    }

    /// Reset all permissions for this context.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn reset_permissions(&self) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.resetPermissions",
            json!({ "browserContextId": self.context_id }),
        )?;
        Ok(())
    }

    /// Set request interception for this context.
    ///
    /// When enabled, all network requests in this context will emit
    /// `Network.requestWillBeSent` events with `isIntercepted: true`.
    /// Each intercepted request must be handled with exactly one of:
    /// - `Network.resumeInterceptedRequest`
    /// - `Network.fulfillInterceptedRequest`
    /// - `Network.abortInterceptedRequest`
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_request_interception(&self, enabled: bool) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.setRequestInterception",
            self.ctx_params(json!({ "enabled": enabled })),
        )?;
        Ok(())
    }

    /// Add a JavaScript binding to this context.
    ///
    /// The binding will be available as `window.<name>()` in all pages
    /// created in this context.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn add_binding(&self, name: &str, script: &str) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.addBinding",
            self.ctx_params(json!({
                "name": name,
                "script": script,
            })),
        )?;
        Ok(())
    }

    /// Set whether the cache is disabled for this context.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_cache_disabled(&self, disabled: bool) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.setCacheDisabled",
            self.ctx_params(json!({ "cacheDisabled": disabled })),
        )?;
        Ok(())
    }

    /// Remove this context and close all its pages.
    ///
    /// Sends `Browser.removeBrowserContext`. This method consumes `self`
    /// to prevent further use.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn close(self) -> Result<(), ProtocolError> {
        self.session().send(
            "Browser.removeBrowserContext",
            json!({ "browserContextId": self.context_id }),
        )?;
        Ok(())
    }

    /// Attempt to remove this context. Used for cleanup on error paths.
    ///
    /// Unlike [`close`](BrowserContext::close), this does not consume `self`
    /// and silently ignores errors.
    pub(crate) fn try_remove(&self) {
        let _ = self.session().send_may_fail(
            "Browser.removeBrowserContext",
            json!({ "browserContextId": self.context_id }),
        );
    }
}

impl std::fmt::Debug for BrowserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowserContext")
            .field("context_id", &self.context_id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::client::Connection;
    use crate::protocol::types::RawMessage;
    use crate::transport::errors::TransportError;
    use crate::transport::{Transport, TransportReader, TransportWriter};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::thread;

    struct TestWriter {
        outgoing_tx: mpsc::Sender<serde_json::Value>,
        closed: Arc<AtomicBool>,
    }

    impl TransportWriter for TestWriter {
        fn send(&mut self, message: &serde_json::Value) -> Result<(), TransportError> {
            if self.closed.load(Ordering::SeqCst) {
                return Err(TransportError::Closed);
            }
            self.outgoing_tx
                .send(message.clone())
                .map_err(|_| TransportError::Closed)
        }

        fn close(&mut self) -> Result<(), TransportError> {
            self.closed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    struct TestReader {
        incoming_rx: mpsc::Receiver<RawMessage>,
        closed: Arc<AtomicBool>,
    }

    impl TransportReader for TestReader {
        fn receive(&mut self) -> Result<RawMessage, TransportError> {
            loop {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(TransportError::Closed);
                }
                match self
                    .incoming_rx
                    .recv_timeout(std::time::Duration::from_millis(10))
                {
                    Ok(msg) => return Ok(msg),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(TransportError::Closed);
                    }
                }
            }
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    struct TestTransport {
        incoming_rx: mpsc::Receiver<RawMessage>,
        outgoing_tx: mpsc::Sender<serde_json::Value>,
        closed: Arc<AtomicBool>,
    }

    impl TestTransport {
        fn new() -> (
            Self,
            mpsc::Sender<RawMessage>,
            mpsc::Receiver<serde_json::Value>,
        ) {
            let (in_tx, in_rx) = mpsc::channel();
            let (out_tx, out_rx) = mpsc::channel();
            (
                Self {
                    incoming_rx: in_rx,
                    outgoing_tx: out_tx,
                    closed: Arc::new(AtomicBool::new(false)),
                },
                in_tx,
                out_rx,
            )
        }
    }

    impl Transport for TestTransport {
        fn into_split(self: Box<Self>) -> (Box<dyn TransportWriter>, Box<dyn TransportReader>) {
            (
                Box::new(TestWriter {
                    outgoing_tx: self.outgoing_tx,
                    closed: Arc::clone(&self.closed),
                }),
                Box::new(TestReader {
                    incoming_rx: self.incoming_rx,
                    closed: self.closed,
                }),
            )
        }
    }

    #[test]
    fn new_main_frame_handler_leak_regression() {
        let (transport, in_tx, out_rx) = TestTransport::new();
        let conn = Arc::new(Connection::new(Box::new(transport)));
        let root_session = conn.root_session();
        let context = BrowserContext::new("ctx-test".into(), &root_session, Arc::clone(&conn));

        let baseline = conn.handler_count();
        assert_eq!(
            baseline, 2,
            "2 persistent root handlers installed by PageRegistry"
        );

        // 1. Timeout path: Browser.newPage responds, but attach event never comes
        let in_tx_clone = in_tx.clone();
        let responder1 = thread::spawn(move || {
            let req = out_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let id = req["id"].as_i64().unwrap();
            assert_eq!(req["method"], "Browser.newPage");
            in_tx_clone
                .send(RawMessage {
                    id: Some(id),
                    method: None,
                    params: None,
                    result: Some(json!({"targetId": "target-1"})),
                    error: None,
                    session_id: None,
                })
                .unwrap();
            out_rx
        });

        let err = context
            .new_main_frame()
            .expect_err("new_main_frame should time out");
        assert!(err.message.contains("timeout"));
        assert_eq!(
            conn.handler_count(),
            baseline,
            "handler count must return to baseline after new_main_frame timeout"
        );
        let out_rx = responder1.join().unwrap();

        // 2. Success path: MainFrame retains six session handlers until drop.
        let in_tx_clone2 = in_tx.clone();
        let responder2 = thread::spawn(move || {
            let req = out_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let id = req["id"].as_i64().unwrap();
            assert_eq!(req["method"], "Browser.newPage");
            in_tx_clone2
                .send(RawMessage {
                    id: Some(id),
                    method: None,
                    params: None,
                    result: Some(json!({"targetId": "target-2"})),
                    error: None,
                    session_id: None,
                })
                .unwrap();

            // Send page attached event
            in_tx_clone2
                .send(RawMessage {
                    id: None,
                    method: Some("Browser.attachedToTarget".into()),
                    params: Some(json!({
                        "sessionId": "page-session-2",
                        "targetInfo": {
                            "type": "page",
                            "targetId": "target-2",
                            "browserContextId": "ctx-test",
                        }
                    })),
                    result: None,
                    error: None,
                    session_id: None,
                })
                .unwrap();

            // Send frame attached event
            in_tx_clone2
                .send(RawMessage {
                    id: None,
                    method: Some("Page.frameAttached".into()),
                    params: Some(json!({
                        "frameId": "frame-2",
                        "parentFrameId": "",
                    })),
                    result: None,
                    error: None,
                    session_id: Some("page-session-2".into()),
                })
                .unwrap();

            out_rx
        });

        let main_frame = context
            .new_main_frame()
            .expect("new_main_frame should succeed");
        assert_eq!(
            conn.handler_count(),
            baseline + 6,
            "MainFrame should retain exactly 6 session handlers (frames, contexts, dialogs)"
        );

        // Claim wakes as soon as frameAttached updates state. Ensure that
        // handler has released its temporary registry upgrade before asserting
        // synchronous destruction below (the reader may own the last Arc).
        let (idle_tx, idle_rx) = mpsc::channel();
        let idle_id = conn.on_event(
            "",
            "Test.idle",
            Arc::new(move |_| {
                idle_tx.send(()).unwrap();
            }),
        );
        in_tx
            .send(RawMessage {
                id: None,
                method: Some("Test.idle".into()),
                params: Some(json!({})),
                result: None,
                error: None,
                session_id: None,
            })
            .unwrap();
        idle_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        conn.off_event("", "Test.idle", idle_id);

        drop(main_frame);
        assert_eq!(
            conn.handler_count(),
            baseline,
            "handler count must return to baseline when MainFrame is dropped"
        );

        let _out_rx = responder2.join().unwrap();
        drop(in_tx);
        drop(context);
        assert_eq!(
            conn.handler_count(),
            0,
            "handler count must return to 0 when context is dropped"
        );
        let mut conn = Arc::try_unwrap(conn).ok().expect("should unwrap Arc");
        conn.wait_closed();
    }
}
