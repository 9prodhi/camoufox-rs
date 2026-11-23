//! Instance management for the daemon.
//!
//! Each `Instance` holds a running Camoufox browser with its connection,
//! context, and pages. The `InstanceManager` provides the high-level
//! operations that CLI commands map to.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use serde_json::json;

use crate::api::main_frame::{
    KeyEventParams, MouseEventParams, NavigateOptions, NavigateOutcome, Rect, ScreenshotOptions,
};
use crate::api::selectors::js_query;
use crate::api::{
    Browser, BrowserOptions, ContextOptions, CookieOptions, DialogInfo, DialogPolicy, MainFrame,
};
use crate::config::LaunchConfig;
use crate::protocol::client::Connection;
use crate::protocol::errors::{ProtocolError, ProtocolErrorKind};
use crate::transport::pipe::PipeTransport;

fn validate_upload_paths(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err("invalid argument: at least one upload path is required".into());
    }
    paths
        .iter()
        .map(|path| {
            let file = Path::new(path);
            if !file.is_absolute() {
                return Err(format!(
                    "invalid argument: upload path must be absolute: {path}"
                ));
            }
            let metadata = std::fs::metadata(file)
                .map_err(|e| format!("invalid argument: upload file {path}: {e}"))?;
            if !metadata.is_file() {
                return Err(format!(
                    "invalid argument: upload path is not a regular file: {path}"
                ));
            }
            Ok(file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned())
        })
        .collect()
}

fn screenshot_path(
    instance_id: &str,
    page_id: &str,
    format: Option<&str>,
    explicit: Option<&str>,
) -> Result<String, String> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    let dir = crate::cli::socket::ensure_runtime_dir()?;
    Ok(default_screenshot_path(&dir, instance_id, page_id, format)
        .to_string_lossy()
        .into_owned())
}

fn default_screenshot_path(
    dir: &Path,
    instance_id: &str,
    page_id: &str,
    format: Option<&str>,
) -> PathBuf {
    let ext = if matches!(format, Some("jpeg" | "jpg" | "image/jpeg")) {
        "jpg"
    } else {
        "png"
    };
    dir.join(format!("screenshot-{instance_id}-{page_id}.{ext}"))
}

fn write_screenshot(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("failed to open screenshot: {e}"))?;
    // Also secure an existing file rather than preserving world-readable mode.
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("failed to secure screenshot: {e}"))?;
    file.write_all(bytes)
        .map_err(|e| format!("failed to write screenshot: {e}"))
}

/// `CAMOU_CONFIG_*` variables that pin a plain Firefox User-Agent for the
/// browser at `executable`.
///
/// Camoufox 150+ launched without a fingerprint config sends
/// `... Gecko/20100101 Camoufox/<version>` as its User-Agent, both over HTTP
/// and in `navigator.userAgent`. The major version is read from the
/// `application.ini` shipped next to the executable; if it can't be read,
/// nothing is set. Only the Linux platform token has been verified against
/// Camoufox, so other platforms are left alone. A `CAMOU_CONFIG*` already in
/// `inherited_env_keys` is the user's own fingerprint and wins.
fn user_agent_env(executable: &Path, inherited_env_keys: &[String]) -> HashMap<String, String> {
    if !cfg!(target_os = "linux")
        || inherited_env_keys
            .iter()
            .any(|key| key.starts_with("CAMOU_CONFIG"))
    {
        return HashMap::new();
    }
    let Some(major) = executable
        .parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("application.ini")).ok())
        .and_then(|ini| {
            ini.lines()
                .find_map(|line| line.strip_prefix("Version="))
                .and_then(|version| version.split('.').next())
                .and_then(|major| major.trim().parse::<u32>().ok())
        })
    else {
        return HashMap::new();
    };
    let user_agent =
        format!("Mozilla/5.0 (X11; Linux x86_64; rv:{major}.0) Gecko/20100101 Firefox/{major}.0");
    LaunchConfig::camou_config_env(&json!({ "navigator.userAgent": user_agent }).to_string())
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
    crate::api::main_frame::escape_js_json_literal(s)
}

fn resolve_frame_chain(
    main: String,
    frames: &[String],
    timeout: Duration,
    mut hop: impl FnMut(&str, &str, Duration) -> Result<String, ProtocolError>,
) -> Result<String, ProtocolError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut current = main;
    for selector in frames {
        current = hop(
            &current,
            selector,
            deadline.saturating_duration_since(std::time::Instant::now()),
        )?;
    }
    Ok(current)
}

// Poll interval is separate from the protocol budget: slow frame hops get all
// remaining time, and every deadline exit uses the same user-facing message.
fn poll_selector(
    mut poll: impl FnMut(Duration) -> Result<bool, ProtocolError>,
    selector: &str,
    timeout: Duration,
) -> Result<u128, String> {
    let started = std::time::Instant::now();
    let deadline = started + timeout;
    let mut last_transient = None;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let result = poll(remaining);
        match result {
            Ok(true) => return Ok(started.elapsed().as_millis()),
            Ok(false) => {}
            Err(e) if e.kind == ProtocolErrorKind::ScriptException => {
                return Err(if e.method.as_deref() == Some("Selector.validate") {
                    e.message
                } else {
                    format!("invalid selector {selector:?}: {}", e.message)
                });
            }
            Err(e) if e.kind == ProtocolErrorKind::NotIframe => return Err(e.message),
            Err(e)
                if matches!(
                    e.kind,
                    ProtocolErrorKind::Closed
                        | ProtocolErrorKind::Crashed
                        | ProtocolErrorKind::DialogOpened
                        | ProtocolErrorKind::Transport
                ) =>
            {
                return Err(e.message)
            }
            Err(e)
                if e.kind == ProtocolErrorKind::Timeout && !e.message.starts_with("--frame ") => {}
            Err(e) => {
                last_transient = Some(e.message);
            }
        }
        std::thread::sleep(
            Duration::from_millis(150)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
    let mut message = format!(
        "timed out after {}s waiting for selector: {selector}",
        timeout.as_secs()
    );
    if let Some(error) = last_transient {
        message.push_str(&format!("; last transient error: {error}"));
    }
    Err(message)
}

fn scope_message(frames: &[String], message: String) -> String {
    if let Some(sel) = frames.last() {
        let prefix = format!("--frame {sel}: ");
        if message.starts_with(&prefix) {
            message
        } else {
            format!("{prefix}{message}")
        }
    } else {
        message
    }
}

fn operation_error(frames: &[String], operation: &str, mut error: ProtocolError) -> String {
    error.message = format!("{operation} failed: {}", error.message);
    scoped_error(frames, error)
}

fn dialog_message(instance: &str, page: &str, info: &DialogInfo) -> String {
    format!("page {page} has an open {} dialog: {:?} — run `camoufox dialog {instance} {page} accept|dismiss [--text T]`", info.kind, info.message)
}

fn scoped_error(frames: &[String], error: ProtocolError) -> String {
    let message = if error.kind == ProtocolErrorKind::FrameDetached && !frames.is_empty() {
        "frame was detached".to_owned()
    } else {
        error.message
    };
    scope_message(frames, message)
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
    pub main_frame: Arc<MainFrame>,
    pub(crate) network: Option<super::network::NetworkSession>,
    /// Extra HTTP headers set on this page so far. `Network.setExtraHTTPHeaders`
    /// replaces the whole set on every call, so the CLI keeps the accumulated
    /// list here and re-sends all of them.
    pub headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Instance
// ---------------------------------------------------------------------------

/// Every `camoufox launch` flag, in one place.
///
/// New launch flags add a field here (and on `DaemonRequest::Launch`), so
/// `launch_with`'s signature never changes. Flags that configure the browser
/// context are mapped in `LaunchOptions::context_options`.
#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    /// `None` means headless.
    pub headless: Option<bool>,
    /// Camoufox executable; `None` uses the default install location.
    pub executable: Option<String>,
    /// Directory to save downloads to.
    pub downloads_dir: Option<PathBuf>,
    /// Disable page Content-Security-Policy.
    pub bypass_csp: bool,
    /// Policy for JavaScript dialogs.
    pub dialogs: DialogPolicy,
    /// Persistent profile directory. When `Some`, the directory is used as-is
    /// (created with mode 0700 if absent) and is never deleted on stop.
    /// When `None`, a temporary directory is used and deleted on stop.
    pub profile_dir: Option<String>,
}

impl LaunchOptions {
    /// Browser-context options derived from the launch flags.
    pub(crate) fn context_options(&self) -> ContextOptions {
        ContextOptions {
            download_options: self.downloads_dir.as_ref().map(|dir| {
                crate::api::context::DownloadOptions {
                    behavior: Some("saveToDisk".to_string()),
                    downloads_dir: Some(dir.to_string_lossy().into_owned()),
                }
            }),
            bypass_csp: self.bypass_csp,
            dialog_policy: self.dialogs,
            ..Default::default()
        }
    }
}

/// Result of a trusted user interaction.
#[derive(Debug, Default, serde::Serialize)]
pub struct InteractionOutcome {
    /// Dialog opened by the interaction, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog: Option<DialogInfo>,
}

fn interaction_error(error: ProtocolError, operation: &str) -> Result<InteractionOutcome, String> {
    if error.kind == ProtocolErrorKind::DialogOpened {
        let dialog = error
            .data
            .as_deref()
            .and_then(|data| serde_json::from_str(data).ok());
        Ok(InteractionOutcome { dialog })
    } else {
        Err(format!("{operation} failed: {}", error.message))
    }
}

/// Owns the lifecycle of a browser profile directory.
pub(crate) enum ProfileDir {
    /// Persistent directory: not removed on drop.
    Persistent(PathBuf),
    /// Temporary directory: removed on drop.
    Temp(tempfile::TempDir),
}

impl ProfileDir {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::Persistent(p) => p.as_path(),
            Self::Temp(t) => t.path(),
        }
    }

    fn is_persistent(&self) -> bool {
        matches!(self, Self::Persistent(_))
    }
}

/// Prepares a persistent profile directory:
/// - Must be an absolute path.
/// - Created with mode 0700 if missing.
/// - Existing directory with looser permissions is tightened to 0700 (Unix).
/// - Returns the canonical path.
pub fn prepare_profile_dir(path_str: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path_str);
    if !path.is_absolute() {
        return Err("invalid argument: --profile-dir must be an absolute path".to_string());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(|e| {
                format!("failed to create profile directory {}: {e}", path.display())
            })?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    format!(
                        "failed to set permissions on profile directory {}: {e}",
                        path.display()
                    )
                },
            )?;
        } else {
            let meta = std::fs::metadata(&path).map_err(|e| {
                format!(
                    "failed to inspect profile directory {}: {e}",
                    path.display()
                )
            })?;
            if !meta.is_dir() {
                return Err(format!(
                    "invalid argument: profile directory {} is not a directory",
                    path.display()
                ));
            }
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(
                    |e| {
                        format!(
                            "failed to set permissions on profile directory {}: {e}",
                            path.display()
                        )
                    },
                )?;
            }
        }
    }

    #[cfg(not(unix))]
    {
        if !path.exists() {
            std::fs::create_dir_all(&path).map_err(|e| {
                format!("failed to create profile directory {}: {e}", path.display())
            })?;
        } else if !path.is_dir() {
            return Err(format!(
                "invalid argument: profile directory {} is not a directory",
                path.display()
            ));
        }
    }

    path.canonicalize().map_err(|e| {
        format!(
            "failed to canonicalize profile directory {}: {e}",
            path.display()
        )
    })
}

/// A running Camoufox browser instance managed by the daemon.
pub struct Instance {
    instance_id: String,
    pub browser: Browser,
    pub child: Child,
    pub version: Option<String>,
    pub pid: u32,
    /// Profile directory handle. `Persistent` keeps the directory after stop;
    /// `Temp` deletes it when the handle is dropped.
    profile_dir: ProfileDir,
    profile_reservation: Option<ProfileReservation>,
    /// Canonical path of the profile directory, used for collision detection.
    pub profile_dir_path: Option<PathBuf>,
    pages: HashMap<String, ManagedMainFrame>,
    closed_pages: HashSet<String>,
    closed_pages_order: VecDeque<String>,
    page_counter: u32,
    close_timeout: Duration,
    registry: Arc<crate::api::page_registry::PageRegistry>,
}

impl Instance {
    fn record_closed_page(&mut self, id: String) {
        if self.closed_pages.insert(id.clone()) {
            self.closed_pages_order.push_back(id);
            if self.closed_pages_order.len() > 1024 {
                if let Some(old) = self.closed_pages_order.pop_front() {
                    self.closed_pages.remove(&old);
                }
            }
        }
    }

    fn page_any(&self, id: &str) -> Result<&ManagedMainFrame, String> {
        if self.closed_pages.contains(id) {
            return Err(format!("page {id} was closed"));
        }
        let mp = self
            .pages
            .get(id)
            .ok_or_else(|| format!("page {id} not found"))?;
        if mp.main_frame.is_crashed() {
            return Err(format!("page {id} crashed"));
        }
        if mp.main_frame.is_closed() || self.registry.is_detached(mp.main_frame.target_id()) {
            return Err(format!("page {id} was closed"));
        }
        Ok(mp)
    }

    fn page_checked(&self, id: &str) -> Result<&ManagedMainFrame, ProtocolError> {
        let terminal = |kind, message| ProtocolError::new(kind, Some("page".into()), message);
        if self.closed_pages.contains(id) {
            return Err(terminal(
                ProtocolErrorKind::Closed,
                format!("page {id} was closed"),
            ));
        }
        let mp = self
            .pages
            .get(id)
            .ok_or_else(|| terminal(ProtocolErrorKind::Closed, format!("page {id} not found")))?;
        if mp.main_frame.is_crashed() {
            return Err(terminal(
                ProtocolErrorKind::Crashed,
                format!("page {id} crashed"),
            ));
        }
        if mp.main_frame.is_closed() || self.registry.is_detached(mp.main_frame.target_id()) {
            return Err(terminal(
                ProtocolErrorKind::Closed,
                format!("page {id} was closed"),
            ));
        }
        if let Some(info) = mp.main_frame.pending_dialog() {
            return Err(terminal(
                ProtocolErrorKind::DialogOpened,
                dialog_message(&self.instance_id, id, &info),
            ));
        }
        Ok(mp)
    }

    fn dialog_error(&self, page_id: &str, mut error: ProtocolError) -> ProtocolError {
        if error.kind == ProtocolErrorKind::DialogOpened {
            let info = self
                .pages
                .get(page_id)
                .and_then(|page| page.main_frame.pending_dialog())
                .or_else(|| {
                    error
                        .data
                        .as_deref()
                        .and_then(|data| serde_json::from_str::<DialogInfo>(data).ok())
                });
            if let Some(info) = info {
                error.message = dialog_message(&self.instance_id, page_id, &info);
            }
        }
        error
    }

    fn page(&self, id: &str) -> Result<&ManagedMainFrame, String> {
        self.page_checked(id).map_err(String::from)
    }

    /// Adopt any ready popups opened by this instance's pages.
    pub fn adopt_popups(&mut self, context: &crate::api::BrowserContext) {
        let mut closed_ids = Vec::new();
        for (page_id, mp) in &self.pages {
            if mp.main_frame.is_closed() || self.registry.is_detached(mp.main_frame.target_id()) {
                closed_ids.push(page_id.clone());
            }
        }
        for id in closed_ids {
            self.pages.remove(&id);
            self.record_closed_page(id);
        }

        let policy = context.dialog_policy();
        for main_frame in context.take_popups() {
            main_frame.set_dialog_policy(policy);
            self.page_counter += 1;
            let page_id = format!("p{}", self.page_counter);
            self.pages.insert(
                page_id,
                ManagedMainFrame {
                    main_frame: Arc::new(main_frame),
                    network: None,
                    headers: Vec::new(),
                },
            );
        }
    }

    fn page_mut(&mut self, id: &str) -> Result<&mut ManagedMainFrame, String> {
        self.page(id)?;
        self.pages
            .get_mut(id)
            .ok_or_else(|| format!("page {id} not found"))
    }

    /// Look up the CLI page ID (e.g. "p1") whose main frame target ID matches `target_id`.
    pub fn page_id_for_target(&self, target_id: &str) -> Option<String> {
        self.pages.iter().find_map(|(page_id, mp)| {
            if mp.main_frame.target_id() == target_id
                && !mp.main_frame.is_closed()
                && !self.registry.is_detached(target_id)
            {
                Some(page_id.clone())
            } else {
                None
            }
        })
    }

    /// Create a new page in this instance's context, fully wired with
    /// session, top frame id, and execution context tracking.
    pub fn create_page(&mut self, context: &crate::api::BrowserContext) -> Result<String, String> {
        let main_frame = context
            .new_main_frame()
            .map_err(|e| format!("failed to create page: {}", e.message))?;

        self.page_counter += 1;
        let page_id = format!("p{}", self.page_counter);
        self.pages.insert(
            page_id.clone(),
            ManagedMainFrame {
                main_frame: Arc::new(main_frame),
                network: None,
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
        let mp = self.page(page_id)?;

        let options = NavigateOptions {
            wait_until: wait_until.map(|s| s.to_owned()),
            ..Default::default()
        };

        mp.main_frame
            .navigate(url, options, timeout)
            .map_err(|e| format!("navigate failed: {}", e.message))
    }

    /// Evaluate JavaScript on a page.
    pub fn evaluate(
        &self,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.evaluate_scoped(page_id, expression, false, &[], timeout)
    }

    /// Evaluate an expression, optionally in a child frame or main world.
    pub fn evaluate_scoped(
        &self,
        page_id: &str,
        expression: &str,
        main_world: bool,
        frames: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        if frames.is_empty() {
            if main_world {
                self.evaluate_main_world(page_id, expression, remaining())
            } else {
                self.eval_checked(page_id, expression, remaining())
            }
        } else {
            let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
            let mp = self.page(page_id)?;
            if main_world {
                mp.main_frame
                    .evaluate_main_world_in_frame(&target_frame_id, expression, remaining())
                    .map_err(|e| scoped_error(frames, e))
            } else {
                self.eval_checked_frame(page_id, Some(&target_frame_id), expression, remaining())
                    .map_err(|e| scoped_error(frames, e))
            }
        }
    }

    /// Evaluate a synchronous expression in the page's main world.
    pub fn evaluate_main_world(
        &self,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.page(page_id)?
            .main_frame
            .evaluate_main_world(expression, timeout)
            .map_err(|e| format!("evaluate failed: {}", e.message))
    }

    // -----------------------------------------------------------------------
    // Shared evaluate helpers
    // -----------------------------------------------------------------------

    fn validate_selectors(
        &self,
        page_id: &str,
        selector: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(), ProtocolError> {
        let entries = frames
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(selector))
            .map(|selector| {
                crate::api::selectors::tokenize(selector)
                    .map(|segments| serde_json::json!({"selector":selector, "segments":segments}))
                    .map_err(|error| {
                        ProtocolError::new(
                            ProtocolErrorKind::ScriptException,
                            Some("Selector.validate".into()),
                            format!("invalid selector {selector:?}: {error}"),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let entries = js_literal(&serde_json::to_string(&entries).unwrap());
        let expression = format!("(() => {{ const entries = JSON.parse({entries}); const root = document.createDocumentFragment(); for (const entry of entries) {{ for (const segment of entry.segments) {{ try {{ root.querySelector(segment); }} catch (e) {{ return {{bad:entry.selector, error:String(e)}}; }} }} }} return null; }})()");
        let result = self.eval_checked_frame(page_id, None, &expression, timeout)?;
        if let Some(bad) = result.get("bad").and_then(|v| v.as_str()) {
            return Err(ProtocolError::new(
                ProtocolErrorKind::ScriptException,
                Some("Selector.validate".into()),
                format!(
                    "invalid selector {bad:?}: {}",
                    result["error"].as_str().unwrap_or("invalid CSS")
                ),
            ));
        }
        Ok(())
    }

    fn resolve_frames(
        &self,
        page_id: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, ProtocolError> {
        self.resolve_frames_with_focus(page_id, frames, timeout, false)
    }

    fn resolve_frames_with_focus(
        &self,
        page_id: &str,
        frames: &[String],
        timeout: Duration,
        focus: bool,
    ) -> Result<String, ProtocolError> {
        let mp = self.page_checked(page_id)?;
        resolve_frame_chain(
            mp.main_frame.frame_id(),
            frames,
            timeout,
            |parent, sel, budget| {
                let deadline = std::time::Instant::now() + budget;
                let child = mp.main_frame.resolve_child_frame(parent, sel, budget)?;
                if focus {
                    let q = js_query(sel);
                    self.eval_checked_frame(page_id, Some(parent), &format!("(() => {{ const el = {q}; if (!el) throw new Error('iframe disappeared'); el.focus(); }})()"), deadline.saturating_duration_since(std::time::Instant::now()))
                    .map_err(|mut e| {e.message = format!("--frame {sel}: {}", e.message); e})?;
                }
                Ok(child)
            },
        )
    }

    fn focus_frame(
        &self,
        page_id: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(), String> {
        if frames.is_empty() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + timeout;
        let fid = self.resolve_frames_with_focus(page_id, frames, timeout, true)?;
        let expression = "(() => { window.focus(); const el = document.activeElement; return !!el && el !== document.body && el !== document.documentElement; })()";
        let active = self
            .eval_checked_frame(
                page_id,
                Some(&fid),
                expression,
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .map_err(|e| scoped_error(frames, e))?;
        if active != true {
            return Err(format!("--frame {}: nothing is focused inside the frame; click or fill an element inside it first", frames.last().unwrap()));
        }
        Ok(())
    }

    fn eval_checked_frame(
        &self,
        page_id: &str,
        frame_id: Option<&str>,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, ProtocolError> {
        let mp = self.page_checked(page_id)?;
        let evaluated = match frame_id {
            Some(fid) => mp.main_frame.evaluate_in_frame(fid, expression, timeout),
            None => mp.main_frame.evaluate(expression, timeout),
        };

        let raw = evaluated.map_err(|mut e| {
            if e.kind == ProtocolErrorKind::DialogOpened {
                e = self.dialog_error(page_id, e);
            } else {
                e.message = format!("evaluate failed: {}", e.message);
            }
            e
        })?;

        if let Some(exc) = raw.get("exceptionDetails") {
            return Err(crate::api::selectors::selector_exception(exc));
        }

        Ok(raw
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

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
        self.eval_checked_frame(page_id, None, expression, timeout)
            .map_err(String::from)
    }

    /// Resolve a CSS selector to its viewport-space bounding box, scrolling it
    /// into view first. Also returns the page's scroll offsets so callers can
    /// convert to document coordinates.
    fn element_rect(
        &self,
        page_id: &str,
        selector: &str,
        frame_id: Option<&str>,
        timeout: Duration,
    ) -> Result<(Rect, f64, f64), ProtocolError> {
        let q = js_query(selector);
        let expr = format!(
            "(() => {{ const el = {q}; if (!el) return null; \
             try {{ el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}}); }} \
             catch (e) {{ el.scrollIntoView(); }} \
             const r = el.getBoundingClientRect(); \
             return [r.x, r.y, r.width, r.height, window.scrollX, window.scrollY]; }})()"
        );
        let value = self.eval_checked_frame(page_id, frame_id, &expr, timeout)?;
        let arr = value.as_array().ok_or_else(|| {
            ProtocolError::response_msg(
                "Runtime.evaluate",
                format!("selector not found: {selector}"),
            )
        })?;
        let n = |i: usize| arr.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rect = Rect {
            x: n(0),
            y: n(1),
            width: n(2),
            height: n(3),
        };
        if rect.width <= 0.0 && rect.height <= 0.0 {
            return Err(ProtocolError::response_msg(
                "Runtime.evaluate",
                format!("element {selector} has a zero-size box (hidden or not laid out)"),
            ));
        }
        Ok((rect, n(4), n(5)))
    }

    // -----------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------

    /// Extract the page's rendered text (`innerText`), optionally scoped to a
    /// CSS selector.
    pub fn text(
        &self,
        page_id: &str,
        selector: Option<&str>,
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
        let frame_arg = if frames.is_empty() {
            None
        } else {
            Some(target_frame_id.as_str())
        };
        let expr = match selector {
            Some(sel) => {
                let q = js_query(sel);
                format!(
                    "(() => {{ const el = {q}; \
                     return el ? el.innerText : null; }})()"
                )
            }
            None => "(() => (document.body || document.documentElement).innerText)()".to_string(),
        };
        match self
            .eval_checked_frame(page_id, frame_arg, &expr, remaining())
            .map_err(|e| scoped_error(frames, e))?
        {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Null => Err(scope_message(
                frames,
                match selector {
                    Some(sel) => format!("selector not found: {sel}"),
                    None => "page has no text content".to_string(),
                },
            )),
            other => Ok(other.to_string()),
        }
    }

    /// Extract page HTML: `outerHTML` of the selector match, or the whole
    /// document when no selector is given.
    pub fn html(
        &self,
        page_id: &str,
        selector: Option<&str>,
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
        let frame_arg = if frames.is_empty() {
            None
        } else {
            Some(target_frame_id.as_str())
        };
        let expr = match selector {
            Some(sel) => {
                let q = js_query(sel);
                format!(
                    "(() => {{ const el = {q}; \
                     return el ? el.outerHTML : null; }})()"
                )
            }
            None => "document.documentElement.outerHTML".to_string(),
        };
        match self
            .eval_checked_frame(page_id, frame_arg, &expr, remaining())
            .map_err(|e| scoped_error(frames, e))?
        {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Null => Err(scope_message(
                frames,
                match selector {
                    Some(sel) => format!("selector not found: {sel}"),
                    None => "page has no document element".to_string(),
                },
            )),
            other => Ok(other.to_string()),
        }
    }

    /// Collect every `<a href>` on the page (or inside `selector`) as
    /// `{text, href}` objects. Whitespace in the link text is collapsed.
    pub fn links(
        &self,
        page_id: &str,
        selector: Option<&str>,
        frames: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
        let frame_arg = if frames.is_empty() {
            None
        } else {
            Some(target_frame_id.as_str())
        };
        let root = match selector {
            Some(sel) => js_query(sel),
            None => "document".to_string(),
        };
        let expr = format!(
            "(() => {{ const root = {root}; if (!root) return null; \
             return Array.from(root.querySelectorAll('a[href]')).map(a => ({{ \
             text: (a.innerText || a.textContent || '').trim().replace(/\\s+/g, ' '), \
             href: a.href }})); }})()"
        );
        match self
            .eval_checked_frame(page_id, frame_arg, &expr, remaining())
            .map_err(|e| scoped_error(frames, e))?
        {
            serde_json::Value::Null => Err(scope_message(
                frames,
                match selector {
                    Some(sel) => format!("selector not found: {sel}"),
                    None => "no links found".to_string(),
                },
            )),
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
        let mp = self.page(page_id)?;
        mp.main_frame
            .go_back()
            .map_err(|e| format!("go back failed: {}", e.message))
    }

    /// Go forward one session-history entry. Returns `false` if there was none.
    pub fn go_forward(&self, page_id: &str) -> Result<bool, String> {
        let mp = self.page(page_id)?;
        mp.main_frame
            .go_forward()
            .map_err(|e| format!("go forward failed: {}", e.message))
    }

    /// Reload the page.
    pub fn reload(&self, page_id: &str) -> Result<(), String> {
        let mp = self.page(page_id)?;
        mp.main_frame
            .reload()
            .map_err(|e| format!("reload failed: {}", e.message))
    }

    /// Poll until `selector` matches an element, or `timeout` elapses.
    ///
    /// Returns the elapsed milliseconds on success.
    pub fn wait_for_selector(
        &self,
        page_id: &str,
        selector: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<u128, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let mut validated = false;
        let expr = format!("!!({})", js_query(selector));
        poll_selector(
            |remaining| {
                let result = (|| {
                    self.page_checked(page_id)?;
                    if self.browser.connection().rejects_sends() {
                        return Err(ProtocolError::closed(None));
                    }
                    let deadline = std::time::Instant::now() + remaining;
                    if !validated {
                        self.validate_selectors(page_id, selector, frames, remaining)?;
                        validated = true;
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let fid = if frames.is_empty() {
                        None
                    } else {
                        Some(self.resolve_frames(page_id, frames, remaining)?)
                    };
                    self.eval_checked_frame(
                        page_id,
                        fid.as_deref(),
                        &expr,
                        deadline.saturating_duration_since(std::time::Instant::now()),
                    )
                    .map_err(|mut e| {
                        if let Some(sel) = frames.last() {
                            e.message = format!("--frame {sel}: {}", e.message);
                        }
                        e
                    })
                    .map(|v| v == true)
                })();
                result.map_err(|e| self.dialog_error(page_id, e))
            },
            selector,
            timeout,
        )
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
        let mp = self.page_mut(page_id)?;

        mp.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        mp.headers.push((name.to_string(), value.to_string()));

        let borrowed: Vec<(&str, &str)> = mp
            .headers
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        mp.main_frame
            .set_extra_http_headers(&borrowed)
            .map_err(|e| format!("set header failed: {}", e.message))?;

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
        frames: &[String],
        timeout: Duration,
    ) -> Result<(i32, i32, InteractionOutcome), String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let (click_x, click_y) = self.pointer_position(page_id, selector, frames, timeout)?;
        let outcome = self
            .click_with_timeout(
                page_id,
                click_x,
                click_y,
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .map_err(|e| scope_message(frames, e))?;
        Ok((click_x, click_y, outcome))
    }

    /// Move the mouse over the element matching `selector` without clicking.
    pub fn hover(
        &self,
        page_id: &str,
        selector: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let (x, y) = self.pointer_position(page_id, selector, frames, timeout)?;

        let mp = self.page(page_id)?;
        mp.main_frame
            .dispatch_mouse_event_with_timeout(
                MouseEventParams {
                    r#type: "mousemove".to_string(),
                    button: 0,
                    buttons: 0,
                    x,
                    y,
                    modifiers: 0,
                    click_count: None,
                },
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .map_err(|mut e| {
                e.message = format!("hover failed: {}", e.message);
                scoped_error(frames, e)
            })?;
        Ok((x, y))
    }

    // Scroll outermost-first, then the target. Only afterwards measure every hop.
    fn pointer_position(
        &self,
        page_id: &str,
        selector: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let mp = self.page(page_id)?;
        let mut fid = mp.main_frame.frame_id();
        let mut hops = Vec::new();
        for sel in frames {
            let q = js_query(sel);
            let rendered = self.eval_checked_frame(page_id, Some(&fid), &format!("(() => {{ const el = {q}; if(el) {{ if (!el.offsetWidth || !el.offsetHeight || !el.getClientRects().length) return false; el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}}); }} return true; }})()"), remaining())
                .map_err(|e| scoped_error(std::slice::from_ref(sel), e))?;
            if rendered == false {
                return Err(format!("--frame {sel}: iframe is not rendered"));
            }
            let child = mp
                .main_frame
                .resolve_child_frame(&fid, sel, remaining())
                .map_err(|e| scoped_error(std::slice::from_ref(sel), e))?;
            hops.push((fid, sel));
            fid = child;
        }
        let (rect, _, _) = self
            .element_rect(
                page_id,
                selector,
                if frames.is_empty() { None } else { Some(&fid) },
                remaining(),
            )
            .map_err(|e| scoped_error(frames, e))?;
        let (mut x, mut y) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
        for (parent, sel) in hops.iter().rev() {
            let q = js_query(sel);
            let expr = format!("(() => {{ const el = {q}; if (!el) return null; const r = el.getBoundingClientRect(); if (!el.offsetWidth || !el.offsetHeight || !r.width || !r.height) return 'not-rendered'; const cs = getComputedStyle(el); const sx = r.width / el.offsetWidth, sy = r.height / el.offsetHeight; return [r.left + ((el.clientLeft || 0) + (parseFloat(cs.paddingLeft) || 0)) * sx, r.top + ((el.clientTop || 0) + (parseFloat(cs.paddingTop) || 0)) * sy, sx, sy]; }})()");
            let v = self
                .eval_checked_frame(page_id, Some(parent), &expr, remaining())
                .map_err(|e| scoped_error(std::slice::from_ref(*sel), e))?;
            if v == "not-rendered" {
                return Err(format!("--frame {sel}: iframe is not rendered"));
            }
            let a = v
                .as_array()
                .ok_or_else(|| format!("--frame {sel}: frame was detached"))?;
            let n = |i: usize| a[i].as_f64().unwrap_or(0.0);
            x = n(0) + x * n(2);
            y = n(1) + y * n(3);
        }
        Ok((x.round() as i32, y.round() as i32))
    }

    /// Insert text into whichever element currently has focus.
    pub fn insert_text(
        &self,
        page_id: &str,
        text: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(), String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let mp = self.page(page_id)?;
        self.focus_frame(page_id, frames, remaining())?;
        mp.main_frame
            .insert_text_with_timeout(text, remaining())
            .map_err(|mut e| {
                e.message = format!("type failed: {}", e.message);
                scoped_error(frames, e)
            })
    }

    /// Focus the element matching `selector`, clear it, then type `value`.
    ///
    /// Returns the tag name of the element that was filled.
    pub fn fill(
        &self,
        page_id: &str,
        selector: &str,
        value: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
        let frame_arg = if frames.is_empty() {
            None
        } else {
            Some(target_frame_id.as_str())
        };
        let q = js_query(selector);
        let expr = format!(
            "(() => {{ const el = {q}; if (!el) return null; \
             try {{ el.scrollIntoView({{block:'center', behavior:'instant'}}); }} \
             catch (e) {{ el.scrollIntoView(); }} \
             el.focus(); \
             if ('value' in el) {{ el.value = ''; \
               el.dispatchEvent(new Event('input', {{bubbles: true}})); }} \
             else if (el.isContentEditable) {{ el.textContent = ''; }} \
             return el.tagName.toLowerCase(); }})()"
        );
        let tag = match self
            .eval_checked_frame(page_id, frame_arg, &expr, remaining())
            .map_err(|e| scoped_error(frames, e))?
        {
            serde_json::Value::String(s) => s,
            _ => {
                return Err(scope_message(
                    frames,
                    format!("selector not found: {selector}"),
                ))
            }
        };
        self.page(page_id)?
            .main_frame
            .insert_text_with_timeout(value, remaining())
            .map_err(|mut e| {
                e.message = format!("type failed: {}", e.message);
                scoped_error(frames, e)
            })?;
        Ok(tag)
    }

    /// Set files on a file input, including hidden inputs and scoped selectors.
    fn upload(
        &self,
        page_id: &str,
        selector: &str,
        paths: &[String],
        frames: &[String],
        timeout: Duration,
    ) -> Result<(), String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
        let q = js_query(selector);
        // Return either a validation error or the actual node so validation and
        // handle creation refer to the same element, without measuring layout.
        let expr = format!(
            "(() => {{ const el = {q}; if (!el) return null; \
             const tag = el.tagName.toLowerCase(); const type = el.type || el.getAttribute('type') || ''; \
             if (tag !== 'input' || type !== 'file') \
               return 'invalid argument: element is not a file input (' + tag + '[type=' + type + '])'; \
             if ({count} > 1 && !el.multiple) \
               return 'invalid argument: input does not accept multiple files'; \
             return el; }})()",
            count = paths.len(),
        );
        let main_frame = &self.page(page_id)?.main_frame;
        let raw = main_frame
            .evaluate_in_frame_raw(&target_frame_id, &expr, false, remaining())
            .map_err(|e| scoped_error(frames, e))?;
        if let Some(exc) = raw.get("exceptionDetails") {
            return Err(scoped_error(
                frames,
                crate::api::selectors::selector_exception(exc),
            ));
        }
        if let Some(error) = raw.pointer("/result/value").and_then(|v| v.as_str()) {
            return Err(error.to_owned());
        }
        let object_id = raw
            .pointer("/result/objectId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("selector not found: {selector}"))?;
        let context_id = main_frame
            .frame_ctx(&target_frame_id)
            .and_then(|t| t.current());
        let files: Vec<&str> = paths.iter().map(String::as_str).collect();
        let result = main_frame.set_file_input_files_with_timeout(
            &target_frame_id,
            object_id,
            &files,
            remaining(),
        );
        // Release the handle on failure too; preserve the upload result if the
        // page navigates or closes while processing its change listener.
        if let Some(context_id) = context_id {
            let _ = main_frame.dispose_object_with_timeout(&context_id, object_id, remaining());
        }
        result.map_err(|e| scoped_error(frames, e))
    }

    /// Press a named key (keydown followed by keyup).
    pub fn press(
        &self,
        page_id: &str,
        key: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<InteractionOutcome, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let (code, key_name, key_code) = key_descriptor(key)?;
        let mp = self.page(page_id)?;
        self.focus_frame(page_id, frames, remaining())?;

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

        if let Err(e) = mp
            .main_frame
            .dispatch_key_event_with_timeout(make("keydown"), remaining())
        {
            if e.kind == ProtocolErrorKind::DialogOpened {
                return interaction_error(e, "press (keydown)");
            }
            return Err(operation_error(frames, "press (keydown)", e));
        }
        if let Err(e) = mp
            .main_frame
            .dispatch_key_event_with_timeout(make("keyup"), remaining())
        {
            if e.kind == ProtocolErrorKind::DialogOpened {
                return interaction_error(e, "press (keyup)");
            }
            return Err(operation_error(frames, "press (keyup)", e));
        }
        Ok(InteractionOutcome {
            dialog: mp.main_frame.pending_dialog(),
        })
    }

    /// Choose an option in a `<select>` by value, label, or visible text.
    pub fn select_option(
        &self,
        page_id: &str,
        selector: &str,
        value: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let target_frame_id = self.resolve_frames(page_id, frames, remaining())?;
        let frame_arg = if frames.is_empty() {
            None
        } else {
            Some(target_frame_id.as_str())
        };
        let q = js_query(selector);
        let expr = format!(
            "(() => {{ const el = {q}; \
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
            val = js_literal(value)
        );
        let result = self
            .eval_checked_frame(page_id, frame_arg, &expr, remaining())
            .map_err(|e| scoped_error(frames, e))?;
        if result.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let reason = result
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("select failed");
            return Err(scope_message(
                frames,
                format!("select {selector}: {reason}"),
            ));
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
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let expr = match selector {
            Some(sel) => {
                let q = js_query(sel);
                format!(
                    "(() => {{ const el = {q}; if (!el) return null; \
                     try {{ el.scrollIntoView({{block:'center', inline:'center', behavior:'instant'}}); }} \
                     catch (e) {{ el.scrollIntoView(); }} \
                     return {{scrollX: window.scrollX, scrollY: window.scrollY, \
                       scrollHeight: document.documentElement.scrollHeight}}; }})()"
                )
            }
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
            .map(|page_id| {
                // Metadata remains available even when page_any rejects commands
                // because the renderer crashed.
                let mp = self.pages.get(&page_id);
                let dialog = mp.as_ref().and_then(|p| p.main_frame.pending_dialog());
                let opener = mp.as_ref().and_then(|p| {
                    p.main_frame
                        .opener_id()
                        .and_then(|tid| self.page_id_for_target(tid))
                });
                let mut tab = match self.url(&page_id, timeout) {
                    Ok((url, title)) => json!({"page_id": page_id, "url": url, "title": title}),
                    Err(_) => json!({"page_id": page_id, "url": null, "title": null}),
                };
                tab["opener"] = json!(opener);
                if mp.is_some_and(|p| p.main_frame.is_crashed()) {
                    tab["crashed"] = json!(true);
                }
                if let Some(dialog) = dialog {
                    tab["dialog"] = json!(dialog);
                }
                tab
            })
            .collect()
    }

    /// Close a page and forget it.
    pub fn close_tab(&mut self, page_id: &str) -> Result<(), String> {
        if self.closed_pages.contains(page_id) {
            return Err(format!("page {page_id} was closed"));
        }
        let mp = self
            .pages
            .remove(page_id)
            .ok_or_else(|| format!("page {page_id} not found"))?;
        self.record_closed_page(page_id.to_string());
        if mp.main_frame.is_crashed() {
            let _ = mp.main_frame.close();
            return Ok(());
        }
        if mp.main_frame.is_closed() || self.registry.is_detached(mp.main_frame.target_id()) {
            return Err(format!("page {page_id} was closed"));
        }
        mp.main_frame
            .close()
            .map_err(|e| format!("close tab failed: {}", e.message))
    }

    /// Dispatch a trusted left-click (mousemove → mousedown → mouseup) at
    /// viewport coordinates (x, y). Trusted browser-level input, so it drives
    /// cross-origin / closed-shadow widgets (e.g. Cloudflare Turnstile) that
    /// synthetic JS events cannot reach.
    pub fn click(&self, page_id: &str, x: i32, y: i32) -> Result<InteractionOutcome, String> {
        self.click_with_timeout(page_id, x, y, Duration::from_secs(60))
    }

    fn click_with_timeout(
        &self,
        page_id: &str,
        x: i32,
        y: i32,
        timeout: Duration,
    ) -> Result<InteractionOutcome, String> {
        let deadline = std::time::Instant::now() + timeout;
        let remaining = || deadline.saturating_duration_since(std::time::Instant::now());
        let mp = self.page(page_id)?;

        let mv = MouseEventParams {
            r#type: "mousemove".to_string(),
            button: 0,
            buttons: 0,
            x,
            y,
            modifiers: 0,
            click_count: None,
        };
        if let Err(e) = mp
            .main_frame
            .dispatch_mouse_event_with_timeout(mv, remaining())
        {
            return interaction_error(e, "click (move)");
        }

        let down = MouseEventParams {
            r#type: "mousedown".to_string(),
            button: 0,
            buttons: 1,
            x,
            y,
            modifiers: 0,
            click_count: Some(1),
        };
        if let Err(e) = mp
            .main_frame
            .dispatch_mouse_event_with_timeout(down, remaining())
        {
            return interaction_error(e, "click (down)");
        }

        let up = MouseEventParams {
            r#type: "mouseup".to_string(),
            button: 0,
            buttons: 0,
            x,
            y,
            modifiers: 0,
            click_count: Some(1),
        };
        if let Err(e) = mp
            .main_frame
            .dispatch_mouse_event_with_timeout(up, remaining())
        {
            return interaction_error(e, "click (up)");
        }

        Ok(InteractionOutcome {
            dialog: mp.main_frame.pending_dialog(),
        })
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
        let _selectors = crate::api::selectors::SelectorScope::enter();
        let mp = self.page_any(page_id)?;

        let rect = match (clip, selector) {
            (Some([x, y, width, height]), _) => Rect {
                x,
                y,
                width,
                height,
            },
            (None, Some(sel)) => {
                let (r, scroll_x, scroll_y) = self.element_rect(page_id, sel, None, timeout)?;
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
            .map_err(|e| format!("screenshot failed: {}", e.message))?;

        let out_path = screenshot_path(&self.instance_id, page_id, Some(mime), path)?;
        write_screenshot(Path::new(&out_path), &bytes)?;

        Ok((bytes, out_path, rect))
    }

    /// Get page IDs for active (non-detached) pages.
    pub fn page_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .pages
            .iter()
            .filter(|(_, mp)| {
                !mp.main_frame.is_closed() && !self.registry.is_detached(mp.main_frame.target_id())
            })
            .map(|(k, _)| k.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Shut down this instance. Browser.close is followed by pipe EOF, which makes Camoufox exit.
    pub fn stop(self) -> Result<(), String> {
        let Instance {
            browser,
            mut child,
            close_timeout,
            profile_dir,
            profile_reservation,
            ..
        } = self;
        let _ = browser.close();
        let result = crate::process::lifecycle::graceful_shutdown(&mut child, close_timeout)
            .map_err(|e| format!("failed to stop browser: {e}"));
        // Keep the profile alive through kill + wait, including error paths.
        drop(profile_dir);
        drop(profile_reservation);
        result
    }

    /// Returns the canonical profile directory path if persistent, else `None`.
    pub fn persistent_profile_dir(&self) -> Option<&std::path::Path> {
        if self.profile_dir.is_persistent() {
            self.profile_dir_path.as_deref()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// InstanceManager
// ---------------------------------------------------------------------------

/// One daemon-managed instance. Everything that must be reachable WITHOUT
/// taking the per-instance mutex (so it can interrupt a running command)
/// lives beside `managed`, not inside it.
#[derive(Clone)]
struct InstanceEntry {
    managed: Arc<Mutex<ManagedInstance>>,
    /// Closes the protocol connection: rejects in-flight requests and makes
    /// Camoufox exit (pipe EOF). Used by stop/shutdown.
    closer: Arc<Connection>,
    /// Download events for this instance's context. `downloads --wait`
    /// blocks on it without taking the instance mutex.
    downloads: Arc<crate::cli::downloads::DownloadLog>,
}

// A lease spans startup, registration and shutdown. Dropping it releases the
// canonical path even when spawn, readiness or protocol bootstrap fails.
struct ProfileReservation {
    profiles: Arc<Mutex<HashMap<PathBuf, Option<String>>>>,
    path: PathBuf,
}

impl ProfileReservation {
    fn reserve(
        profiles: &Arc<Mutex<HashMap<PathBuf, Option<String>>>>,
        path: &Path,
    ) -> Result<Self, String> {
        let mut owners = profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(owner) = owners.get(path) {
            return Err(match owner {
                Some(id) => {
                    format!("invalid argument: profile directory already in use by instance {id}")
                }
                None => {
                    "invalid argument: profile directory already in use by a launch in progress"
                        .into()
                }
            });
        }
        owners.insert(path.to_owned(), None);
        Ok(Self {
            profiles: Arc::clone(profiles),
            path: path.to_owned(),
        })
    }

    fn bind(&self, id: &str) {
        self.profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.path.clone(), Some(id.to_owned()));
    }
}

impl Drop for ProfileReservation {
    fn drop(&mut self) {
        self.profiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.path);
    }
}

/// Manages all browser instances for the daemon.
#[derive(Default)]
pub struct InstanceManager {
    instances: Mutex<HashMap<String, InstanceEntry>>,
    profiles: Arc<Mutex<HashMap<PathBuf, Option<String>>>>,
    closing: AtomicBool,
    counter: AtomicU64,
}

struct ManagedInstance {
    instance: Option<Instance>,
    context: crate::api::BrowserContext,
    /// Keeps the download-log event subscriptions alive for the instance.
    _download_guards: Vec<crate::api::main_frame::SubscriptionGuard>,
}

impl InstanceManager {
    pub fn new() -> Self {
        InstanceManager {
            instances: Mutex::new(HashMap::new()),
            profiles: Arc::default(),
            counter: AtomicU64::new(0),
            closing: AtomicBool::new(false),
        }
    }

    /// Launch a new browser instance. Retains the mutable receiver for existing
    /// callers; daemon workers use the shared-receiver implementation.
    pub fn launch(
        &mut self,
        headless: Option<bool>,
        executable: Option<&str>,
    ) -> Result<(String, Option<String>, u32), String> {
        self.launch_with(&LaunchOptions {
            headless,
            executable: executable.map(str::to_owned),
            ..Default::default()
        })
    }

    /// Launch a new browser instance from the full set of `launch` flags.
    pub fn launch_with(
        &self,
        options: &LaunchOptions,
    ) -> Result<(String, Option<String>, u32), String> {
        if self.is_closing() {
            return Err("daemon shutting down".into());
        }
        if let Some(ref dir) = options.downloads_dir {
            if !dir.is_absolute() {
                return Err("--downloads-dir must be an absolute path".to_string());
            }
            if !dir.exists() {
                std::fs::create_dir_all(dir).map_err(|e| {
                    format!("failed to create downloads dir {}: {e}", dir.display())
                })?;
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            let probe = dir.join(format!(".camoufox-probe-{}", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)
            {
                Ok(file) => {
                    drop(file);
                    let _ = std::fs::remove_file(&probe);
                }
                Err(e) => {
                    return Err(format!(
                        "downloads dir {} is not writable: {e}",
                        dir.display()
                    ));
                }
            }
        }

        let instance_id = format!("{:08x}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);
        let (profile_dir, profile_dir_path, profile_reservation) = match &options.profile_dir {
            Some(dir_str) => {
                let canonical = prepare_profile_dir(dir_str)?;
                let reservation = ProfileReservation::reserve(&self.profiles, &canonical)?;
                (
                    ProfileDir::Persistent(canonical.clone()),
                    Some(canonical),
                    Some(reservation),
                )
            }
            None => {
                let temp =
                    tempfile::tempdir().map_err(|e| format!("failed to create temp dir: {e}"))?;
                let canonical = temp.path().canonicalize().ok();
                (ProfileDir::Temp(temp), canonical, None)
            }
        };
        let headless = options.headless;

        let executable = options
            .executable
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(crate::config::default_executable);
        let inherited_env_keys: Vec<String> = std::env::vars_os()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        let config = LaunchConfig {
            env: user_agent_env(&executable, &inherited_env_keys),
            executable,
            profile_dir: Some(profile_dir.path().to_owned()),
            persistent: profile_dir.is_persistent(),
            headless: headless.unwrap_or(true),
            close_timeout: Duration::from_secs(10),
            ..Default::default()
        };

        let launched = crate::process::unix::spawn(&config)
            .map_err(|e| format!("failed to spawn browser: {e}"))?;

        let mut child = crate::process::ChildGuard::new(launched.child);
        let pid = child.child_mut().id();

        let _ = crate::process::readiness::wait_for_ready(child.child_mut(), config.timeout)
            .map_err(|e| format!("browser did not become ready: {e}"))?;

        let transport = PipeTransport::new(launched.command_pipe, launched.response_pipe);
        let conn = Connection::new(Box::new(transport));
        let session = conn.root_session();
        let browser = Browser::connect(
            conn,
            session,
            BrowserOptions {
                attach_to_default_context: profile_dir.is_persistent(),
                ..Default::default()
            },
        )
        .map_err(|e| format!("bootstrap failed: {}", e.message))?;

        let version = browser.version().map(|s| s.to_owned());

        // Persistent profiles store browsing data in Firefox's default context.
        // Temporary profiles keep using an isolated, remove-on-detach context.
        let context = if profile_dir.is_persistent() {
            browser.default_context(options.context_options())
        } else {
            browser.new_context(options.context_options())
        }
        .map_err(|e| format!("failed to create context: {}", e.message))?;

        let closer = Arc::clone(browser.connection());
        let download_log = crate::cli::downloads::DownloadLog::new(
            context.id().to_string(),
            options.downloads_dir.clone(),
        );
        let download_guards = download_log.subscribe(browser.connection());

        let instance = Instance {
            instance_id: instance_id.clone(),
            browser,
            child: child.into_child(),
            version: version.clone(),
            pid,
            profile_dir,
            profile_reservation,
            profile_dir_path,
            pages: HashMap::new(),
            closed_pages: HashSet::new(),
            closed_pages_order: VecDeque::new(),
            page_counter: 0,
            close_timeout: config.close_timeout,
            registry: Arc::clone(context.registry()),
        };

        let mut instances = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_closing() {
            drop(instances);
            let _ = instance.stop();
            return Err("daemon shutting down".into());
        }
        if let Some(reservation) = &instance.profile_reservation {
            reservation.bind(&instance_id);
        }
        instances.insert(
            instance_id.clone(),
            InstanceEntry {
                managed: Arc::new(Mutex::new(ManagedInstance {
                    instance: Some(instance),
                    context,
                    _download_guards: download_guards,
                })),
                closer,
                downloads: download_log,
            },
        );

        Ok((instance_id, version, pid))
    }

    /// List all instances.
    pub fn list(&self) -> Vec<serde_json::Value> {
        let mut result = Vec::new();
        let entries: Vec<_> = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(id, e)| (id.clone(), Arc::clone(&e.managed)))
            .collect();
        for (id, entry) in entries {
            let entry = entry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(inst) = &entry.instance {
                let mut item = json!({
                    "instance_id": id,
                    "pid": inst.pid,
                    "version": inst.version,
                    "pages": inst.page_ids(),
                });
                if let Some(profile_dir) = inst.persistent_profile_dir() {
                    item["profile_dir"] = json!(profile_dir.to_string_lossy());
                }
                result.push(item);
            }
        }
        result.sort_by(|a, b| {
            a.get("instance_id")
                .and_then(|v| v.as_str())
                .cmp(&b.get("instance_id").and_then(|v| v.as_str()))
        });
        result
    }

    /// Stop an instance.
    pub fn stop(&self, instance_id: &str) -> Result<(), String> {
        let InstanceEntry {
            managed: entry,
            closer,
            ..
        } = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(instance_id)
            .ok_or_else(|| Self::missing(instance_id))?;
        // Interrupt any command running on this instance first, so the
        // instance mutex below frees promptly.
        let _ = closer.close();
        let inst = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .instance
            .take()
            .ok_or_else(|| Self::missing(instance_id))?;
        inst.stop()
    }

    /// Create a new page in an instance.
    pub fn new_page(&self, instance_id: &str) -> Result<String, String> {
        self.with_instance(instance_id, |inst, context| inst.create_page(context))
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
        self.with_instance(instance_id, |inst, _| {
            inst.navigate(page_id, url, timeout, wait_until)
        })
    }

    /// Evaluate JavaScript.
    pub fn evaluate(
        &self,
        instance_id: &str,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.evaluate_scoped(instance_id, page_id, expression, false, &[], timeout)
    }

    /// Evaluate a synchronous expression in the page's main world.
    pub fn evaluate_main_world(
        &self,
        instance_id: &str,
        page_id: &str,
        expression: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.evaluate_scoped(instance_id, page_id, expression, true, &[], timeout)
    }

    /// Evaluate JavaScript, optionally scoped to frames and main world.
    pub fn evaluate_scoped(
        &self,
        instance_id: &str,
        page_id: &str,
        expression: &str,
        main_world: bool,
        frames: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.evaluate_scoped(page_id, expression, main_world, frames, timeout)
        })
    }

    /// Dispatch a trusted left-click at viewport coordinates (x, y).
    pub fn click(
        &self,
        instance_id: &str,
        page_id: &str,
        x: i32,
        y: i32,
    ) -> Result<InteractionOutcome, String> {
        self.with_instance(instance_id, |inst, _| inst.click(page_id, x, y))
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
        self.with_instance(instance_id, |inst, _| {
            inst.screenshot(page_id, format, quality, path, selector, clip, timeout)
        })
    }

    fn missing(id: &str) -> String {
        format!("instance {id} not found")
    }

    /// Snapshot the entry under the map lock, then lock only this instance.
    fn with_instance<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Instance, &crate::api::BrowserContext) -> Result<T, String>,
    ) -> Result<T, String> {
        let entry = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|e| Arc::clone(&e.managed))
            .ok_or_else(|| Self::missing(id))?;
        let mut entry = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ManagedInstance {
            instance, context, ..
        } = &mut *entry;
        let inst = instance.as_mut().ok_or_else(|| Self::missing(id))?;
        inst.adopt_popups(context);
        f(inst, context)
    }

    /// Begin passive HTTP recording for a page into an empty absolute directory.
    pub fn record_start(
        &self,
        instance_id: &str,
        page_id: &str,
        dir: &Path,
        urls: &[String],
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            let connection = Arc::clone(inst.browser.connection());
            let page = inst.page_mut(page_id)?;
            if page.network.is_some() {
                return Err(
                    "invalid argument: record or replay is already active for this page".into(),
                );
            }
            page.network = Some(super::network::NetworkSession::record(
                Arc::clone(&page.main_frame),
                connection,
                dir,
                urls,
            )?);
            Ok(json!({"recording": true, "dir": dir}))
        })
    }

    /// Finalize a page's recording and atomically publish its cassette index.
    pub fn record_stop(
        &self,
        instance_id: &str,
        page_id: &str,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            let page = inst.page_mut(page_id)?;
            if !page.network.as_ref().is_some_and(|s| s.is_recording()) {
                return Err("invalid argument: recording is not active for this page".into());
            }
            page.network.take().unwrap().stop()
        })
    }

    /// Validate a cassette and enable offline replay for a page.
    pub fn replay_start(
        &self,
        instance_id: &str,
        page_id: &str,
        dir: &Path,
        match_body: bool,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            let connection = Arc::clone(inst.browser.connection());
            let page = inst.page_mut(page_id)?;
            if page.network.is_some() {
                return Err(
                    "invalid argument: record or replay is already active for this page".into(),
                );
            }
            page.network = Some(super::network::NetworkSession::replay(
                Arc::clone(&page.main_frame),
                connection,
                dir,
                match_body,
            )?);
            Ok(json!({"replaying": true, "dir": dir}))
        })
    }

    /// Disable interception and return the page's replay counts and misses.
    pub fn replay_stop(
        &self,
        instance_id: &str,
        page_id: &str,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            let page = inst.page_mut(page_id)?;
            if page.network.as_ref().is_none_or(|s| s.is_recording()) {
                return Err("invalid argument: replay is not active for this page".into());
            }
            // Stop accepting events and resolve every request already queued
            // before disabling: Juggler resumes pending requests on disable.
            let result = page.network.take().unwrap().stop();
            page.main_frame
                .set_request_interception(false)
                .map_err(|e| e.message)?;
            result
        })
    }

    /// Access the download log for an instance without locking the instance.
    pub fn download_log(
        &self,
        instance_id: &str,
    ) -> Option<Arc<crate::cli::downloads::DownloadLog>> {
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(instance_id)
            .map(|entry| Arc::clone(&entry.downloads))
    }

    /// Fill CLI page IDs (e.g. "p1") on download entries via a brief instance lock.
    pub fn populate_download_pages(
        &self,
        instance_id: &str,
        entries: &mut [crate::cli::downloads::DownloadEntry],
    ) {
        // try_lock: `downloads` must not wait behind a long-running command on
        // this instance. If the instance is busy, `page` stays None.
        let Some(managed) = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(instance_id)
            .map(|e| Arc::clone(&e.managed))
        else {
            return;
        };
        let guard = match managed.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        if let Some(inst) = &guard.instance {
            for entry in entries.iter_mut() {
                if entry.page.is_none() {
                    entry.page = inst.page_id_for_target(&entry.page_target_id);
                }
            }
        }
    }

    /// View or wait for downloads on an instance.
    ///
    /// The condvar wait runs outside the per-instance mutex so it never blocks
    /// other commands on this instance.
    pub fn downloads(
        &self,
        instance_id: &str,
        wait: Option<Duration>,
    ) -> Result<Vec<crate::cli::downloads::DownloadEntry>, String> {
        let entry = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(instance_id)
            .cloned()
            .ok_or_else(|| Self::missing(instance_id))?;
        let log = Arc::clone(&entry.downloads);

        let mut entries = match wait {
            Some(timeout) => {
                let seq = log.snapshot().0;
                let (_, entries) = log.wait_change(seq, timeout);
                entries
            }
            None => log.snapshot().1,
        };

        self.populate_download_pages(instance_id, &mut entries);
        Ok(entries)
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
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.text(page_id, selector, frames, timeout)
        })
    }

    /// Extract page HTML.
    pub fn html(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.html(page_id, selector, frames, timeout)
        })
    }

    /// Collect page links.
    pub fn links(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        frames: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.links(page_id, selector, frames, timeout)
        })
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
        self.with_instance(instance_id, |inst, _| {
            inst.data(page_id, og, jsonld, meta, timeout)
        })
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
        self.with_instance(instance_id, |inst, _| inst.url(page_id, timeout))
    }

    /// Go back one history entry.
    pub fn go_back(&self, instance_id: &str, page_id: &str) -> Result<bool, String> {
        self.with_instance(instance_id, |inst, _| inst.go_back(page_id))
    }

    /// Go forward one history entry.
    pub fn go_forward(&self, instance_id: &str, page_id: &str) -> Result<bool, String> {
        self.with_instance(instance_id, |inst, _| inst.go_forward(page_id))
    }

    /// Reload a page.
    pub fn reload(&self, instance_id: &str, page_id: &str) -> Result<(), String> {
        self.with_instance(instance_id, |inst, _| inst.reload(page_id))
    }

    /// Wait for a selector to appear.
    pub fn wait_for_selector(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<u128, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.wait_for_selector(page_id, selector, frames, timeout)
        })
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
        self.with_instance(instance_id, |inst, ctx| {
            let resolved_url = match (url, domain) {
                (Some(u), _) => Some(u.to_string()),
                (None, Some(_)) => None,
                (None, None) => {
                    let (page_url, _) = inst.url(page_id, timeout)?;
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
                .map_err(|e| format!("set_cookies failed: {}", e.message))?;

            Ok(json!({
                "cookie_set": name,
                "value": value,
                "url": resolved_url,
                "domain": domain,
            }))
        })
    }

    /// Add an extra HTTP request header to a page.
    pub fn set_header(
        &self,
        instance_id: &str,
        page_id: &str,
        name: &str,
        value: &str,
    ) -> Result<Vec<(String, String)>, String> {
        self.with_instance(instance_id, |inst, _| inst.set_header(page_id, name, value))
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
        frames: &[String],
        timeout: Duration,
    ) -> Result<(i32, i32, InteractionOutcome), String> {
        self.with_instance(instance_id, |inst, _| {
            inst.click_selector(page_id, selector, frames, timeout)
        })
    }

    /// Hover an element by CSS selector.
    pub fn hover(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(i32, i32), String> {
        self.with_instance(instance_id, |inst, _| {
            inst.hover(page_id, selector, frames, timeout)
        })
    }

    /// Fill an element by CSS selector.
    pub fn fill(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        value: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<String, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.fill(page_id, selector, value, frames, timeout)
        })
    }

    /// Set a file input's files and return their basenames in argument order.
    ///
    /// Paths must be absolute regular files. All paths are validated before
    /// accessing the page. Multiple files require the input's `multiple` attribute.
    pub fn upload(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        paths: &[String],
        frames: &[String],
        timeout: Duration,
    ) -> Result<Vec<String>, String> {
        let files = validate_upload_paths(paths)?;
        self.with_instance(instance_id, |inst, _| {
            inst.upload(page_id, selector, paths, frames, timeout)
        })?;
        Ok(files)
    }

    /// Insert text into the focused element.
    pub fn insert_text(
        &self,
        instance_id: &str,
        page_id: &str,
        text: &str,
        frames: &[String],
    ) -> Result<(), String> {
        self.insert_text_with_timeout(instance_id, page_id, text, frames, Duration::from_secs(30))
    }

    pub fn insert_text_with_timeout(
        &self,
        instance_id: &str,
        page_id: &str,
        text: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<(), String> {
        self.with_instance(instance_id, |inst, _| {
            inst.insert_text(page_id, text, frames, timeout)
        })
    }

    /// Press a named key.
    pub fn press(
        &self,
        instance_id: &str,
        page_id: &str,
        key: &str,
        frames: &[String],
    ) -> Result<InteractionOutcome, String> {
        self.press_with_timeout(instance_id, page_id, key, frames, Duration::from_secs(30))
    }

    pub fn press_with_timeout(
        &self,
        instance_id: &str,
        page_id: &str,
        key: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<InteractionOutcome, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.press(page_id, key, frames, timeout)
        })
    }

    /// Select an option in a `<select>`.
    pub fn select_option(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: &str,
        value: &str,
        frames: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.select_option(page_id, selector, value, frames, timeout)
        })
    }

    /// Scroll an element into view, or to the page bottom.
    pub fn scroll(
        &self,
        instance_id: &str,
        page_id: &str,
        selector: Option<&str>,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.with_instance(instance_id, |inst, _| {
            inst.scroll(page_id, selector, timeout)
        })
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
        self.with_instance(instance_id, |inst, _| Ok(inst.tabs(timeout)))
    }

    /// Accept or dismiss the pending dialog on a page.
    pub fn dialog(
        &self,
        instance_id: &str,
        page_id: &str,
        accept: bool,
        text: Option<&str>,
    ) -> Result<DialogInfo, String> {
        self.with_instance(instance_id, |inst, _| {
            let page = inst.page_any(page_id)?;
            let info = page
                .main_frame
                .pending_dialog()
                .ok_or_else(|| format!("no dialog is open on page {page_id}"))?;
            page.main_frame.handle_dialog(&info.id, accept, text);
            Ok(info)
        })
    }

    /// Close a page.
    pub fn close_tab(&self, instance_id: &str, page_id: &str) -> Result<(), String> {
        self.with_instance(instance_id, |inst, _| inst.close_tab(page_id))
    }

    /// Export all cookies for an instance's browser context (including HttpOnly).
    ///
    /// Calls `Browser.getCookies` on the root session with the instance's
    /// `browserContextId`. HttpOnly cookies are included — the Juggler protocol
    /// returns them in the same array as ordinary cookies.
    pub fn cookies(&self, instance_id: &str) -> Result<Vec<serde_json::Value>, String> {
        self.with_instance(instance_id, |_, ctx| {
            let cookies = ctx
                .get_cookies()
                .map_err(|e| format!("get_cookies failed: {}", e.message))?;
            let values: Vec<serde_json::Value> = cookies
                .iter()
                .map(|c| serde_json::to_value(c).expect("Cookie is always serializable"))
                .collect();
            Ok(values)
        })
    }

    /// Number of running instances.
    pub fn instance_count(&self) -> usize {
        self.instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub(crate) fn begin_shutdown(&self) {
        let _guard = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.closing.store(true, Ordering::Release);
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn close_all_connections(&self) {
        let closers: Vec<_> = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|e| Arc::clone(&e.closer))
            .collect();
        for closer in closers {
            let _ = closer.close();
        }
    }

    /// Shut down all instances in parallel.
    pub fn shutdown_all(&self) {
        let entries: Vec<_> = self
            .instances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        let workers: Vec<_> = entries
            .into_iter()
            .map(
                |InstanceEntry {
                     managed: entry,
                     closer,
                     ..
                 }| {
                    std::thread::spawn(move || {
                        let _ = closer.close();
                        let mut entry = entry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(instance) = entry.instance.take() {
                            let _ = instance.stop();
                        }
                    })
                },
            )
            .collect();
        for worker in workers {
            if worker.join().is_err() {
                log::warn!("instance shutdown worker panicked");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod phase_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_paths_accept_regular_files_and_symlinks_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("fixture one.txt");
        let second = dir.path().join("fixture-two.pdf");
        std::fs::write(&first, b"one").unwrap();
        std::os::unix::fs::symlink(&first, &second).unwrap();
        let paths = vec![first.display().to_string(), second.display().to_string()];
        assert_eq!(
            validate_upload_paths(&paths).unwrap(),
            ["fixture one.txt", "fixture-two.pdf"]
        );
    }

    #[test]
    fn upload_paths_reject_empty_relative_missing_directory_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let cases = [
            vec![],
            vec!["relative.txt".into()],
            vec![dir.path().join("missing.txt").display().to_string()],
            vec![dir.path().display().to_string()],
            vec![socket.display().to_string()],
        ];
        for paths in cases {
            // Validation must happen before even looking up the instance/page.
            let error = InstanceManager::new()
                .upload(
                    "absent",
                    "absent",
                    "input",
                    &paths,
                    &[],
                    Duration::from_secs(1),
                )
                .unwrap_err();
            assert!(error.starts_with("invalid argument: "), "{error}");
        }
    }

    #[test]
    fn resolve_chain_shares_budget_across_delayed_hops() {
        let mut budgets = Vec::new();
        let result = resolve_frame_chain(
            "main".into(),
            &["outer".into(), "inner".into()],
            Duration::from_secs(1),
            |_, sel, budget| {
                budgets.push(budget);
                let delay = Duration::from_millis(650);
                std::thread::sleep(delay.min(budget));
                if budget < delay {
                    Err(ProtocolError::timeout("resolve", budget))
                } else {
                    Ok(sel.into())
                }
            },
        );
        assert!(
            result.is_err(),
            "each hop must not receive a fresh full timeout"
        );
        assert_eq!(budgets.len(), 2);
        assert!(budgets[1] < Duration::from_millis(500), "{budgets:?}");
    }

    #[test]
    fn wait_accepts_frame_resolution_slower_than_quarter_second() {
        let result = poll_selector(
            |budget| {
                resolve_frame_chain(
                    "main".into(),
                    &["outer".into(), "inner".into()],
                    budget,
                    |_, sel, remaining| {
                        let delay = Duration::from_millis(180);
                        std::thread::sleep(delay.min(remaining));
                        if remaining < delay {
                            Err(ProtocolError::timeout("resolve", remaining))
                        } else {
                            Ok(sel.into())
                        }
                    },
                )?;
                Ok(true)
            },
            "#ready",
            Duration::from_secs(1),
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn wait_deadline_normalizes_a_final_protocol_timeout() {
        let err = poll_selector(
            |budget| {
                std::thread::sleep(budget);
                Err(ProtocolError::timeout("Runtime.evaluate", budget))
            },
            "#never",
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(err, "timed out after 0s waiting for selector: #never");
    }

    #[test]
    fn satisfied_or_terminal_poll_wins_at_deadline() {
        let duration = Duration::from_millis(10);
        assert!(poll_selector(
            |budget| {
                std::thread::sleep(budget);
                Ok(true)
            },
            "#ready",
            duration
        )
        .is_ok());
        for kind in [
            ProtocolErrorKind::Closed,
            ProtocolErrorKind::Crashed,
            ProtocolErrorKind::DialogOpened,
        ] {
            let error = poll_selector(
                |budget| {
                    std::thread::sleep(budget);
                    Err(ProtocolError::new(
                        kind,
                        Some("page".into()),
                        "terminal page failure",
                    ))
                },
                "#ready",
                duration,
            )
            .unwrap_err();
            assert_eq!(error, "terminal page failure", "{kind:?}");
        }
    }

    #[test]
    fn launch_options_default_context_is_unchanged() {
        let opts = LaunchOptions::default();
        assert!(opts.headless.is_none());
        assert!(!opts.context_options().bypass_csp);
        assert!(opts.context_options().download_options.is_none());
        assert!(
            LaunchOptions {
                bypass_csp: true,
                ..Default::default()
            }
            .context_options()
            .bypass_csp
        );
    }

    #[test]
    fn screenshot_runtime_path_and_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = default_screenshot_path(dir.path(), "00000001", "p1", None);
        assert_eq!(path, dir.path().join("screenshot-00000001-p1.png"));
        assert_ne!(
            path,
            default_screenshot_path(dir.path(), "00000002", "p1", None)
        );
        assert_eq!(
            default_screenshot_path(&crate::cli::socket::runtime_dir(), "i", "p1", Some("jpeg"))
                .parent()
                .unwrap(),
            crate::cli::socket::runtime_dir()
        );
        write_screenshot(&path, b"png").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_screenshot(&path, b"new").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn manager_recovers_poisoned_map() {
        let manager = InstanceManager::new();
        let _ = std::panic::catch_unwind(|| {
            let _guard = manager.instances.lock().unwrap();
            panic!("test poison");
        });
        assert_eq!(manager.instance_count(), 0);
        assert!(manager.list().is_empty());
        assert!(manager
            .new_page("missing")
            .unwrap_err()
            .contains("not found"));
        manager.shutdown_all();
    }

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

    /// A browser install directory holding a Camoufox `application.ini` for
    /// `version`, plus the path its executable would have.
    fn fake_browser_install(version: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ini = format!(
            "[App]\nVendor=Mozilla\nName=Firefox\nCodeName=Camoufox\nVersion={version}\n\
             BuildID=20260831224046\n\n[Gecko]\nMinVersion=1.0\nMaxVersion=1.0\n"
        );
        std::fs::write(dir.path().join("application.ini"), ini).expect("write application.ini");
        let exe = dir.path().join("camoufox");
        (dir, exe)
    }

    fn configured_user_agent(env: &HashMap<String, String>) -> Option<String> {
        let json = env.get("CAMOU_CONFIG_1")?;
        let config: serde_json::Value = serde_json::from_str(json).expect("CAMOU_CONFIG_1 is JSON");
        config["navigator.userAgent"].as_str().map(str::to_owned)
    }

    /// Camoufox 150+ launched without a fingerprint config sends
    /// `Camoufox/<version>` in its User-Agent. Launch pins a plain Firefox UA
    /// whose version matches the browser actually being started.
    #[cfg(target_os = "linux")]
    #[test]
    fn user_agent_env_pins_firefox_user_agent_for_browser_version() {
        let cases = [
            (
                "152.0.4-beta.30",
                "Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0",
            ),
            (
                "135.0.1-beta.24",
                "Mozilla/5.0 (X11; Linux x86_64; rv:135.0) Gecko/20100101 Firefox/135.0",
            ),
        ];
        for (version, want) in cases {
            let (_dir, exe) = fake_browser_install(version);
            let env = user_agent_env(&exe, &[]);
            assert_eq!(
                configured_user_agent(&env).as_deref(),
                Some(want),
                "browser {version}"
            );
        }
    }

    /// A `CAMOU_CONFIG*` the user already exported is their fingerprint;
    /// launch must not override it with the default UA.
    #[test]
    fn user_agent_env_leaves_user_supplied_camou_config_alone() {
        let (_dir, exe) = fake_browser_install("152.0.4-beta.30");
        let inherited = vec!["PATH".to_string(), "CAMOU_CONFIG_1".to_string()];
        assert!(user_agent_env(&exe, &inherited).is_empty());
    }

    /// Without a readable `application.ini` the browser version is unknown,
    /// so no UA is guessed.
    #[test]
    fn user_agent_env_sets_nothing_when_browser_version_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(user_agent_env(&dir.path().join("camoufox"), &[]).is_empty());
    }

    #[test]
    fn downloads_dir_relative_rejected() {
        let mgr = InstanceManager::new();
        let res = mgr.launch_with(&LaunchOptions {
            downloads_dir: Some("rel/dir".into()),
            ..Default::default()
        });
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .contains("--downloads-dir must be an absolute path"));
    }
    #[test]
    fn s6_crashed_tab_is_listed_and_close_succeeds_best_effort() {
        use crate::protocol::RawMessage;
        use crate::transport::errors::TransportError;
        use crate::transport::{Transport, TransportReader, TransportWriter};
        use std::sync::mpsc;

        struct Writer {
            incoming: Option<mpsc::Sender<RawMessage>>,
            records: Arc<Mutex<Vec<serde_json::Value>>>,
        }
        impl TransportWriter for Writer {
            fn send(&mut self, value: &serde_json::Value) -> Result<(), TransportError> {
                self.records.lock().unwrap().push(value.clone());
                // Bootstrap responses; crashed-page close deliberately gets none.
                if value["method"] != "Page.close" {
                    self.incoming
                        .as_ref()
                        .unwrap()
                        .send(RawMessage {
                            id: value["id"].as_i64(),
                            result: Some(json!({})),
                            method: None,
                            params: None,
                            error: None,
                            session_id: None,
                        })
                        .unwrap();
                }
                Ok(())
            }
            fn close(&mut self) -> Result<(), TransportError> {
                self.incoming.take();
                Ok(())
            }
            fn is_closed(&self) -> bool {
                self.incoming.is_none()
            }
        }
        struct Reader(mpsc::Receiver<RawMessage>);
        impl TransportReader for Reader {
            fn receive(&mut self) -> Result<RawMessage, TransportError> {
                self.0.recv().map_err(|_| TransportError::Closed)
            }
            fn is_closed(&self) -> bool {
                false
            }
        }
        struct MockTransport(Writer, Reader);
        impl Transport for MockTransport {
            fn into_split(self: Box<Self>) -> (Box<dyn TransportWriter>, Box<dyn TransportReader>) {
                (Box::new(self.0), Box::new(self.1))
            }
        }
        let (tx, rx) = mpsc::channel();
        let records = Arc::new(Mutex::new(Vec::new()));
        let connection = Connection::new(Box::new(MockTransport(
            Writer {
                incoming: Some(tx.clone()),
                records: records.clone(),
            },
            Reader(rx),
        )));
        let root = connection.root_session();
        let browser = Browser::connect(connection, root, BrowserOptions::default()).unwrap();
        let conn = browser.connection().clone();
        let context =
            crate::api::BrowserContext::new("ctx".into(), &conn.root_session(), conn.clone());
        tx.send(RawMessage { id: None, method: Some("Browser.attachedToTarget".into()), params: Some(json!({ "sessionId": "s", "targetInfo": { "type": "page", "targetId": "t", "browserContextId": "ctx" } })), result: None, error: None, session_id: None }).unwrap();
        tx.send(RawMessage {
            id: None,
            method: Some("Page.frameAttached".into()),
            params: Some(json!({ "frameId": "f" })),
            result: None,
            error: None,
            session_id: Some("s".into()),
        })
        .unwrap();
        let page = context
            .registry()
            .claim("t", Duration::from_secs(2))
            .unwrap();
        let main_frame =
            MainFrame::from_tracked(page, conn.clone(), context.registry().clone()).unwrap();
        conn.create_session("s".into()).mark_crashed();
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let mut instance = Instance {
            instance_id: "test".into(),
            browser,
            child,
            version: None,
            pid: 0,
            profile_dir: ProfileDir::Temp(tempfile::tempdir().unwrap()),
            profile_reservation: None,
            profile_dir_path: None,
            pages: HashMap::from([(
                "p1".into(),
                ManagedMainFrame {
                    main_frame: Arc::new(main_frame),
                    network: None,
                    headers: Vec::new(),
                },
            )]),
            closed_pages: HashSet::new(),
            closed_pages_order: VecDeque::new(),
            page_counter: 1,
            close_timeout: Duration::from_millis(100),
            registry: context.registry().clone(),
        };
        instance.adopt_popups(&context);
        assert_eq!(instance.page_ids(), vec!["p1"]);
        assert_eq!(instance.page_any("p1").err().unwrap(), "page p1 crashed");
        let tabs = instance.tabs(Duration::from_millis(100));
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0]["crashed"], true);
        assert!(tabs[0]["url"].is_null());
        assert!(instance.close_tab("p1").is_ok());
        assert!(instance.page_ids().is_empty());
        assert_eq!(instance.page_any("p1").err().unwrap(), "page p1 was closed");
        assert!(records
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["method"] == "Page.close" && v["sessionId"] == "s"));
        drop(instance);
        drop(context);
        drop(tx);
        drop(conn);
    }

    #[test]
    fn prepare_profile_dir_rejects_relative_path() {
        let err = prepare_profile_dir("relative/path").unwrap_err();
        assert_eq!(
            err,
            "invalid argument: --profile-dir must be an absolute path"
        );
    }

    #[test]
    fn prepare_profile_dir_creates_missing_dir_with_mode_0700() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("missing_profile_subdir");
        assert!(!target.exists());

        let canonical = prepare_profile_dir(target.to_str().unwrap()).unwrap();
        assert!(canonical.exists());
        assert!(canonical.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&canonical).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "created directory permissions must be 0700");
        }
    }

    #[test]
    fn prepare_profile_dir_tightens_looser_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("loose_profile");
        std::fs::create_dir_all(&target).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o777)).unwrap();
            let meta = std::fs::metadata(&target).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o777);

            let canonical = prepare_profile_dir(target.to_str().unwrap()).unwrap();
            let meta = std::fs::metadata(&canonical).unwrap();
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o700,
                "looser permissions must be tightened to 0700"
            );
        }

        #[cfg(not(unix))]
        {
            let canonical = prepare_profile_dir(target.to_str().unwrap()).unwrap();
            assert!(canonical.is_dir());
        }
    }

    #[test]
    fn prepare_profile_dir_canonicalizes_aliases_to_one_path() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("profile");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("profile-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let dotted = format!("{}/./sub/../", real.display());
        std::fs::create_dir(real.join("sub")).unwrap();

        let canonical = prepare_profile_dir(real.to_str().unwrap()).unwrap();
        assert_eq!(canonical, real.canonicalize().unwrap());
        // Collision detection compares canonical paths, so every alias must
        // resolve to the same value.
        assert_eq!(
            prepare_profile_dir(link.to_str().unwrap()).unwrap(),
            canonical
        );
        assert_eq!(prepare_profile_dir(&dotted).unwrap(), canonical);
        let mode = std::fs::metadata(&canonical).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "a private directory is left unchanged");
    }

    #[test]
    fn prepare_profile_dir_creates_missing_parents_and_tightens_group_access() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("a").join("b").join("profile");
        let canonical = prepare_profile_dir(nested.to_str().unwrap()).unwrap();
        assert!(canonical.is_dir());
        let group = temp.path().join("group");
        std::fs::create_dir(&group).unwrap();
        std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o750)).unwrap();
        prepare_profile_dir(group.to_str().unwrap()).unwrap();
        let mode = std::fs::metadata(&group).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn launch_profile_dir_errors_precede_browser_spawn() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("fixture.pdf");
        std::fs::write(&file, b"contents").unwrap();
        let manager = InstanceManager::new();
        // A nonexistent executable proves the profile error is returned
        // before any browser process is spawned.
        let launch = |profile_dir: &str| {
            manager
                .launch_with(&LaunchOptions {
                    executable: Some("/nonexistent/camoufox-test-binary".into()),
                    profile_dir: Some(profile_dir.into()),
                    ..Default::default()
                })
                .unwrap_err()
        };
        assert_eq!(
            launch("relative/profile"),
            "invalid argument: --profile-dir must be an absolute path"
        );
        let error = launch(file.to_str().unwrap());
        assert!(error.ends_with("is not a directory"), "{error}");
        assert_eq!(
            crate::cli::ipc::DaemonResponse::err(error).kind.as_deref(),
            Some("invalid_argument")
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"contents");
        // A valid directory gets past validation and fails at spawn instead.
        let error = launch(temp.path().join("profile").to_str().unwrap());
        assert!(error.starts_with("failed to spawn browser"), "{error}");
        assert!(temp.path().join("profile").is_dir());
        assert!(manager.list().is_empty());
        // Shutdown takes precedence over profile validation.
        manager.begin_shutdown();
        assert_eq!(launch("relative/profile"), "daemon shutting down");
    }

    #[test]
    fn cassette_commands_report_missing_instance_and_validate_nothing_else() {
        let manager = InstanceManager::new();
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cassette");
        let results = [
            manager.record_start("absent", "p1", &dir, &[]),
            manager.record_stop("absent", "p1"),
            manager.replay_start("absent", "p1", &dir, false),
            manager.replay_stop("absent", "p1"),
        ];
        for result in results {
            let error = result.unwrap_err();
            assert_eq!(error, "instance absent not found");
            assert_eq!(
                crate::cli::ipc::DaemonResponse::err(error).kind.as_deref(),
                Some("instance_not_found")
            );
        }
        assert!(
            !dir.exists(),
            "a missing instance must not create a cassette directory"
        );
    }

    #[test]
    fn prepare_profile_dir_rejects_existing_non_directory() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("some_file");
        std::fs::write(&file_path, "not a directory").unwrap();

        let err = prepare_profile_dir(file_path.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("is not a directory"),
            "expected non-directory error, got: {err}"
        );
    }
}
