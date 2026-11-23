# Rust Library API Reference: camoufox

Reference for the public modules and types exported by the `camoufox` crate.

---

## Crate Layout & Module Overview

```text
camoufox/
├── api/             # High-level browser automation (Browser, BrowserContext, MainFrame)
├── codec/           # Wire encoding/decoding (NUL-delimited JSON RPC frames)
├── config/          # Launch arguments, profile layout, environment variables
├── obs/             # Observability, protocol tracing, structured logging
├── process/         # Unix process management (spawn, pipe setup, readiness, lifecycle)
├── protocol/        # Juggler protocol types, error kinds, event demuxing, session state
├── transport/       # Reader/writer transport abstractions
└── cli/             # Daemon and client CLI infrastructure (feature = "cli")
```

---

## 1. `camoufox::api`

High-level browser automation layer.

### `Browser`

Represents a running browser connection attached to the root Juggler session.

```rust,no_run
# fn doc_test(conn: camoufox::protocol::client::Connection, root: camoufox::protocol::client::Session) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::browser::{Browser, BrowserOptions};
use camoufox::api::context::ContextOptions;

let browser = Browser::connect(conn, root, BrowserOptions::default())?;
let context = browser.new_context(ContextOptions::default())?;
browser.close()?;
# Ok(())
# }
```

Key methods:
- `Browser::connect(connection, root_session, options)`: Connect to an established Juggler connection.
- `browser.new_context(options)`: Create a new isolated profile context.
- `browser.default_context(options)`: Configure and return the `BrowserContext` for the browser's persistent on-disk profile. Requires connecting with `BrowserOptions::attach_to_default_context: true` to receive page and popup events. Unlike private contexts, calling `close()` on the default context is a no-op; call `Browser::close()` to terminate the process and flush profile storage.
- `browser.version()`: Returns the browser version string if queried.
- `browser.user_agent()`: Returns the browser User-Agent string.
- `browser.close()`: Consumes the handle and gracefully terminates the browser.

### `BrowserContext`

Represents an isolated browsing context (profile) with its own cookies and storage.

```rust,no_run
# fn doc_test(context: &camoufox::api::context::BrowserContext) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::context::CookieOptions;

// Open a new page pinned to its top frame
let page = context.new_main_frame()?;

// Inspect cookies
let cookies = context.get_cookies()?;

// Set cookies
let cookie = CookieOptions {
    name: "session".into(),
    value: "abc".into(),
    url: None,
    domain: Some(".example.com".into()),
    path: Some("/".into()),
    secure: None,
    http_only: None,
    same_site: None,
    expires: None,
};
context.set_cookies(&[cookie])?;

// Clear cookies
context.clear_cookies()?;

// Retrieve newly opened popups adopted by the registry
let popups = context.take_popups();
# Ok(())
# }
```

Key methods:
- `context.context_id()`: Returns the context's unique ID string. For the default context, this is empty (`""`).
- `context.new_main_frame()`: Creates a new tab/page and returns a `MainFrame` handle pinned to the top frame.
- `context.get_cookies()`: Retrieves all cookies stored in the context.
- `context.set_cookies(cookies)`: Sets one or more cookies.
- `context.clear_cookies()`: Clears all cookies from the context.
- `context.take_popups()`: Returns and drains any popup pages adopted by the context registry.
- `context.close()`: Closes and removes the browser context. For the default context, this is a no-op (`Ok(())`); use `Browser::close()` instead.

### `MainFrame`

Represents an open page handle structurally pinned to the top frame.

```rust,no_run
# fn doc_test(page: &camoufox::api::main_frame::MainFrame) -> Result<(), Box<dyn std::error::Error>> {
use camoufox::api::dialog::DialogPolicy;
use camoufox::api::main_frame::{NavigateOptions, NavigateOutcome};
use std::time::Duration;

let timeout = Duration::from_secs(10);

// Navigate
let outcome: NavigateOutcome = page.navigate("https://example.com", NavigateOptions::default(), timeout)?;
let _status = outcome.status_code;

// JavaScript evaluation
let raw_title: serde_json::Value = page.evaluate("document.title", timeout)?;
let main_world_val: serde_json::Value = page.evaluate_main_world("window.location.href", timeout)?;

// Dialog handling
page.set_dialog_policy(DialogPolicy::Manual);
if let Some(d) = page.pending_dialog() {
    page.handle_dialog(&d.id, true, None);
}

// History & reload
page.reload()?;
page.go_back()?;
page.go_forward()?;

// Frame inspection
let top_id = page.frame_id();
let frames = page.frames();

// Close page
page.close()?;
# Ok(())
# }
```

Key methods:
- `page.navigate(url, options, timeout)`: Navigates to a URL. Returns `Result<camoufox::api::main_frame::NavigateOutcome, ProtocolError>` containing `nav_id: Option<String>` and `status_code: Option<u16>`.
- `page.evaluate(expr, timeout)`: Evaluates JavaScript in the isolated world. Returns raw `Result<serde_json::Value, ProtocolError>` matching the Juggler `Runtime.evaluate` response (value is under `val["result"]["value"]`). Page exceptions return `Ok` containing an `exceptionDetails` object.
- `page.evaluate_main_world(expr, timeout)`: Evaluates JavaScript in the main page world. Unwraps and returns the evaluated value directly as `Result<serde_json::Value, ProtocolError>`.
- `page.evaluate_in_frame(frame_id, expr, timeout)`: Evaluates JavaScript scoped to a child frame. Returns raw `Result<serde_json::Value, ProtocolError>` matching `evaluate`.
- `page.resolve_child_frame(parent_id, selector, timeout)`: Resolves a child frame ID from a CSS selector.
- `page.set_file_input_files(frame_id, object_id, files)`: Sets files on a file input element with a default 60-second timeout. Dispatches `input` and `change` events.
- `page.set_file_input_files_with_timeout(frame_id, object_id, files, timeout)`: Sets files on a file input element within an explicit command timeout. Dispatches `input` and `change` events.
- `page.insert_text(text)`: Inserts text into the currently focused element.
- `page.dispatch_key_event(params)`: Dispatches a raw keyboard event.
- `page.dispatch_mouse_event(params)`: Dispatches a raw mouse event.
- `page.dispatch_wheel_event(params)`: Dispatches a raw wheel scroll event.
- `page.screenshot(options)`: Captures a PNG or JPEG screenshot.
- `page.pending_dialog()`: Returns the currently pending dialog, if any.
- `page.handle_dialog(dialog_id, accept, prompt_text)`: Responds to a dialog.
- `page.set_dialog_policy(policy)`: Sets dialog handling policy (`Manual`, `Accept`, `Dismiss`).
- `page.frames()`: Returns tracked frame IDs and their parent frame IDs.
- `page.is_closed()`: Returns true if the page session was closed or detached.
- `page.close()`: Closes the page target.

---

## 2. `camoufox::config`

Process and launch configuration.

### `LaunchConfig`

Configuration struct passed to `process::unix::spawn`:

```rust,no_run
use camoufox::config::LaunchConfig;
use std::path::PathBuf;

let config = LaunchConfig {
    executable: PathBuf::from("/path/to/camoufox"),
    profile_dir: Some(PathBuf::from("/tmp/profile")),
    headless: true,
    args: vec![],
    env: std::collections::HashMap::new(),
    persistent: false,
    timeout: std::time::Duration::from_secs(180),
    close_timeout: std::time::Duration::from_secs(30),
};
```

*Note:* `profile_dir` must be `Some(...)`. `LaunchConfig::validate()` returns `Err(ConfigError::MissingProfile)` when it is `None`; `process::unix::spawn` runs that validation and surfaces it as a `ProcessError` wrapping an `io::Error` with the message "profile_dir must be set before launch".

### `default_executable()`

```rust,no_run
let exe: std::path::PathBuf = camoufox::config::default_executable();
```

Where the CLI and the integration tests find the browser when no path is given:

1. `$CAMOUFOX_BIN`, if set and non-empty.
2. The install marked active in `<cache>/config.json` (`active_version`).
3. The newest `<cache>/browsers/<repo>/<version>/camoufox`.
4. `<cache>/camoufox` (legacy layout), returned even if missing.

`<cache>` is `$XDG_CACHE_HOME/camoufox` when that is an absolute path, else `$HOME/.cache/camoufox`. This matches where the Python `camoufox fetch` installs on Linux; on macOS, set `CAMOUFOX_BIN`.

---

## 3. `camoufox::process` (Unix Only)

Unix process spawning, readiness detection, and lifecycle management.

- `process::unix::spawn(config)`: Launches the browser process with anonymous pipes on fd 3 and fd 4. Returns a `LaunchedProcess` containing `child`, `command_pipe`, and `response_pipe`.
- `process::readiness::wait_for_ready(child, timeout)`: Watches stdout and stderr concurrently for the Juggler readiness banner.

---

## 4. `camoufox::protocol`

Low-level Juggler protocol types, error handling, and session state.

### `ProtocolErrorKind`

Non-exhaustive enum classifying protocol error variants:
- `Response`: Server returned a protocol error object.
- `FrameDetached`: Scoped frame was detached or navigated away.
- `ScriptException`: JavaScript evaluation threw an exception.
- `NotIframe`: Matched element is not an iframe.
- `FrameNotFound`: Selector did not match any iframe.
- `ShadowRoot`: Element has no open shadow root.
- `Closed`: Target or connection was closed.
- `Crashed`: Renderer process crashed.
- `DialogOpened`: Interrupted by an unhandled JavaScript dialog.
- `Transport`: Pipe or socket I/O error.
- `Timeout`: Request deadline expired.
- `NavigationBecameDownload`: Navigation diverted into a file download.

### `DownloadInfo`

Structured metadata attached to `ProtocolError` when kind is `NavigationBecameDownload`:
- `url`: String
- `frame_id`: Option of String
- `download_id`: Option of String

---

## 5. `camoufox::obs`

Observability and structured protocol logging.

- `ProtocolLogger`: Unit struct (`pub struct ProtocolLogger;`) and logging helpers for tracing incoming and outgoing protocol traffic at `TRACE` level.
