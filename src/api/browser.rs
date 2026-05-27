//! High-level browser handle.
//!
//! [`Browser`] wraps the root session (no `sessionId` on the wire) and
//! provides methods for the `Browser.*` protocol domain. It manages the
//! bootstrap sequence, context creation, and graceful shutdown.

use std::sync::Arc;

use serde_json::json;

use crate::api::context::{BrowserContext, ContextOptions};
pub use crate::protocol::client::{Connection, Session};
use crate::protocol::errors::ProtocolError;

// ---------------------------------------------------------------------------
// ProxyConfig
// ---------------------------------------------------------------------------

/// Proxy server configuration.
///
/// Matches the parameter shape for `Browser.setBrowserProxy` and
/// `Browser.setContextProxy` (PROTOCOL.md Section 6).
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Proxy type: `"http"`, `"https"`, `"socks"`, or `"socks4"`.
    pub r#type: String,
    /// Proxy host.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// List of hosts/domains to bypass the proxy.
    pub bypass: Vec<String>,
    /// Optional proxy username for authentication.
    pub username: Option<String>,
    /// Optional proxy password for authentication.
    pub password: Option<String>,
}

impl ProxyConfig {
    /// Serialize this proxy configuration into a `serde_json::Value` suitable
    /// for sending as protocol params.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut obj = json!({
            "type": self.r#type,
            "host": self.host,
            "port": self.port,
            "bypass": self.bypass,
        });
        if let Some(ref u) = self.username {
            obj["username"] = json!(u);
        }
        if let Some(ref p) = self.password {
            obj["password"] = json!(p);
        }
        obj
    }

    /// Serialize this proxy configuration into a `serde_json::Value` with a
    /// `browserContextId` field, suitable for `Browser.setContextProxy`.
    pub(crate) fn to_context_json(&self, context_id: &str) -> serde_json::Value {
        let mut obj = self.to_json();
        obj["browserContextId"] = json!(context_id);
        obj
    }
}

// ---------------------------------------------------------------------------
// BrowserOptions
// ---------------------------------------------------------------------------

/// Options for the browser bootstrap sequence.
///
/// These control the initial `Browser.enable` call and optional proxy setup.
/// All fields default to sensible values; use `Default::default()` for a
/// minimal configuration.
#[derive(Debug, Clone, Default)]
pub struct BrowserOptions {
    /// Whether to receive `attachedToTarget` events for pages in the
    /// default (non-isolated) browser context. Default: `false`.
    pub attach_to_default_context: bool,

    /// User preferences to set via `Browser.enable`. Each entry is a
    /// `(name, value)` pair where `value` must be a boolean, string, or number.
    pub user_prefs: Vec<(String, serde_json::Value)>,

    /// Optional browser-level proxy to set during bootstrap.
    pub proxy: Option<ProxyConfig>,
}

// ---------------------------------------------------------------------------
// Browser
// ---------------------------------------------------------------------------

/// High-level browser handle.
///
/// Wraps the root session for `Browser.*` commands. Created via
/// [`Browser::connect`], which performs the bootstrap sequence described
/// in PROTOCOL.md Section 12.
///
/// # Lifecycle
///
/// 1. [`connect`](Browser::connect) sends `Browser.enable` + `Browser.getInfo`
///    in parallel, plus an optional `Browser.setBrowserProxy`.
/// 2. Use [`new_context`](Browser::new_context) to create isolated contexts.
/// 3. Call [`close`](Browser::close) to shut down the browser gracefully.
pub struct Browser {
    /// Root session (no `sessionId` on the wire).
    session: Session,
    /// Shared connection reference.
    connection: Arc<Connection>,
    /// User-agent string from `Browser.getInfo`.
    user_agent: Option<String>,
    /// Browser version string from `Browser.getInfo` (e.g., `"Firefox/128.0"`).
    version: Option<String>,
}

impl Browser {
    /// Connect to an existing browser and perform the bootstrap sequence.
    ///
    /// The bootstrap sends the following commands (per PROTOCOL.md Section 12,
    /// Step 3):
    ///
    /// - `Browser.enable` with the configured `attachToDefaultContext` and
    ///   `userPrefs` values.
    /// - `Browser.getInfo` to retrieve the user-agent and version.
    /// - Optionally `Browser.setBrowserProxy` if a proxy is configured.
    ///
    /// These commands are sent sequentially in the current implementation.
    /// The protocol layer supports pipelining internally, and a future
    /// version may exploit that for true parallelism.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if any of the bootstrap commands fail.
    pub fn connect(
        connection: Connection,
        session: Session,
        options: BrowserOptions,
    ) -> Result<Self, ProtocolError> {
        let connection = Arc::new(connection);

        // Build Browser.enable params.
        let enable_params = {
            let prefs: Vec<serde_json::Value> = options
                .user_prefs
                .iter()
                .map(|(name, value)| json!({"name": name, "value": value}))
                .collect();

            let mut p = json!({
                "attachToDefaultContext": options.attach_to_default_context,
            });
            if !prefs.is_empty() {
                p["userPrefs"] = json!(prefs);
            }
            p
        };

        // Step 1: Browser.enable (must succeed before createBrowserContext).
        session.send("Browser.enable", enable_params)?;

        // Step 2: Browser.getInfo (does not require enable).
        let info = session.send("Browser.getInfo", json!({}))?;
        let user_agent = info
            .get("userAgent")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());
        let version = info
            .get("version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned());

        // Step 3: Optional browser-level proxy.
        if let Some(ref proxy) = options.proxy {
            session.send("Browser.setBrowserProxy", proxy.to_json())?;
        }

        // Step 4: Set browser-wide download behavior to "cancel".
        //
        // Without this, a navigation to an attachment-style URL (e.g. a
        // `Content-Disposition: attachment` response) wedges the protocol
        // layer — Juggler diverts the response into a download flow,
        // `Page.navigate` never resolves, and the blocked caller parks
        // forever. By setting `behavior: cancel` we tell the browser to
        // emit `Browser.downloadCreated` (which the connection's reader
        // thread intercepts to unblock the navigate caller with a
        // structured `NavigationBecameDownload` error) but write nothing
        // to disk.
        //
        // `Browser.setDownloadOptions` accepts an optional
        // `browserContextId`; omitting it sets the global default that
        // applies to all contexts that do not override it via
        // [`ContextOptions::download_options`]. Failures here are
        // demoted to a debug log: older browser builds may not understand
        // the call, and download-detection is a defense-in-depth measure
        // rather than a hard requirement for bootstrap.
        let dl_params = json!({
            "downloadOptions": { "behavior": "cancel" }
        });
        if let Err(e) = session.send("Browser.setDownloadOptions", dl_params) {
            log::debug!(
                "Browser.setDownloadOptions(cancel) failed at bootstrap; \
                 download detection may be limited: {e}"
            );
        }

        Ok(Browser {
            session,
            connection,
            user_agent,
            version,
        })
    }

    /// Returns the browser's user-agent string, if available.
    ///
    /// Populated during [`connect`](Browser::connect) from `Browser.getInfo`.
    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    /// Returns the browser version string (e.g., `"Firefox/128.0"`), if available.
    ///
    /// Populated during [`connect`](Browser::connect) from `Browser.getInfo`.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Returns a reference to the shared connection.
    pub fn connection(&self) -> &Arc<Connection> {
        &self.connection
    }

    /// Returns a reference to the root session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Create a new isolated browser context.
    ///
    /// Sends `Browser.createBrowserContext` with `removeOnDetach: true` so that
    /// the context is automatically destroyed when the pipe disconnects.
    ///
    /// The returned [`BrowserContext`] is configured according to `options`
    /// before being returned. If configuration fails, the context is removed
    /// automatically.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if context creation or configuration fails.
    /// `Browser.enable` must have been called first (handled automatically by
    /// [`connect`](Browser::connect)).
    pub fn new_context(&self, options: ContextOptions) -> Result<BrowserContext, ProtocolError> {
        let result = self.session.send(
            "Browser.createBrowserContext",
            json!({ "removeOnDetach": true }),
        )?;

        let context_id = result
            .get("browserContextId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let ctx = BrowserContext::new(context_id, &self.session, Arc::clone(&self.connection));

        // Apply context options. On failure, attempt to clean up the context.
        if let Err(e) = ctx.configure(&options) {
            ctx.try_remove();
            return Err(e);
        }

        Ok(ctx)
    }

    /// Set the browser-level proxy.
    ///
    /// Sends `Browser.setBrowserProxy`. This affects all contexts that do not
    /// have their own context-level proxy configured.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn set_proxy(&self, proxy: &ProxyConfig) -> Result<(), ProtocolError> {
        self.session
            .send("Browser.setBrowserProxy", proxy.to_json())?;
        Ok(())
    }

    /// Clear the global HTTP cache.
    ///
    /// **Warning**: This is a global operation that affects all contexts
    /// (PROTOCOL.md Section 14, item 24).
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn clear_cache(&self) -> Result<(), ProtocolError> {
        self.session.send("Browser.clearCache", json!({}))?;
        Ok(())
    }

    /// Cancel a download by UUID.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the command fails.
    pub fn cancel_download(&self, uuid: &str) -> Result<(), ProtocolError> {
        self.session
            .send("Browser.cancelDownload", json!({ "uuid": uuid }))?;
        Ok(())
    }

    /// Close the browser gracefully.
    ///
    /// Sends `Browser.close` which triggers the shutdown sequence described
    /// in PROTOCOL.md Section 13. The browser will:
    ///
    /// 1. Wait for idle tasks.
    /// 2. Destroy all sessions.
    /// 3. Stop the pipe transport.
    /// 4. Force-quit the process.
    ///
    /// This method consumes `self` to prevent further use after close.
    ///
    /// # Errors
    ///
    /// Returns a [`ProtocolError`] if the close command fails to send.
    /// Note that the response to `Browser.close` (sent with `id=-9999`)
    /// is silently discarded by the protocol layer.
    pub fn close(self) -> Result<(), ProtocolError> {
        // Browser.close must be sent with id=-9999 (PROTOCOL.md §13).
        // Use the connection-level close() which sends directly via
        // transport with BROWSER_CLOSE_MESSAGE_ID, bypassing the
        // session's normal ID allocation. The response is silently
        // discarded by the reader thread.
        self.connection.close()
    }
}

impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browser")
            .field("user_agent", &self.user_agent)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
